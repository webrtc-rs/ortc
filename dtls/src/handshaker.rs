use crate::cipher_suite::*;
use crate::config::*;
use crate::conn::*;
use crate::content::*;
use crate::crypto::*;
use crate::extension::extension_use_srtp::*;
use crate::signature_hash_algorithm::*;
use shared::error::*;

use log::*;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

//use std::io::BufWriter;

// [RFC6347 Section-4.2.4]
//                      +-----------+
//                +---> | PREPARING | <--------------------+
//                |     +-----------+                      |
//                |           |                            |
//                |           | Buffer next flight         |
//                |           |                            |
//                |          \|/                           |
//                |     +-----------+                      |
//                |     |  SENDING  |<------------------+  | Send
//                |     +-----------+                   |  | HelloRequest
//        Receive |           |                         |  |
//           next |           | Send flight             |  | or
//         flight |  +--------+                         |  |
//                |  |        | Set retransmit timer    |  | Receive
//                |  |       \|/                        |  | HelloRequest
//                |  |  +-----------+                   |  | Send
//                +--)--|  WAITING  |-------------------+  | ClientHello
//                |  |  +-----------+   Timer expires   |  |
//                |  |         |                        |  |
//                |  |         +------------------------+  |
//        Receive |  | Send           Read retransmit      |
//           last |  | last                                |
//         flight |  | flight                              |
//                |  |                                     |
//               \|/\|/                                    |
//            +-----------+                                |
//            | FINISHED  | -------------------------------+
//            +-----------+
//                 |  /|\
//                 |   |
//                 +---+
//              Read retransmit
//           Retransmit last flight

#[derive(Copy, Clone, PartialEq)]
pub(crate) enum HandshakeState {
    Errored,
    Preparing,
    Sending,
    Waiting,
    Finished,
}

impl fmt::Display for HandshakeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            HandshakeState::Errored => write!(f, "Errored"),
            HandshakeState::Preparing => write!(f, "Preparing"),
            HandshakeState::Sending => write!(f, "Sending"),
            HandshakeState::Waiting => write!(f, "Waiting"),
            HandshakeState::Finished => write!(f, "Finished"),
        }
    }
}

pub(crate) type VerifyPeerCertificateFn =
    Arc<dyn (Fn(&[Vec<u8>], &[rustls::Certificate]) -> Result<()>) + Send + Sync>;

pub struct HandshakeConfig {
    pub(crate) local_psk_callback: Option<PskCallback>,
    pub(crate) local_psk_identity_hint: Option<Vec<u8>>,
    pub(crate) local_cipher_suites: Vec<CipherSuiteId>, // Available CipherSuites
    pub(crate) local_signature_schemes: Vec<SignatureHashAlgorithm>, // Available signature schemes
    pub(crate) extended_master_secret: ExtendedMasterSecretType, // Policy for the Extended Master Support extension
    pub(crate) local_srtp_protection_profiles: Vec<SrtpProtectionProfile>, // Available SRTPProtectionProfiles, if empty no SRTP support
    pub(crate) server_name: String,
    pub(crate) client_auth: ClientAuthType, // If we are a client should we request a client certificate
    pub(crate) local_certificates: Vec<Certificate>,
    pub(crate) name_to_certificate: HashMap<String, Certificate>,
    pub(crate) insecure_skip_verify: bool,
    pub(crate) insecure_verification: bool,
    pub(crate) verify_peer_certificate: Option<VerifyPeerCertificateFn>,
    pub(crate) roots_cas: rustls::RootCertStore,
    pub(crate) server_cert_verifier: Arc<dyn rustls::ServerCertVerifier>,
    pub(crate) client_cert_verifier: Option<Arc<dyn rustls::ClientCertVerifier>>,
    pub(crate) retransmit_interval: std::time::Duration,
    pub(crate) initial_epoch: u16,
    pub(crate) maximum_transmission_unit: usize,
    pub(crate) replay_protection_window: usize, //log           logging.LeveledLogger
                                                //mu sync.Mutex
}

impl Default for HandshakeConfig {
    fn default() -> Self {
        HandshakeConfig {
            local_psk_callback: None,
            local_psk_identity_hint: None,
            local_cipher_suites: vec![],
            local_signature_schemes: vec![],
            extended_master_secret: ExtendedMasterSecretType::Disable,
            local_srtp_protection_profiles: vec![],
            server_name: String::new(),
            client_auth: ClientAuthType::NoClientCert,
            local_certificates: vec![],
            name_to_certificate: HashMap::new(),
            insecure_skip_verify: false,
            insecure_verification: false,
            verify_peer_certificate: None,
            roots_cas: rustls::RootCertStore::empty(),
            server_cert_verifier: Arc::new(rustls::WebPKIVerifier::new()),
            client_cert_verifier: None,
            retransmit_interval: std::time::Duration::from_secs(0),
            initial_epoch: 0,
            maximum_transmission_unit: DEFAULT_MTU,
            replay_protection_window: DEFAULT_REPLAY_PROTECTION_WINDOW,
        }
    }
}

impl HandshakeConfig {
    pub(crate) fn get_certificate(&self, server_name: &str) -> Result<Certificate> {
        //TODO
        /*if self.name_to_certificate.is_empty() {
            let mut name_to_certificate = HashMap::new();
            for cert in &self.local_certificates {
                if let Ok((_rem, x509_cert)) = x509_parser::parse_x509_der(&cert.certificate) {
                    if let Some(a) = x509_cert.tbs_certificate.subject.iter_common_name().next() {
                        let common_name = match a.attr_value.as_str() {
                            Ok(cn) => cn.to_lowercase(),
                            Err(err) => return Err(Error::new(err.to_string())),
                        };
                        name_to_certificate.insert(common_name, cert.clone());
                    }
                    if let Some((_, sans)) = x509_cert.tbs_certificate.subject_alternative_name() {
                        for gn in &sans.general_names {
                            match gn {
                                x509_parser::extensions::GeneralName::DNSName(san) => {
                                    let san = san.to_lowercase();
                                    name_to_certificate.insert(san, cert.clone());
                                }
                                _ => {}
                            }
                        }
                    }
                } else {
                    continue;
                }
            }
            self.name_to_certificate = name_to_certificate;
        }*/

        if self.local_certificates.is_empty() {
            return Err(Error::ErrNoCertificates);
        }

        if self.local_certificates.len() == 1 {
            // There's only one choice, so no point doing any work.
            return Ok(self.local_certificates[0].clone());
        }

        if server_name.is_empty() {
            return Ok(self.local_certificates[0].clone());
        }

        let lower = server_name.to_lowercase();
        let name = lower.trim_end_matches('.');

        if let Some(cert) = self.name_to_certificate.get(name) {
            return Ok(cert.clone());
        }

        // try replacing labels in the name with wildcards until we get a
        // match.
        let mut labels: Vec<&str> = name.split_terminator('.').collect();
        for i in 0..labels.len() {
            labels[i] = "*";
            let candidate = labels.join(".");
            if let Some(cert) = self.name_to_certificate.get(&candidate) {
                return Ok(cert.clone());
            }
        }

        // If nothing matches, return the first certificate.
        Ok(self.local_certificates[0].clone())
    }
}

pub(crate) fn srv_cli_str(is_client: bool) -> String {
    if is_client {
        return "client".to_owned();
    }
    "server".to_owned()
}

impl DTLSConn {
    pub(crate) fn handshake(&mut self) -> Result<()> {
        loop {
            trace!(
                "[handshake:{}] {}: {}",
                srv_cli_str(self.state.is_client),
                self.current_flight.to_string(),
                self.current_handshake_state.to_string()
            );

            if self.current_handshake_state == HandshakeState::Finished
                && !self.is_handshake_completed()
            {
                self.set_handshake_completed();
                return Ok(());
            }

            self.current_handshake_state = match self.current_handshake_state {
                HandshakeState::Preparing => self.prepare()?,
                HandshakeState::Sending => self.send()?,
                HandshakeState::Waiting => self.wait()?,
                HandshakeState::Finished => self.finish()?,
                _ => return Err(Error::ErrInvalidFsmTransition),
            };
        }
    }

    fn prepare(&mut self) -> Result<HandshakeState> {
        self.flights = None;

        // Prepare flights
        self.retransmit = self.current_flight.has_retransmit();

        let result = self
            .current_flight
            .generate(&mut self.state, &self.cache, &self.cfg);

        match result {
            Err((a, err)) => {
                if let Some(a) = a {
                    self.notify(a.alert_level, a.alert_description);
                }
                if let Some(err) = err {
                    return Err(err);
                }
            }
            Ok(pkts) => self.flights = Some(pkts),
        };

        let epoch = self.cfg.initial_epoch;
        let mut next_epoch = epoch;
        if let Some(pkts) = &mut self.flights {
            for p in pkts {
                p.record.record_layer_header.epoch += epoch;
                if p.record.record_layer_header.epoch > next_epoch {
                    next_epoch = p.record.record_layer_header.epoch;
                }
                if let Content::Handshake(h) = &mut p.record.content {
                    h.handshake_header.message_sequence = self.state.handshake_send_sequence as u16;
                    self.state.handshake_send_sequence += 1;
                }
            }
        }
        if epoch != next_epoch {
            trace!(
                "[handshake:{}] -> changeCipherSpec (epoch: {})",
                srv_cli_str(self.state.is_client),
                next_epoch
            );
            self.set_local_epoch(next_epoch);
        }

        Ok(HandshakeState::Sending)
    }
    fn send(&mut self) -> Result<HandshakeState> {
        // Send flights
        if let Some(pkts) = self.flights.clone() {
            self.write_packets(pkts);
        }

        if self.current_flight.is_last_send_flight() {
            Ok(HandshakeState::Finished)
        } else {
            self.current_retransmit_timer = Some(Instant::now() + self.cfg.retransmit_interval);
            Ok(HandshakeState::Waiting)
        }
    }
    fn wait(&mut self) -> Result<HandshakeState> {
        if self.handshake_rx.take().is_some() {
            trace!(
                "[handshake:{}] {} received handshake_rx",
                srv_cli_str(self.state.is_client),
                self.current_flight.to_string()
            );
            let result = self.current_flight.parse(
                /*&mut self.handle_queue_tx,*/ &mut self.state,
                &self.cache,
                &self.cfg,
            );
            match result {
                Err((alert, err)) => {
                    trace!(
                        "[handshake:{}] {} result alert:{:?}, err:{:?}",
                        srv_cli_str(self.state.is_client),
                        self.current_flight.to_string(),
                        alert,
                        err
                    );

                    if let Some(alert) = alert {
                        self.notify(alert.alert_level, alert.alert_description);
                    }
                    if let Some(err) = err {
                        return Err(err);
                    }
                }
                Ok(next_flight) => {
                    trace!(
                        "[handshake:{}] {} -> {}",
                        srv_cli_str(self.state.is_client),
                        self.current_flight.to_string(),
                        next_flight.to_string()
                    );
                    if next_flight.is_last_recv_flight()
                        && self.current_flight.to_string() == next_flight.to_string()
                    {
                        return Ok(HandshakeState::Finished);
                    }
                    self.current_flight = next_flight;
                    return Ok(HandshakeState::Preparing);
                }
            }
        }

        Ok(HandshakeState::Waiting)
    }
    fn finish(&mut self) -> Result<HandshakeState> {
        if self.handshake_rx.take().is_some() {
            let result = self.current_flight.parse(
                /*&mut self.handle_queue_tx,*/ &mut self.state,
                &self.cache,
                &self.cfg,
            );
            if let Err((alert, err)) = result {
                if let Some(alert) = alert {
                    self.notify(alert.alert_level, alert.alert_description);
                }
                if let Some(err) = err {
                    return Err(err);
                }
            };
        }

        Ok(HandshakeState::Finished)
    }

    pub(crate) fn handshake_timeout(&mut self, _now: Instant) -> Result<()> {
        let next_handshake_state = if self.current_handshake_state == HandshakeState::Waiting {
            trace!(
                "[handshake:{}] {} retransmit_timer",
                srv_cli_str(self.state.is_client),
                self.current_flight.to_string()
            );
            if self.retransmit {
                Some(HandshakeState::Sending)
            } else {
                //TODO: what's max retransmit?
                self.current_retransmit_timer = Some(Instant::now() + self.cfg.retransmit_interval);
                Some(HandshakeState::Waiting)
            }
        } else if self.current_handshake_state == HandshakeState::Finished {
            // Retransmit last flight
            Some(HandshakeState::Sending)
        } else {
            None
        };

        if let Some(next_handshake_state) = next_handshake_state {
            self.current_handshake_state = next_handshake_state;
            self.handshake()
        } else {
            Ok(())
        }
    }
}
