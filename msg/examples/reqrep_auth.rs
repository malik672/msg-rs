use bytes::Bytes;
use tokio_stream::StreamExt;

use msg::{
    tcp::{self, Tcp},
    Authenticator, RepSocket, ReqSocket,
};

#[derive(Default)]
struct Auth;

impl Authenticator for Auth {
    fn authenticate(&self, id: &Bytes) -> bool {
        println!("Auth request from: {:?}", id);
        // Custom authentication logic
        true
    }
}

#[tokio::main]
async fn main() {
    // Initialize the reply socket (server side) with a transport
    // and an authenticator.
    let mut rep = RepSocket::new(Tcp::default()).with_auth(Auth);
    rep.bind("0.0.0.0:4444".parse().unwrap()).await.unwrap();

    // Initialize the request socket (client side) with a transport
    // and an identifier. This will implicitly turn on client authentication.
    let mut req = ReqSocket::new(Tcp::new(
        tcp::Config::default().auth_token(Bytes::from("client1")),
    ));

    req.connect("0.0.0.0:4444".parse().unwrap()).await.unwrap();

    tokio::spawn(async move {
        // Receive the request and respond with "world"
        // RepSocket implements `Stream`
        let req = rep.next().await.unwrap();
        println!("Message: {:?}", req.msg());

        req.respond(Bytes::from("world")).unwrap();
    });

    let res: Bytes = req.request(Bytes::from("hello")).await.unwrap();
    println!("Response: {:?}", res);
}
