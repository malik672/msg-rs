use std::time::Duration;

use bytes::Bytes;
use msg_transport::tcp;
use tokio::sync::oneshot;
use tokio_stream::StreamExt;

use msg::{tcp::Tcp, Authenticator, RepSocket, ReqOptions, ReqSocket};
use tracing::Instrument;

#[derive(Default)]
struct Auth;

impl Authenticator for Auth {
    fn authenticate(&self, id: &Bytes) -> bool {
        tracing::info!("Auth request from: {:?}, authentication passed.", id);
        // Custom authentication logic
        true
    }
}

#[tracing::instrument(name = "RepSocket")]
async fn start_rep() {
    // Initialize the reply socket (server side) with a transport
    // and an authenticator.
    let mut rep = RepSocket::new(Tcp::default()).with_auth(Auth);
    while rep.bind("0.0.0.0:4444".parse().unwrap()).await.is_err() {
        rep = RepSocket::new(Tcp::default()).with_auth(Auth);
        tracing::warn!("Failed to bind rep socket, retrying...");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // Receive the request and respond with "world"
    // RepSocket implements `Stream`
    let mut n_reqs = 0;
    loop {
        let req = rep.next().await.unwrap();
        n_reqs += 1;
        tracing::info!("Message: {:?}", req.msg());

        let msg = String::from_utf8_lossy(req.msg()).to_string();
        let msg_id = msg
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<i32>()
            .unwrap();

        if n_reqs == 5 {
            tracing::warn!(
                "RepSocket received the 5th request, dropping the request to trigger a timeout..."
            );

            continue;
        }

        let response = format!("PONG {msg_id}");
        req.respond(Bytes::from(response)).unwrap();
    }
}

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt::try_init();

    // Initialize the request socket (client side) with a transport
    // and an identifier. This will implicitly turn on client authentication.
    let mut req = ReqSocket::with_options(
        Tcp::new(tcp::Config::default().auth_token(Bytes::from("client1"))),
        ReqOptions::default().timeout(Duration::from_secs(4)),
    );

    let (tx, rx) = oneshot::channel();

    tokio::spawn(
        async move {
            tracing::info!("Trying to connect to rep socket... This will start the connection process in the background, it won't immediately connect.");
            req.connect("0.0.0.0:4444".parse().unwrap()).await.unwrap();

            for i in 0..10 {
                tracing::info!("Sending request {i}...");
                if i == 0 {
                    tracing::warn!("At this point the RepSocket is not running yet, so the request will block while \
                    the ReqSocket continues to establish a connection. The RepSocket will be started in 3 seconds.");
                }

                let msg = format!("PING {i}");

                let res = loop {
                    match req.request(Bytes::from(msg.clone())).await {
                        Ok(res) => break res,
                        Err(e) => {
                            tracing::error!("Request failed: {:?}, retrying...", e);
                            tokio::time::sleep(Duration::from_millis(1000)).await;
                        }
                    }
                };

                tracing::info!("Response: {:?}", res);
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }

            tx.send(true).unwrap();
        }
        .instrument(tracing::info_span!("ReqSocket")),
    );

    tokio::time::sleep(Duration::from_secs(3)).await;

    tracing::info!("==========================");
    tracing::info!("Starting the RepSocket now");
    tracing::info!("==========================");

    tokio::spawn(start_rep());

    // Wait for the client to finish
    rx.await.unwrap();
    tracing::info!("DONE. Sent all 10 PINGS and received 10 PONGS.");
}
