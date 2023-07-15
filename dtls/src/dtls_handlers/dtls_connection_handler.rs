use retty::channel::{Handler, InboundContext, InboundHandler, OutboundContext, OutboundHandler};
use retty::transport::{TaggedBytesMut, TransportContext};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use crate::config::HandshakeConfig;
use crate::conn::DTLSConn;
use crate::state::State;
use bytes::BytesMut;
use shared::error::Result;

struct DtlsConnectionInboundHandler {
    transport: Option<TransportContext>,
    conn: Rc<RefCell<DTLSConn>>,
}
struct DtlsConnectionOutboundHandler {
    transport: Option<TransportContext>,
    conn: Rc<RefCell<DTLSConn>>,
}
pub struct DtlsConnectionHandler {
    inbound: DtlsConnectionInboundHandler,
    outbound: DtlsConnectionOutboundHandler,
}

impl DtlsConnectionHandler {
    pub fn new(
        handshake_config: HandshakeConfig,
        is_client: bool,
        client_transport: Option<TransportContext>,
        initial_state: Option<State>,
    ) -> Self {
        let conn = Rc::new(RefCell::new(DTLSConn::new(
            handshake_config,
            is_client,
            initial_state,
        )));

        DtlsConnectionHandler {
            inbound: DtlsConnectionInboundHandler {
                transport: client_transport,
                conn: Rc::clone(&conn),
            },
            outbound: DtlsConnectionOutboundHandler {
                transport: client_transport,
                conn,
            },
        }
    }
}

impl InboundHandler for DtlsConnectionInboundHandler {
    type Rin = TaggedBytesMut;
    type Rout = Self::Rin;

    fn transport_active(&mut self, ctx: &InboundContext<Self::Rin, Self::Rout>) {
        let try_dtls_active = || -> Result<()> {
            let mut conn = self.conn.borrow_mut();
            conn.handshake()
        };
        if let Err(err) = try_dtls_active() {
            ctx.fire_read_exception(Box::new(err));
        }
        handle_outgoing(ctx, &self.conn, &self.transport);

        ctx.fire_transport_active();
    }

    fn transport_inactive(&mut self, ctx: &InboundContext<Self::Rin, Self::Rout>) {
        ctx.fire_transport_inactive();
    }

    fn read(&mut self, ctx: &InboundContext<Self::Rin, Self::Rout>, msg: Self::Rin) {
        if self.transport.is_none() {
            self.transport = Some(msg.transport);
        }

        let try_dtls_read = || -> Result<Vec<BytesMut>> {
            let mut messages = vec![];
            let mut conn = self.conn.borrow_mut();
            conn.read(&msg.message)?;
            if !conn.is_handshake_completed() {
                conn.handshake()?;
                conn.handle_incoming_queued_packets()?;
            }
            while let Some(message) = conn.incoming_application_data() {
                messages.push(message);
            }
            Ok(messages)
        };
        match try_dtls_read() {
            Ok(messages) => {
                for message in messages {
                    ctx.fire_read(TaggedBytesMut {
                        now: msg.now,
                        transport: msg.transport,
                        message,
                    })
                }
            }
            Err(err) => ctx.fire_read_exception(Box::new(err)),
        };
        handle_outgoing(ctx, &self.conn, &self.transport);
    }

    fn handle_timeout(&mut self, ctx: &InboundContext<Self::Rin, Self::Rout>, now: Instant) {
        let try_dtls_timeout = || -> Result<()> {
            let mut conn = self.conn.borrow_mut();
            if conn.current_retransmit_timer.take().is_some() && !conn.is_handshake_completed() {
                conn.handshake_timeout(now)?
            }
            Ok(())
        };
        if let Err(err) = try_dtls_timeout() {
            ctx.fire_read_exception(Box::new(err));
        }
        handle_outgoing(ctx, &self.conn, &self.transport);

        ctx.fire_handle_timeout(now);
    }

    fn poll_timeout(&mut self, ctx: &InboundContext<Self::Rin, Self::Rout>, eto: &mut Instant) {
        let current_eto = {
            let conn = self.conn.borrow();
            conn.current_retransmit_timer
        };
        if let Some(current_eto) = current_eto {
            if current_eto < *eto {
                *eto = current_eto;
            }
        };
        ctx.fire_poll_timeout(eto);
    }
}

impl OutboundHandler for DtlsConnectionOutboundHandler {
    type Win = TaggedBytesMut;
    type Wout = Self::Win;

    fn write(&mut self, ctx: &OutboundContext<Self::Win, Self::Wout>, msg: Self::Win) {
        if self.transport.is_none() {
            self.transport = Some(msg.transport);
        }

        let try_dtls_write = || -> Result<()> {
            let mut conn = self.conn.borrow_mut();
            conn.write(&msg.message)
        };
        if let Err(err) = try_dtls_write() {
            ctx.fire_write_exception(Box::new(err));
        }
        handle_outgoing(ctx, &self.conn, &self.transport);
    }

    fn close(&mut self, ctx: &OutboundContext<Self::Win, Self::Wout>) {
        {
            let mut conn = self.conn.borrow_mut();
            conn.close();
        }
        handle_outgoing(ctx, &self.conn, &self.transport);

        ctx.fire_close();
    }
}

impl Handler for DtlsConnectionHandler {
    type Rin = TaggedBytesMut;
    type Rout = Self::Rin;
    type Win = TaggedBytesMut;
    type Wout = Self::Win;

    fn name(&self) -> &str {
        "DtlsConnectionHandler"
    }

    fn split(
        self,
    ) -> (
        Box<dyn InboundHandler<Rin = Self::Rin, Rout = Self::Rout>>,
        Box<dyn OutboundHandler<Win = Self::Win, Wout = Self::Wout>>,
    ) {
        (Box::new(self.inbound), Box::new(self.outbound))
    }
}

fn handle_outgoing(
    ctx: &OutboundContext<TaggedBytesMut, TaggedBytesMut>,
    conn: &Rc<RefCell<DTLSConn>>,
    transport: &Option<TransportContext>,
) {
    if let Some(transport) = transport {
        let mut outgoing_raw_packets = vec![];
        {
            let mut c = conn.borrow_mut();
            while let Some(pkt) = c.outgoing_raw_packet() {
                outgoing_raw_packets.push(pkt);
            }
        };
        for message in outgoing_raw_packets {
            ctx.fire_write(TaggedBytesMut {
                now: Instant::now(),
                transport: *transport,
                message,
            });
        }
    }
}