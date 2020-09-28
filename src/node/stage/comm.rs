// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::error::{Error, Result};
use bytes::Bytes;
use futures::{
    lock::Mutex,
    stream::{FuturesUnordered, StreamExt},
};
use lru_time_cache::LruCache;
use qp2p::{Connection, Endpoint, IncomingConnections, QuicP2p};
use std::{net::SocketAddr, slice, sync::Arc};

// Number of Connections to maintain in the cache
const CONNECTIONS_CACHE_SIZE: usize = 1024;

/// Maximal number of resend attempts to the same target.
pub const RESEND_MAX_ATTEMPTS: u8 = 3;

// Communication component of the node to interact with other nodes.
#[derive(Clone)]
pub(crate) struct Comm {
    inner: Arc<Inner>,
}

impl Comm {
    pub fn new(transport_config: qp2p::Config) -> Result<Self> {
        let quic_p2p = QuicP2p::with_config(Some(transport_config), Default::default(), true)?;

        // Don't bootstrap, just create an endpoint where to listen to
        // the incoming messages from other nodes.
        let endpoint = quic_p2p.new_endpoint()?;
        let node_conns = Mutex::new(LruCache::with_capacity(CONNECTIONS_CACHE_SIZE));

        Ok(Self {
            inner: Arc::new(Inner {
                _quic_p2p: quic_p2p,
                endpoint,
                node_conns,
            }),
        })
    }

    pub async fn from_bootstrapping(transport_config: qp2p::Config) -> Result<(Self, SocketAddr)> {
        let mut quic_p2p = QuicP2p::with_config(Some(transport_config), Default::default(), true)?;

        // Bootstrap to the network returning the connection to a node.
        let (endpoint, conn) = quic_p2p.bootstrap().await?;
        let addr = conn.remote_address();

        let mut node_conns = LruCache::with_capacity(CONNECTIONS_CACHE_SIZE);
        let _ = node_conns.insert(addr, Arc::new(conn));
        let node_conns = Mutex::new(node_conns);

        Ok((
            Self {
                inner: Arc::new(Inner {
                    _quic_p2p: quic_p2p,
                    endpoint,
                    node_conns,
                }),
            },
            addr,
        ))
    }

    /// Starts listening for connections returning a stream where to read them from.
    pub fn listen(&self) -> Result<IncomingConnections> {
        Ok(self.inner.endpoint.listen()?)
    }

    pub fn our_connection_info(&self) -> Result<SocketAddr> {
        self.inner.endpoint.our_endpoint().map_err(|err| {
            debug!("Failed to retrieve our connection info: {:?}", err);
            err.into()
        })
    }

    pub async fn send_message_to_targets(
        &self,
        recipients: &[SocketAddr],
        delivery_group_size: usize,
        msg: Bytes,
    ) -> SendStatus {
        if recipients.len() < delivery_group_size {
            warn!(
                "Less than delivery_group_size valid recipients - delivery_group_size: {}, recipients: {:?}",
                delivery_group_size,
                recipients,
            );
        }

        // Use `FuturesUnordered` to execute all the send tasks concurrently, but still on the same
        // thread.
        let mut state = SendState::new(recipients, delivery_group_size);
        let mut tasks = FuturesUnordered::new();

        loop {
            while let Some(addr) = state.next() {
                trace!("Sending message to {}", addr);
                let msg = msg.clone();
                let task = async move {
                    let result = self.inner.send(&addr, msg).await;
                    (addr, result)
                };
                tasks.push(task);
            }

            if let Some((addr, result)) = tasks.next().await {
                match result {
                    Ok(_) => {
                        trace!("Sending message to {} succeeded", addr);
                        state.success(&addr);
                    }
                    Err(err) => {
                        trace!("Sending message to {} failed: {}", addr, err);
                        state.failure(&addr);
                    }
                }
            } else {
                break;
            }
        }

        let status = state.finish();

        trace!(
            "Sending message finished to {}/{} recipients (failed: {:?})",
            delivery_group_size - status.remaining,
            delivery_group_size,
            status.failed_recipients
        );

        status
    }

    pub async fn send_message_to_target(&self, recipient: &SocketAddr, msg: Bytes) -> SendStatus {
        self.send_message_to_targets(slice::from_ref(recipient), 1, msg)
            .await
    }
}

#[derive(Debug)]
pub struct SendStatus {
    // The number of recipients out of the requested delivery group that we haven't successfully
    // sent the message to.
    pub remaining: usize,
    // Recipients that failed all the send attempts.
    pub failed_recipients: Vec<SocketAddr>,
}

impl From<SendStatus> for Result<(), Error> {
    fn from(status: SendStatus) -> Self {
        if status.remaining == 0 {
            Ok(())
        } else {
            Err(Error::FailedSend)
        }
    }
}

struct Inner {
    _quic_p2p: QuicP2p,
    endpoint: Endpoint,
    node_conns: Mutex<LruCache<SocketAddr, Arc<Connection>>>,
}

impl Inner {
    async fn send(&self, recipient: &SocketAddr, msg: Bytes) -> Result<(), qp2p::Error> {
        // Cache the Connection to the node or obtain the already cached one
        // Note: not using the entry API to avoid holding the mutex longer than necessary.
        let conn = self.node_conns.lock().await.get(recipient).cloned();
        let conn = if let Some(conn) = conn {
            conn
        } else {
            let conn = self.endpoint.connect_to(recipient).await?;
            let conn = Arc::new(conn);
            let _ = self
                .node_conns
                .lock()
                .await
                .insert(*recipient, Arc::clone(&conn));

            conn
        };

        conn.send_uni(msg).await?;

        Ok(())
    }
}

// Helper to track the sending of a single message to potentially multiple recipients.
struct SendState {
    recipients: Vec<Recipient>,
    remaining: usize,
}

struct Recipient {
    addr: SocketAddr,
    sending: bool,
    attempt: u8,
}

impl SendState {
    fn new(recipients: &[SocketAddr], delivery_group_size: usize) -> Self {
        Self {
            recipients: recipients
                .iter()
                .map(|addr| Recipient {
                    addr: *addr,
                    sending: false,
                    attempt: 0,
                })
                .collect(),
            remaining: delivery_group_size,
        }
    }

    // Returns the next recipient to sent to.
    fn next(&mut self) -> Option<SocketAddr> {
        let active = self
            .recipients
            .iter()
            .filter(|recipient| recipient.sending)
            .count();

        if active >= self.remaining {
            return None;
        }

        let recipient = self
            .recipients
            .iter_mut()
            .filter(|recipient| !recipient.sending && recipient.attempt < RESEND_MAX_ATTEMPTS)
            .min_by_key(|recipient| recipient.attempt)?;

        recipient.attempt += 1;
        recipient.sending = true;

        Some(recipient.addr)
    }

    // Marks the recipient as failed.
    fn failure(&mut self, addr: &SocketAddr) {
        if let Some(recipient) = self
            .recipients
            .iter_mut()
            .find(|recipient| recipient.addr == *addr)
        {
            recipient.sending = false;
        }
    }

    // Marks the recipient as successful.
    fn success(&mut self, addr: &SocketAddr) {
        if let Some(index) = self
            .recipients
            .iter()
            .position(|recipient| recipient.addr == *addr)
        {
            let _ = self.recipients.swap_remove(index);
            self.remaining -= 1;
        }
    }

    fn finish(self) -> SendStatus {
        SendStatus {
            remaining: self.remaining,
            failed_recipients: self
                .recipients
                .into_iter()
                .filter(|recipient| !recipient.sending && recipient.attempt >= RESEND_MAX_ATTEMPTS)
                .map(|recipient| recipient.addr)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use futures::future;
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::Duration,
    };
    use tokio::{net::UdpSocket, sync::mpsc, time};

    #[tokio::test]
    async fn successful_send() -> Result<()> {
        let comm = Comm::new(transport_config())?;

        let mut peer0 = Peer::new()?;
        let mut peer1 = Peer::new()?;

        let message = Bytes::from_static(b"hello world");
        let status = comm
            .send_message_to_targets(&[peer0.addr, peer1.addr], 2, message.clone())
            .await;

        assert_eq!(status.remaining, 0);
        assert!(status.failed_recipients.is_empty());

        assert_eq!(peer0.rx.recv().await, Some(message.clone()));
        assert_eq!(peer1.rx.recv().await, Some(message));

        Ok(())
    }

    #[tokio::test]
    async fn successful_send_to_subset() -> Result<()> {
        let comm = Comm::new(transport_config())?;

        let mut peer0 = Peer::new()?;
        let mut peer1 = Peer::new()?;

        let message = Bytes::from_static(b"hello world");
        let status = comm
            .send_message_to_targets(&[peer0.addr, peer1.addr], 1, message.clone())
            .await;

        assert_eq!(status.remaining, 0);
        assert!(status.failed_recipients.is_empty());

        assert_eq!(peer0.rx.recv().await, Some(message));

        assert!(time::timeout(Duration::from_millis(100), peer1.rx.recv())
            .await
            .unwrap_or_default()
            .is_none());

        Ok(())
    }

    #[tokio::test]
    async fn failed_send() -> Result<()> {
        let comm = Comm::new(transport_config())?;
        let invalid_addr = get_invalid_addr().await?;

        let message = Bytes::from_static(b"hello world");
        let status = comm
            .send_message_to_targets(&[invalid_addr], 1, message.clone())
            .await;

        assert_eq!(status.remaining, 1);
        assert_eq!(status.failed_recipients, [invalid_addr]);

        Ok(())
    }

    #[tokio::test]
    async fn successful_send_after_failed_attempts() -> Result<()> {
        let comm = Comm::new(transport_config())?;
        let mut peer = Peer::new()?;
        let invalid_addr = get_invalid_addr().await?;

        let message = Bytes::from_static(b"hello world");
        let status = comm
            .send_message_to_targets(&[invalid_addr, peer.addr], 1, message.clone())
            .await;

        assert_eq!(status.remaining, 0);
        assert_eq!(peer.rx.recv().await, Some(message));

        Ok(())
    }

    #[tokio::test]
    async fn partially_successful_send() -> Result<()> {
        let comm = Comm::new(transport_config())?;
        let mut peer = Peer::new()?;
        let invalid_addr = get_invalid_addr().await?;

        let message = Bytes::from_static(b"hello world");
        let status = comm
            .send_message_to_targets(&[invalid_addr, peer.addr], 2, message.clone())
            .await;

        assert_eq!(status.remaining, 1);
        assert_eq!(status.failed_recipients, [invalid_addr]);
        assert_eq!(peer.rx.recv().await, Some(message));

        Ok(())
    }

    fn transport_config() -> qp2p::Config {
        qp2p::Config {
            ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            idle_timeout_msec: Some(1),
            ..Default::default()
        }
    }

    struct Peer {
        addr: SocketAddr,
        rx: mpsc::Receiver<Bytes>,
    }

    impl Peer {
        fn new() -> Result<Self> {
            let transport = QuicP2p::with_config(Some(transport_config()), &[], false)?;

            let endpoint = transport.new_endpoint()?;
            let addr = endpoint.local_addr()?;
            let mut incoming_connections = endpoint.listen()?;

            let (tx, rx) = mpsc::channel(1);

            let _ = tokio::spawn(async move {
                while let Some(mut connection) = incoming_connections.next().await {
                    let mut tx = tx.clone();
                    let _ = tokio::spawn(async move {
                        while let Some(message) = connection.next().await {
                            let _ = tx.send(message.get_message_data()).await;
                        }
                    });
                }
            });

            Ok(Self { addr, rx })
        }
    }

    async fn get_invalid_addr() -> Result<SocketAddr> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = socket.local_addr()?;

        // Keep the socket alive to keep the address bound, but don't read/write to it so any
        // attempt to connect to it will fail.
        let _ = tokio::spawn(async move {
            future::pending::<()>().await;
            let _ = socket;
        });

        Ok(addr)
    }
}
