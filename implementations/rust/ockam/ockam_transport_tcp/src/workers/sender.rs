use crate::{TcpRecvProcessor, TcpRouterHandle};
use core::time::Duration;
use ockam_core::{
    async_trait,
    compat::{net::SocketAddr, sync::Arc},
    AllowAll,
};
use ockam_core::{
    Address, Any, Decodable, Encodable, LocalMessage, Mailbox, Mailboxes, Message, Result, Routed,
    TransportMessage, Worker,
};
use ockam_node::{Context, ProcessorBuilder, WorkerBuilder};
use ockam_transport_core::TransportError;
use serde::{Deserialize, Serialize};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tracing::{debug, trace, warn};

/// Provides the transmit and receive parts of a TCP connection
#[derive(Debug)]
pub(crate) struct WorkerPair {
    hostnames: Vec<String>,
    peer: SocketAddr,
    tx_addr: Address,
}

impl WorkerPair {
    /// Return a reference to the peer's hostname(s)
    pub fn hostnames(&self) -> &[String] {
        &self.hostnames
    }

    /// Return a reference to the peer's [`SocketAddr`](std::net::SocketAddr)
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Return a clone of the transmit [`Address`]
    pub fn tx_addr(&self) -> Address {
        self.tx_addr.clone()
    }
}

#[derive(Serialize, Deserialize, Message, Clone)]
pub(crate) enum TcpSendWorkerMsg {
    ConnectionClosed,
}

/// A TCP sending message worker
///
/// Create this worker type by calling
/// [`TcpSendWorker::start_pair`](crate::TcpSendWorker::start_pair)
///
/// This half of the worker is created when spawning a new connection
/// worker pair, and listens for messages from the node message system
/// to dispatch to a remote peer.
pub(crate) struct TcpSendWorker {
    router_handle: TcpRouterHandle,
    rx: Option<OwnedReadHalf>,
    tx: Option<OwnedWriteHalf>,
    peer: SocketAddr,
    internal_addr: Address,
    rx_addr: Option<Address>,
}

impl TcpSendWorker {
    /// Create a new `TcpSendWorker`
    fn new(
        router_handle: TcpRouterHandle,
        stream: Option<TcpStream>,
        peer: SocketAddr,
        internal_addr: Address,
    ) -> Self {
        let (rx, tx) = match stream {
            Some(s) => {
                let (rx, tx) = s.into_split();
                (Some(rx), Some(tx))
            }
            None => (None, None),
        };

        Self {
            router_handle,
            rx,
            tx,
            peer,
            internal_addr,
            rx_addr: None,
        }
    }

    pub(crate) fn internal_addr(&self) -> &Address {
        &self.internal_addr
    }

    /// Create a `(TcpSendWorker, WorkerPair)` without spawning the worker.
    pub(crate) async fn new_pair(
        router_handle: TcpRouterHandle,
        stream: Option<TcpStream>,
        peer: SocketAddr,
        hostnames: Vec<String>,
    ) -> Result<(Self, WorkerPair)> {
        let tx_addr = Address::random_tagged("TcpSendWorker_tx_addr");
        let int_addr = Address::random_tagged("TcpSendWorker_int_addr");
        let sender = TcpSendWorker::new(router_handle, stream, peer, int_addr);
        Ok((
            sender,
            WorkerPair {
                hostnames,
                peer,
                tx_addr,
            },
        ))
    }

    /// Start a `(TcpSendWorker, TcpRecvProcessor)` pair that opens and
    /// manages the connection with the given peer
    pub(crate) async fn start_pair(
        // NOTE context is 0#TcpRouter.detached _not_ 0#TcpRouter_main_addr!
        ctx: &Context,
        router_handle: TcpRouterHandle,
        stream: Option<TcpStream>,
        peer: SocketAddr,
        hostnames: Vec<String>,
    ) -> Result<WorkerPair> {
        // save the TcpRouter main address
        let _tcprouter_main_addr = router_handle.main_addr().clone();

        trace!("Creating new TCP worker pair");
        let (mut worker, pair) = Self::new_pair(router_handle, stream, peer, hostnames).await?;

        // TODO @ac gawd this is bad. Also assigned in `TcpSendWorker::initialize` depending on context.
        let rx_addr = Address::random_tagged("TcpRecvProcessor");
        worker.rx_addr = Some(rx_addr.clone());

        // TODO: @ac 0#TcpSendWorker_tx_addr
        // in:  0#TcpSendWorker_tx_addr_9  <=  [0#TcpRouter_main_addr_0]
        // out: n/a
        let tx_mailbox = Mailbox::new(
            pair.tx_addr(),
            Arc::new(AllowAll),
            // Arc::new(ockam_core::AllowSourceAddress(tcprouter_main_addr)),
            Arc::new(AllowAll),
            // Arc::new(ockam_core::DenyAll),
        );

        // @ac 0#TcpSendWorker_int_addr
        // in:  0#TcpSendWorker_int_addr_10  <=  [0#TcpRecvProcessor_12]
        // out: n/a
        let internal_mailbox = Mailbox::new(
            worker.internal_addr().clone(),
            Arc::new(AllowAll),
            // Arc::new(ockam_core::AllowSourceAddress(rx_addr)),
            Arc::new(AllowAll),
            // Arc::new(ockam_core::DenyAll),
        );

        WorkerBuilder::with_mailboxes(Mailboxes::new(tx_mailbox, vec![internal_mailbox]), worker)
            .start(ctx)
            .await?;

        Ok(pair)
    }

    async fn stop_and_unregister(&self, ctx: &Context) -> Result<()> {
        self.router_handle.unregister(ctx.address()).await?;

        ctx.stop_worker(ctx.address()).await?;

        Ok(())
    }
}

#[async_trait]
impl Worker for TcpSendWorker {
    type Context = Context;
    type Message = Any;

    async fn initialize(&mut self, ctx: &mut Self::Context) -> Result<()> {
        ctx.set_cluster(crate::CLUSTER_NAME).await?;

        if self.tx.is_none() {
            debug!(addr = %self.peer, "Connecting");
            let connection = match TcpStream::connect(self.peer).await {
                Ok(c) => {
                    debug!(addr = %self.peer, "Connected");
                    c
                }
                Err(e) => {
                    debug!(addr = %self.peer, err = %e, "Failed to connect");
                    self.stop_and_unregister(ctx).await?;

                    return Err(TransportError::from(e).into());
                }
            };

            let keepalive = TcpKeepalive::new()
                .with_time(Duration::from_secs(300))
                .with_retries(2)
                .with_interval(Duration::from_secs(75));
            let socket = SockRef::from(&connection);
            socket.set_tcp_keepalive(&keepalive).unwrap();

            let (rx, tx) = connection.into_split();
            self.tx = Some(tx);
            self.rx = Some(rx);
        }

        let rx = self.rx.take().ok_or(TransportError::GenericIo)?;

        //let rx_addr = Address::random_tagged("TcpRecvProcessor");
        let rx_addr = if let Some(rx_addr) = &self.rx_addr {
            rx_addr.clone()
        } else {
            // TODO @ac gawd this is bad. Also assigned in `TcpSendWorker.start_pair` depending on context.
            Address::random_tagged("TcpRecvProcessor")
        };
        let receiver = TcpRecvProcessor::new(
            rx,
            format!("{}#{}", crate::TCP, self.peer).into(),
            self.internal_addr.clone(),
        );

        // TODO @ac 0#TcpRecvProcessor
        // in:  n/a
        // out: 0#TcpRecvProcessor_12  =>  [0#TcpPortalWorker_remote_6, 0#TcpSendWorker_int_addr_10, 0#outlet]
        let mailbox = Mailbox::new(
            rx_addr,
            Arc::new(AllowAll),
            // Arc::new(ockam_core::DenyAll),
            Arc::new(AllowAll),
            // Arc::new(ockam_core::ToDoAccessControl), // TODO @ac at least LocalOriginOnly
        );
        ProcessorBuilder::with_mailboxes(Mailboxes::new(mailbox, vec![]), receiver)
            .start(ctx)
            .await?;

        // TODO see above
        //self.rx_addr = Some(rx_addr);

        Ok(())
    }

    async fn shutdown(&mut self, ctx: &mut Self::Context) -> Result<()> {
        if let Some(rx_addr) = self.rx_addr.take() {
            let _ = ctx.stop_processor(rx_addr).await;
        }

        Ok(())
    }

    // TcpSendWorker will receive messages from the TcpRouter to send
    // across the TcpStream to our friend
    async fn handle_message(
        &mut self,
        ctx: &mut Context,
        msg: Routed<Self::Message>,
    ) -> Result<()> {
        let tx = match &mut self.tx {
            Some(tx) => tx,
            None => return Err(TransportError::PeerNotFound.into()),
        };

        let recipient = msg.msg_addr();
        if recipient == self.internal_addr {
            let msg = TcpSendWorkerMsg::decode(msg.payload())?;

            match msg {
                TcpSendWorkerMsg::ConnectionClosed => {
                    warn!("Stopping sender due to closed connection {}", self.peer);
                    // No need to stop Receiver as it notified us about connection drop and will
                    // stop itself
                    self.rx_addr = None;
                    self.stop_and_unregister(ctx).await?;

                    return Ok(());
                }
            }
        } else {
            let mut msg = LocalMessage::decode(msg.payload())?.into_transport_message();
            // Remove our own address from the route so the other end
            // knows what to do with the incoming message
            msg.onward_route.step()?;
            // Create a message buffer with prepended length
            let msg = prepare_message(msg)?;

            if tx.write_all(msg.as_slice()).await.is_err() {
                warn!("Failed to send message to peer {}", self.peer);
                self.stop_and_unregister(ctx).await?;

                return Ok(());
            }
        }

        Ok(())
    }
}

/// Helper that creates a length-prefixed buffer containing the given
/// `TransportMessage`'s payload
///
/// The length-prefix is encoded as a big-endian 16-bit unsigned
/// integer.
fn prepare_message(msg: TransportMessage) -> Result<Vec<u8>> {
    let mut msg_buf = msg.encode().map_err(|_| TransportError::SendBadMessage)?;

    // Create a buffer that includes the message length in big endian
    let mut len = (msg_buf.len() as u16).to_be_bytes().to_vec();

    // Fun fact: reversing a vector in place, appending the length,
    // and then reversing it again is faster for large message sizes
    // than adding the large chunk of data.
    //
    // https://play.rust-lang.org/?version=stable&mode=release&edition=2018&gist=8669a640004ac85c7be38b19e3e73dcb
    msg_buf.reverse();
    len.reverse();
    msg_buf.append(&mut len);
    msg_buf.reverse();

    Ok(msg_buf)
}
