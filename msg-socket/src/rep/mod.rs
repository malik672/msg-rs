use std::{
    collections::VecDeque,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::{Future, SinkExt, Stream, StreamExt};
use msg_transport::ServerTransport;
use msg_wire::reqrep;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_stream::StreamMap;
use tokio_util::codec::Framed;

const DEFAULT_BUFFER_SIZE: usize = 1024;

/// A reply socket. This socket can bind multiple times.
pub struct RepSocket<T: ServerTransport> {
    from_backend: Option<mpsc::Receiver<Request>>,
    transport: Option<T>,
    local_addr: Option<SocketAddr>,
    options: Arc<RepOptions>,
}

impl<T: ServerTransport> RepSocket<T> {
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }
}

impl<T: ServerTransport> Stream for RepSocket<T> {
    type Item = Request;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.from_backend
            .as_mut()
            .expect("Inactive socket")
            .poll_recv(cx)
    }
}

#[derive(Debug, Error)]
pub enum RepError {
    #[error("IO error: {0:?}")]
    Io(#[from] std::io::Error),
    #[error("Wire protocol error: {0:?}")]
    Wire(#[from] msg_wire::reqrep::Error),
    #[error("Socket closed")]
    SocketClosed,
    #[error("Transport error: {0:?}")]
    Transport(#[from] Box<dyn std::error::Error + Send + Sync>),
}

pub struct RepOptions {
    pub set_nodelay: bool,
    pub max_connections: Option<usize>,
}

impl Default for RepOptions {
    fn default() -> Self {
        Self {
            set_nodelay: true,
            max_connections: None,
        }
    }
}

impl<T: ServerTransport> RepSocket<T> {
    pub fn new(transport: T) -> Self {
        Self::new_with_options(transport, RepOptions::default())
    }

    pub fn new_with_options(transport: T, options: RepOptions) -> Self {
        Self {
            from_backend: None,
            transport: Some(transport),
            local_addr: None,
            options: Arc::new(options),
        }
    }
}

impl<T: ServerTransport> RepSocket<T> {
    pub async fn bind(&mut self, addr: &str) -> Result<(), RepError> {
        let (to_socket, from_backend) = mpsc::channel(DEFAULT_BUFFER_SIZE);

        // Take the transport here, so we can move it into the backend task
        let mut transport = self.transport.take().unwrap();

        transport
            .bind(addr)
            .await
            .map_err(|e| RepError::Transport(Box::new(e)))?;

        let local_addr = transport
            .local_addr()
            .map_err(|e| RepError::Transport(Box::new(e)))?;

        tracing::debug!("Listening on {}", local_addr);

        let backend = RepBackend {
            transport,
            peer_states: StreamMap::with_capacity(128),
            to_socket,
        };

        tokio::spawn(backend);

        self.local_addr = Some(local_addr);
        self.from_backend = Some(from_backend);

        Ok(())
    }
}

pub struct Request {
    source: SocketAddr,
    response: oneshot::Sender<Bytes>,
    msg: Bytes,
}

impl Request {
    pub fn source(&self) -> SocketAddr {
        self.source
    }

    pub fn msg(&self) -> &Bytes {
        &self.msg
    }

    pub fn respond(self, response: Bytes) -> Result<(), RepError> {
        self.response
            .send(response)
            .map_err(|_| RepError::SocketClosed)
    }
}

struct PeerState<T: AsyncRead + AsyncWrite> {
    pending_requests: JoinSet<Option<(u32, Bytes)>>,
    conn: Framed<T, reqrep::Codec>,
    addr: SocketAddr,
    egress_queue: VecDeque<reqrep::Message>,
}

struct RepBackend<T: ServerTransport> {
    transport: T,
    /// [`StreamMap`] of connected peers. The key is the peer's address.
    /// Note that when the [`PeerState`] stream ends, it will be silently removed
    /// from this map.
    peer_states: StreamMap<SocketAddr, PeerState<T::Io>>,
    to_socket: mpsc::Sender<Request>,
}

impl<T: ServerTransport + Unpin> Future for RepBackend<T> {
    type Output = Result<(), RepError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            if let Poll::Ready(Some((peer, msg))) = this.peer_states.poll_next_unpin(cx) {
                match msg {
                    Ok(request) => {
                        tracing::debug!("Received message from peer {}", peer);
                        let _ = this.to_socket.try_send(request);
                    }

                    Err(e) => {
                        tracing::error!("Error receiving message from peer {}: {:?}", peer, e);
                    }
                }

                continue;
            }

            match this.transport.poll_accept(cx) {
                Poll::Ready(Ok((stream, addr))) => {
                    this.peer_states.insert(
                        addr,
                        PeerState {
                            addr,
                            pending_requests: JoinSet::new(),
                            conn: Framed::new(stream, reqrep::Codec::new()),
                            egress_queue: VecDeque::new(),
                        },
                    );
                    tracing::debug!("New connection from {}, inserted into PeerStates", addr);

                    continue;
                }
                Poll::Ready(Err(e)) => {
                    // Errors here are usually about `WouldBlock`
                    tracing::error!("Error accepting connection: {:?}", e);

                    continue;
                }
                Poll::Pending => {}
            }

            return Poll::Pending;
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Stream for PeerState<T> {
    type Item = Result<Request, RepError>;

    /// Advances the state of the peer.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            let _ = this.conn.poll_flush_unpin(cx);

            if this.conn.poll_ready_unpin(cx).is_ready() {
                if let Some(msg) = this.egress_queue.pop_front() {
                    match this.conn.start_send_unpin(msg) {
                        Ok(_) => {
                            // We might be able to send more queued messages
                            continue;
                        }
                        Err(e) => {
                            tracing::error!("Failed to send message to socket: {:?}", e);
                            // End this stream as we can't send any more messages
                            return Poll::Ready(None);
                        }
                    }
                }
            }

            // First, try to drain the egress queue.
            // First check for completed requests
            match this.pending_requests.poll_join_next(cx) {
                Poll::Ready(Some(Ok(Some((id, payload))))) => {
                    let msg = reqrep::Message::new(id, payload);
                    this.egress_queue.push_back(msg);

                    continue;
                }
                Poll::Ready(Some(Err(e))) => {
                    tracing::error!("Error receiving response: {:?}", e);
                    continue;
                }
                _ => {}
            }

            match this.conn.poll_next_unpin(cx) {
                Poll::Ready(Some(result)) => {
                    tracing::trace!("Received message from peer {}: {:?}", this.addr, result);
                    let msg = result?;
                    let msg_id = msg.id();

                    let (tx, rx) = oneshot::channel();

                    // Spawn a task to listen for the response. On success, return message ID and response.
                    this.pending_requests
                        .spawn(async move { rx.await.ok().map(|res| (msg_id, res)) });

                    let request = Request {
                        source: this.addr,
                        response: tx,
                        msg: msg.into_payload(),
                    };

                    return Poll::Ready(Some(Ok(request)));
                }
                Poll::Ready(None) => {
                    tracing::debug!("Connection closed");
                    return Poll::Ready(None);
                }
                Poll::Pending => {}
            }

            return Poll::Pending;
        }
    }
}

#[cfg(test)]
mod tests {
    use msg_transport::Tcp;
    use rand::Rng;

    use crate::req::ReqSocket;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_req_rep() {
        tracing_subscriber::fmt::init();
        let mut rep = RepSocket::new(Tcp::new());
        rep.bind("127.0.0.1:0").await.unwrap();

        let mut req = ReqSocket::new(Tcp::new());
        req.connect(&rep.local_addr().unwrap().to_string())
            .await
            .unwrap();

        tokio::spawn(async move {
            loop {
                let req = rep.next().await.unwrap();

                req.respond(Bytes::from("hello")).unwrap();
            }
        });

        let n_reqs = 100000;
        let mut rng = rand::thread_rng();
        let msg_vec: Vec<Bytes> = (0..n_reqs)
            .map(|_| {
                let mut vec = vec![0u8; 512];
                rng.fill(&mut vec[..]);
                Bytes::from(vec)
            })
            .collect();

        let start = std::time::Instant::now();
        for msg in msg_vec {
            let _res = req.request(msg).await.unwrap();
            // println!("Response: {:?} {:?}", _res, req_start.elapsed());
        }
        let elapsed = start.elapsed();
        tracing::info!("{} reqs in {:?}", n_reqs, elapsed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_batch_req_rep() {
        tracing_subscriber::fmt::init();
        let mut rep = RepSocket::new(Tcp::new());
        rep.bind("127.0.0.1:0").await.unwrap();

        let mut req = ReqSocket::new(Tcp::new());
        req.connect(&rep.local_addr().unwrap().to_string())
            .await
            .unwrap();

        let par_factor = 64;
        let n_reqs = 100000;

        tokio::spawn(async move {
            rep.map(|req| async move {
                req.respond(Bytes::from("hello")).unwrap();
            })
            .buffer_unordered(par_factor)
            .for_each(|_| async {})
            .await;
        });

        let mut rng = rand::thread_rng();
        let msg_vec: Vec<Bytes> = (0..n_reqs)
            .map(|_| {
                let mut vec = vec![0u8; 512];
                rng.fill(&mut vec[..]);
                Bytes::from(vec)
            })
            .collect();

        let start = std::time::Instant::now();

        tokio_stream::iter(msg_vec)
            .map(|msg| req.request(msg))
            .buffer_unordered(par_factor)
            .for_each(|_| async {})
            .await;

        let elapsed = start.elapsed();
        // On my machine (Mac M1 8 cores, 16 GB RAM: 400ms or about 250k req/s)
        tracing::info!(
            "{} reqs in {:?}, req/s: {}",
            n_reqs,
            elapsed,
            n_reqs as f64 / elapsed.as_secs_f64()
        );
    }
}
