#[cfg(test)]
mod conn_test;

use crate::alert::*;
use crate::application_data::*;
use crate::cipher_suite::*;
use crate::content::*;
use crate::curve::named_curve::NamedCurve;
use crate::extension::extension_use_srtp::*;
use crate::flight::flight0::*;
use crate::flight::flight1::*;
use crate::flight::flight5::*;
use crate::flight::flight6::*;
use crate::flight::*;
use crate::fragment_buffer::*;
use crate::handshake::handshake_cache::*;
use crate::handshake::handshake_header::HandshakeHeader;
use crate::handshake::*;
use crate::handshaker::*;
use crate::record_layer::record_layer_header::*;
use crate::record_layer::*;
use crate::state::*;
use std::collections::VecDeque;

use shared::{error::*, replay_detector::*};

use bytes::BytesMut;
use log::*;
use std::io::{BufReader, BufWriter};
use std::marker::{Send, Sync};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

pub(crate) const INITIAL_TICKER_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const COOKIE_LENGTH: usize = 20;
pub(crate) const DEFAULT_NAMED_CURVE: NamedCurve = NamedCurve::X25519;
pub(crate) const INBOUND_BUFFER_SIZE: usize = 8192;
// Default replay protection window is specified by RFC 6347 Section 4.1.2.6
pub(crate) const DEFAULT_REPLAY_PROTECTION_WINDOW: usize = 64;

pub static INVALID_KEYING_LABELS: &[&str] = &[
    "client finished",
    "server finished",
    "master secret",
    "key expansion",
];

type PacketSendRequest = (Vec<Packet>, Option<mpsc::Sender<Result<()>>>);

struct ConnReaderContext {}

// Conn represents a DTLS connection
pub struct DTLSConn {
    is_client: bool,
    replay_protection_window: usize,
    replay_detector: Vec<Box<dyn ReplayDetector + Send>>,
    incoming_decrypted_packets: VecDeque<BytesMut>, // Decrypted Application Data or error, pull by calling `Read`
    incoming_encrypted_packets: VecDeque<Vec<u8>>,
    fragment_buffer: FragmentBuffer,
    pub(crate) cache: HandshakeCache, // caching of handshake messages for verifyData generation
    pub(crate) outgoing_packets: VecDeque<Packet>,

    pub(crate) state: State, // Internal state

    handshake_completed: bool,
    connection_closed_by_user: bool,
    // closeLock              sync.Mutex
    closed: AtomicBool, //  *closer.Closer
    //handshakeLoopsFinished sync.WaitGroup

    //readDeadline  :deadline.Deadline,
    //writeDeadline :deadline.Deadline,

    //log logging.LeveledLogger
    /*
    reading               chan struct{}
    handshakeRecv         chan chan struct{}
    cancelHandshaker      func()
    cancelHandshakeReader func()
    */
    pub(crate) current_handshake_state: HandshakeState,
    pub(crate) current_retransmit_timer: Option<Instant>,

    pub(crate) current_flight: Box<dyn Flight + Send + Sync>,
    pub(crate) flights: Option<Vec<Packet>>,
    pub(crate) cfg: HandshakeConfig,
    pub(crate) retransmit: bool,
    pub(crate) handshake_rx: Option<()>,

    pub(crate) handle_queue_tx: mpsc::Sender<mpsc::Sender<()>>,
    pub(crate) handshake_done_tx: Option<mpsc::Sender<()>>,
    reader_close_tx: Mutex<Option<mpsc::Sender<()>>>,
}

impl DTLSConn {
    pub fn new(
        handshake_config: HandshakeConfig,
        is_client: bool,
        initial_state: Option<State>,
    ) -> Self {
        let (state, flight, initial_fsm_state) = if let Some(state) = initial_state {
            let flight = if is_client {
                Box::new(Flight5 {}) as Box<dyn Flight + Send + Sync>
            } else {
                Box::new(Flight6 {}) as Box<dyn Flight + Send + Sync>
            };

            (state, flight, HandshakeState::Finished)
        } else {
            let flight = if is_client {
                Box::new(Flight1 {}) as Box<dyn Flight + Send + Sync>
            } else {
                Box::new(Flight0 {}) as Box<dyn Flight + Send + Sync>
            };

            (
                State {
                    is_client,
                    ..Default::default()
                },
                flight,
                HandshakeState::Preparing,
            )
        };

        let (handshake_done_tx, _handshake_done_rx) = mpsc::channel(1);
        let (handle_queue_tx, _handle_queue_rx) = mpsc::channel(1);
        let (reader_close_tx, _reader_close_rx) = mpsc::channel(1);
        let cache = HandshakeCache::new();

        Self {
            is_client,
            replay_protection_window: handshake_config.replay_protection_window,
            replay_detector: vec![],
            incoming_decrypted_packets: VecDeque::new(),
            incoming_encrypted_packets: VecDeque::new(),
            fragment_buffer: FragmentBuffer::new(),

            cache,
            state,
            handshake_completed: false,
            connection_closed_by_user: false,
            closed: AtomicBool::new(false),

            current_handshake_state: initial_fsm_state,
            current_retransmit_timer: None,

            current_flight: flight,
            flights: None,
            cfg: handshake_config,
            retransmit: false,
            handshake_rx: None,
            outgoing_packets: VecDeque::new(),
            handle_queue_tx,
            handshake_done_tx: Some(handshake_done_tx),
            reader_close_tx: Mutex::new(Some(reader_close_tx)),
        }
        /*TODO:
                tokio::spawn(async move {
                    loop {
                        let rx = packet_rx.recv().await;
                        if let Some(r) = rx {
                            let (pkt, result_tx) = r;

                            let result = DTLSConn::handle_outgoing_packets(
                                &next_conn_tx,
                                pkt,
                                &mut cache1,
                                is_client,
                                &sequence_number,
                                &cipher_suite1,
                                maximum_transmission_unit,
                            )
                            .await;

                            if let Some(tx) = result_tx {
                                let _ = tx.send(result).await;
                            }
                        } else {
                            trace!("{}: handle_outgoing_packets exit", srv_cli_str(is_client));
                            break;
                        }
                    }
                });
        */
    }

    // Read reads data from the connection.
    pub fn read(&mut self) -> Option<BytesMut> {
        if !self.is_handshake_completed() {
            None
        } else {
            self.incoming_decrypted_packets.pop_front()
        }
    }

    // Write writes len(p) bytes from p to the DTLS connection
    pub fn write(&mut self, p: &[u8]) -> Result<usize> {
        if self.is_connection_closed() {
            return Err(Error::ErrConnClosed);
        }

        if !self.is_handshake_completed() {
            return Err(Error::ErrHandshakeInProgress);
        }

        let pkts = vec![Packet {
            record: RecordLayer::new(
                PROTOCOL_VERSION1_2,
                self.get_local_epoch(),
                Content::ApplicationData(ApplicationData {
                    data: BytesMut::from(p),
                }),
            ),
            should_encrypt: true,
            reset_local_sequence_number: false,
        }];

        self.write_packets(pkts);

        Ok(p.len())
    }

    // Close closes the connection.
    pub async fn close(&mut self) -> Result<()> {
        if !self.closed.load(Ordering::SeqCst) {
            self.closed.store(true, Ordering::SeqCst);

            // Discard error from notify() to return non-error on the first user call of Close()
            // even if the underlying connection is already closed.
            self.notify(AlertLevel::Warning, AlertDescription::CloseNotify);

            {
                let mut reader_close_tx = self.reader_close_tx.lock().await;
                reader_close_tx.take();
            }
            /*TODO: self.conn
            .close()
            .await
            .map_err(|err| Error::Other(err.to_string()))?;*/
        }

        Ok(())
    }

    /// connection_state returns basic DTLS details about the connection.
    /// Note that this replaced the `Export` function of v1.
    pub async fn connection_state(&self) -> State {
        self.state.clone().await
    }

    /// selected_srtpprotection_profile returns the selected SRTPProtectionProfile
    pub fn selected_srtpprotection_profile(&self) -> SrtpProtectionProfile {
        self.state.srtp_protection_profile
    }

    pub(crate) fn notify(&mut self, level: AlertLevel, desc: AlertDescription) {
        self.write_packets(vec![Packet {
            record: RecordLayer::new(
                PROTOCOL_VERSION1_2,
                self.get_local_epoch(),
                Content::Alert(Alert {
                    alert_level: level,
                    alert_description: desc,
                }),
            ),
            should_encrypt: self.is_handshake_completed(),
            reset_local_sequence_number: false,
        }]);
    }

    pub(crate) fn write_packets(&mut self, pkts: Vec<Packet>) {
        for pkt in pkts {
            self.outgoing_packets.push_back(pkt);
        }
    }

    async fn handle_outgoing_packets(
        next_conn: &Arc<dyn util::Conn + Send + Sync>,
        mut pkts: Vec<Packet>,
        cache: &mut HandshakeCache,
        is_client: bool,
        local_sequence_number: &Arc<Mutex<Vec<u64>>>,
        cipher_suite: &Arc<std::sync::Mutex<Option<Box<dyn CipherSuite + Send + Sync>>>>,
        maximum_transmission_unit: usize,
    ) -> Result<()> {
        let mut raw_packets = vec![];
        for p in &mut pkts {
            if let Content::Handshake(h) = &p.record.content {
                let mut handshake_raw = vec![];
                {
                    let mut writer = BufWriter::<&mut Vec<u8>>::new(handshake_raw.as_mut());
                    p.record.marshal(&mut writer)?;
                }
                trace!(
                    "Send [handshake:{}] -> {} (epoch: {}, seq: {})",
                    srv_cli_str(is_client),
                    h.handshake_header.handshake_type.to_string(),
                    p.record.record_layer_header.epoch,
                    h.handshake_header.message_sequence
                );
                cache.push(
                    handshake_raw[RECORD_LAYER_HEADER_SIZE..].to_vec(),
                    p.record.record_layer_header.epoch,
                    h.handshake_header.message_sequence,
                    h.handshake_header.handshake_type,
                    is_client,
                );

                let raw_handshake_packets = DTLSConn::process_handshake_packet(
                    local_sequence_number,
                    cipher_suite,
                    maximum_transmission_unit,
                    p,
                    h,
                )
                .await?;
                raw_packets.extend_from_slice(&raw_handshake_packets);
            } else {
                /*if let Content::Alert(a) = &p.record.content {
                    if a.alert_description == AlertDescription::CloseNotify {
                        closed = true;
                    }
                }*/

                let raw_packet =
                    DTLSConn::process_packet(local_sequence_number, cipher_suite, p).await?;
                raw_packets.push(raw_packet);
            }
        }

        if !raw_packets.is_empty() {
            let compacted_raw_packets =
                compact_raw_packets(&raw_packets, maximum_transmission_unit);

            for compacted_raw_packets in &compacted_raw_packets {
                next_conn
                    .send(compacted_raw_packets)
                    .await
                    .map_err(|err| Error::Other(err.to_string()))?;
            }
        }

        Ok(())
    }

    async fn process_packet(
        local_sequence_number: &Arc<Mutex<Vec<u64>>>,
        cipher_suite: &Arc<std::sync::Mutex<Option<Box<dyn CipherSuite + Send + Sync>>>>,
        p: &mut Packet,
    ) -> Result<Vec<u8>> {
        let epoch = p.record.record_layer_header.epoch as usize;
        let seq = {
            let mut lsn = local_sequence_number.lock().await;
            while lsn.len() <= epoch {
                lsn.push(0);
            }

            lsn[epoch] += 1;
            lsn[epoch] - 1
        };
        //trace!("{}: seq = {}", srv_cli_str(is_client), seq);

        if seq > MAX_SEQUENCE_NUMBER {
            // RFC 6347 Section 4.1.0
            // The implementation must either abandon an association or rehandshake
            // prior to allowing the sequence number to wrap.
            return Err(Error::ErrSequenceNumberOverflow);
        }
        p.record.record_layer_header.sequence_number = seq;

        let mut raw_packet = vec![];
        {
            let mut writer = BufWriter::<&mut Vec<u8>>::new(raw_packet.as_mut());
            p.record.marshal(&mut writer)?;
        }

        if p.should_encrypt {
            let cipher_suite = cipher_suite.lock()?;
            if let Some(cipher_suite) = &*cipher_suite {
                raw_packet = cipher_suite.encrypt(&p.record.record_layer_header, &raw_packet)?;
            }
        }

        Ok(raw_packet)
    }

    async fn process_handshake_packet(
        local_sequence_number: &Arc<Mutex<Vec<u64>>>,
        cipher_suite: &Arc<std::sync::Mutex<Option<Box<dyn CipherSuite + Send + Sync>>>>,
        maximum_transmission_unit: usize,
        p: &Packet,
        h: &Handshake,
    ) -> Result<Vec<Vec<u8>>> {
        let mut raw_packets = vec![];

        let handshake_fragments = DTLSConn::fragment_handshake(maximum_transmission_unit, h)?;

        let epoch = p.record.record_layer_header.epoch as usize;

        let mut lsn = local_sequence_number.lock().await;
        while lsn.len() <= epoch {
            lsn.push(0);
        }

        for handshake_fragment in &handshake_fragments {
            let seq = {
                lsn[epoch] += 1;
                lsn[epoch] - 1
            };
            //trace!("seq = {}", seq);
            if seq > MAX_SEQUENCE_NUMBER {
                return Err(Error::ErrSequenceNumberOverflow);
            }

            let record_layer_header = RecordLayerHeader {
                protocol_version: p.record.record_layer_header.protocol_version,
                content_type: p.record.record_layer_header.content_type,
                content_len: handshake_fragment.len() as u16,
                epoch: p.record.record_layer_header.epoch,
                sequence_number: seq,
            };

            let mut record_layer_header_bytes = vec![];
            {
                let mut writer = BufWriter::<&mut Vec<u8>>::new(record_layer_header_bytes.as_mut());
                record_layer_header.marshal(&mut writer)?;
            }

            //p.record.record_layer_header = record_layer_header;

            let mut raw_packet = vec![];
            raw_packet.extend_from_slice(&record_layer_header_bytes);
            raw_packet.extend_from_slice(handshake_fragment);
            if p.should_encrypt {
                let cipher_suite = cipher_suite.lock()?;
                if let Some(cipher_suite) = &*cipher_suite {
                    raw_packet = cipher_suite.encrypt(&record_layer_header, &raw_packet)?;
                }
            }

            raw_packets.push(raw_packet);
        }

        Ok(raw_packets)
    }

    fn fragment_handshake(maximum_transmission_unit: usize, h: &Handshake) -> Result<Vec<Vec<u8>>> {
        let mut content = vec![];
        {
            let mut writer = BufWriter::<&mut Vec<u8>>::new(content.as_mut());
            h.handshake_message.marshal(&mut writer)?;
        }

        let mut fragmented_handshakes = vec![];

        let mut content_fragments = split_bytes(&content, maximum_transmission_unit);
        if content_fragments.is_empty() {
            content_fragments = vec![vec![]];
        }

        let mut offset = 0;
        for content_fragment in &content_fragments {
            let content_fragment_len = content_fragment.len();

            let handshake_header_fragment = HandshakeHeader {
                handshake_type: h.handshake_header.handshake_type,
                length: h.handshake_header.length,
                message_sequence: h.handshake_header.message_sequence,
                fragment_offset: offset as u32,
                fragment_length: content_fragment_len as u32,
            };

            offset += content_fragment_len;

            let mut handshake_header_fragment_raw = vec![];
            {
                let mut writer =
                    BufWriter::<&mut Vec<u8>>::new(handshake_header_fragment_raw.as_mut());
                handshake_header_fragment.marshal(&mut writer)?;
            }

            let mut fragmented_handshake = vec![];
            fragmented_handshake.extend_from_slice(&handshake_header_fragment_raw);
            fragmented_handshake.extend_from_slice(content_fragment);

            fragmented_handshakes.push(fragmented_handshake);
        }

        Ok(fragmented_handshakes)
    }

    pub(crate) fn set_handshake_completed(&mut self) {
        self.handshake_completed = true;
    }

    pub(crate) fn is_handshake_completed(&self) -> bool {
        self.handshake_completed
    }

    pub(crate) fn read_and_buffer(&mut self, buf: &[u8]) -> Result<()> {
        for pkt in unpack_datagram(buf)? {
            let (hs, alert, err) = self.handle_incoming_packet(pkt, true);
            if let Some(alert) = alert {
                self.outgoing_packets.push_back(Packet {
                    record: RecordLayer::new(
                        PROTOCOL_VERSION1_2,
                        self.state.local_epoch,
                        Content::Alert(Alert {
                            alert_level: alert.alert_level,
                            alert_description: alert.alert_description,
                        }),
                    ),
                    should_encrypt: self.is_handshake_completed(),
                    reset_local_sequence_number: false,
                });

                if alert.alert_level == AlertLevel::Fatal
                    || alert.alert_description == AlertDescription::CloseNotify
                {
                    return Err(Error::ErrAlertFatalOrClose);
                }
            }

            if let Some(err) = err {
                return Err(err);
            }

            if hs {
                self.handshake_rx = Some(());
            }
        }

        Ok(())
    }

    pub(crate) fn handle_queued_packets(&mut self) -> Result<()> {
        if self.is_handshake_completed() {
            for p in self
                .incoming_encrypted_packets
                .drain(..)
                .collect::<Vec<_>>()
            {
                let (_, alert, err) = self.handle_incoming_packet(p, false); // don't re-enqueue
                if let Some(alert) = alert {
                    self.outgoing_packets.push_back(Packet {
                        record: RecordLayer::new(
                            PROTOCOL_VERSION1_2,
                            self.state.local_epoch,
                            Content::Alert(Alert {
                                alert_level: alert.alert_level,
                                alert_description: alert.alert_description,
                            }),
                        ),
                        should_encrypt: self.is_handshake_completed(),
                        reset_local_sequence_number: false,
                    });

                    if alert.alert_level == AlertLevel::Fatal
                        || alert.alert_description == AlertDescription::CloseNotify
                    {
                        return Err(Error::ErrAlertFatalOrClose);
                    }
                }

                if let Some(err) = err {
                    return Err(err);
                }
            }
        }

        Ok(())
    }

    fn handle_incoming_packet(
        &mut self,
        mut pkt: Vec<u8>,
        enqueue: bool,
    ) -> (bool, Option<Alert>, Option<Error>) {
        let mut reader = BufReader::new(pkt.as_slice());
        let h = match RecordLayerHeader::unmarshal(&mut reader) {
            Ok(h) => h,
            Err(err) => {
                // Decode error must be silently discarded
                // [RFC6347 Section-4.1.2.7]
                debug!(
                    "{}: discarded broken packet: {}",
                    srv_cli_str(self.is_client),
                    err
                );
                return (false, None, None);
            }
        };

        // Validate epoch
        let epoch = self.state.remote_epoch;
        if h.epoch > epoch {
            if h.epoch > epoch + 1 {
                debug!(
                    "{}: discarded future packet (epoch: {}, seq: {})",
                    srv_cli_str(self.is_client),
                    h.epoch,
                    h.sequence_number,
                );
                return (false, None, None);
            }
            if enqueue {
                debug!(
                    "{}: received packet of next epoch, queuing packet",
                    srv_cli_str(self.is_client)
                );
                self.incoming_encrypted_packets.push_back(pkt);
            }
            return (false, None, None);
        }

        // Anti-replay protection
        while self.replay_detector.len() <= h.epoch as usize {
            self.replay_detector
                .push(Box::new(SlidingWindowDetector::new(
                    self.replay_protection_window,
                    MAX_SEQUENCE_NUMBER,
                )));
        }

        let ok = self.replay_detector[h.epoch as usize].check(h.sequence_number);
        if !ok {
            debug!(
                "{}: discarded duplicated packet (epoch: {}, seq: {})",
                srv_cli_str(self.is_client),
                h.epoch,
                h.sequence_number,
            );
            return (false, None, None);
        }

        // Decrypt
        if h.epoch != 0 {
            let invalid_cipher_suite = {
                if let Some(cipher_suite) = &self.state.cipher_suite {
                    !cipher_suite.is_initialized()
                } else {
                    true
                }
            };
            if invalid_cipher_suite {
                if enqueue {
                    debug!(
                        "{}: handshake not finished, queuing packet",
                        srv_cli_str(self.is_client)
                    );
                    self.incoming_encrypted_packets.push_back(pkt);
                }
                return (false, None, None);
            }

            if let Some(cipher_suite) = &self.state.cipher_suite {
                pkt = match cipher_suite.decrypt(&pkt) {
                    Ok(pkt) => pkt,
                    Err(err) => {
                        debug!("{}: decrypt failed: {}", srv_cli_str(self.is_client), err);
                        return (false, None, None);
                    }
                };
            }
        }

        let is_handshake = match self.fragment_buffer.push(&pkt) {
            Ok(is_handshake) => is_handshake,
            Err(err) => {
                // Decode error must be silently discarded
                // [RFC6347 Section-4.1.2.7]
                debug!(
                    "{}: defragment failed: {}",
                    srv_cli_str(self.is_client),
                    err
                );
                return (false, None, None);
            }
        };
        if is_handshake {
            self.replay_detector[h.epoch as usize].accept();
            while let Ok((out, epoch)) = self.fragment_buffer.pop() {
                //log::debug!("Extension Debug: out.len()={}", out.len());
                let mut reader = BufReader::new(out.as_slice());
                let raw_handshake = match Handshake::unmarshal(&mut reader) {
                    Ok(rh) => {
                        trace!(
                            "Recv [handshake:{}] -> {} (epoch: {}, seq: {})",
                            srv_cli_str(self.is_client),
                            rh.handshake_header.handshake_type.to_string(),
                            h.epoch,
                            rh.handshake_header.message_sequence
                        );
                        rh
                    }
                    Err(err) => {
                        debug!(
                            "{}: handshake parse failed: {}",
                            srv_cli_str(self.is_client),
                            err
                        );
                        continue;
                    }
                };

                self.cache.push(
                    out,
                    epoch,
                    raw_handshake.handshake_header.message_sequence,
                    raw_handshake.handshake_header.handshake_type,
                    !self.is_client,
                );
            }

            return (true, None, None);
        }

        let mut reader = BufReader::new(pkt.as_slice());
        let r = match RecordLayer::unmarshal(&mut reader) {
            Ok(r) => r,
            Err(err) => {
                return (
                    false,
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::DecodeError,
                    }),
                    Some(err),
                );
            }
        };

        match r.content {
            Content::Alert(mut a) => {
                trace!("{}: <- {}", srv_cli_str(self.is_client), a.to_string());
                if a.alert_description == AlertDescription::CloseNotify {
                    // Respond with a close_notify [RFC5246 Section 7.2.1]
                    a = Alert {
                        alert_level: AlertLevel::Warning,
                        alert_description: AlertDescription::CloseNotify,
                    };
                }
                self.replay_detector[h.epoch as usize].accept();
                return (
                    false,
                    Some(a),
                    Some(Error::Other(format!("Error of Alert {a}"))),
                );
            }
            Content::ChangeCipherSpec(_) => {
                let invalid_cipher_suite = {
                    if let Some(cipher_suite) = &self.state.cipher_suite {
                        !cipher_suite.is_initialized()
                    } else {
                        true
                    }
                };

                if invalid_cipher_suite {
                    if enqueue {
                        debug!(
                            "{}: CipherSuite not initialized, queuing packet",
                            srv_cli_str(self.is_client)
                        );
                        self.incoming_encrypted_packets.push_back(pkt);
                    }
                    return (false, None, None);
                }

                let new_remote_epoch = h.epoch + 1;
                trace!(
                    "{}: <- ChangeCipherSpec (epoch: {})",
                    srv_cli_str(self.is_client),
                    new_remote_epoch
                );

                if epoch + 1 == new_remote_epoch {
                    self.state.remote_epoch = new_remote_epoch;
                    self.replay_detector[h.epoch as usize].accept();
                }
            }
            Content::ApplicationData(a) => {
                if h.epoch == 0 {
                    return (
                        false,
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::UnexpectedMessage,
                        }),
                        Some(Error::ErrApplicationDataEpochZero),
                    );
                }

                self.replay_detector[h.epoch as usize].accept();

                self.incoming_decrypted_packets.push_back(a.data);
            }
            _ => {
                return (
                    false,
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::UnexpectedMessage,
                    }),
                    Some(Error::ErrUnhandledContextType),
                );
            }
        };

        (false, None, None)
    }

    fn is_connection_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn set_local_epoch(&mut self, epoch: u16) {
        self.state.local_epoch = epoch;
    }

    pub(crate) fn get_local_epoch(&self) -> u16 {
        self.state.local_epoch
    }
}

fn compact_raw_packets(raw_packets: &[Vec<u8>], maximum_transmission_unit: usize) -> Vec<Vec<u8>> {
    let mut combined_raw_packets = vec![];
    let mut current_combined_raw_packet = vec![];

    for raw_packet in raw_packets {
        if !current_combined_raw_packet.is_empty()
            && current_combined_raw_packet.len() + raw_packet.len() >= maximum_transmission_unit
        {
            combined_raw_packets.push(current_combined_raw_packet);
            current_combined_raw_packet = vec![];
        }
        current_combined_raw_packet.extend_from_slice(raw_packet);
    }

    combined_raw_packets.push(current_combined_raw_packet);

    combined_raw_packets
}

fn split_bytes(bytes: &[u8], split_len: usize) -> Vec<Vec<u8>> {
    let mut splits = vec![];
    let num_bytes = bytes.len();
    for i in (0..num_bytes).step_by(split_len) {
        let mut j = i + split_len;
        if j > num_bytes {
            j = num_bytes;
        }

        splits.push(bytes[i..j].to_vec());
    }

    splits
}
