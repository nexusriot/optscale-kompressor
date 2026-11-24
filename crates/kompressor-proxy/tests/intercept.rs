//! The intercept listener, end to end.
//!
//! The unit tests cover parsing, matching and the authority. What they cannot
//! show is the thing the module exists for: that a client which believes it is
//! talking to a provider reaches our router instead, and that a client talking
//! to anything else is not touched. Both are checked here against a real
//! socket, a real handshake and a real `CONNECT`.

use std::path::PathBuf;

use axum::Router;
use axum::routing::get;
use kompressor_proxy::intercept::Intercept;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kompressor-intercept-{name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A router that says who answered, so a response cannot be mistaken for one
/// from somewhere else.
fn router() -> Router {
    Router::new().route("/v1/messages", get(|| async { "served by the router" }))
}

#[tokio::test]
async fn an_intercepted_host_reaches_our_router() {
    let ca_dir = temp_dir("router");
    let intercept = Intercept::bind(
        "127.0.0.1:0",
        &["api.anthropic.com".to_string()],
        &ca_dir,
        &["http/1.1".to_string()],
        8,
    )
    .await
    .expect("bind");
    let proxy = intercept.address();
    let ca_pem = std::fs::read(intercept.certificate_path()).expect("ca pem");

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(intercept.serve(router(), async move {
        let _ = stop_rx.await;
    }));

    // A client that believes it is talking to the provider: it resolves and
    // trusts nothing of ours except the CA, and names the provider's host.
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy}")).expect("proxy"))
        .add_root_certificate(reqwest::Certificate::from_pem(&ca_pem).expect("cert"))
        // Nothing listens on the real address in this test, and nothing needs
        // to: an intercepted connection never reaches an origin.
        .danger_accept_invalid_certs(false)
        .build()
        .expect("client");

    let response = client
        .get("https://api.anthropic.com/v1/messages")
        .send()
        .await
        .expect("request");
    assert!(response.status().is_success(), "{:?}", response.status());
    assert_eq!(response.text().await.unwrap(), "served by the router");

    let _ = stop_tx.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}

#[tokio::test]
async fn an_intercepted_request_carries_the_origin_it_was_addressed_to() {
    use axum::Extension;
    use kompressor_proxy::intercept::InterceptedOrigin;

    let ca_dir = temp_dir("origin");
    let intercept = Intercept::bind(
        "127.0.0.1:0",
        &["api.anthropic.com".to_string()],
        &ca_dir,
        &["http/1.1".to_string()],
        8,
    )
    .await
    .expect("bind");
    let proxy = intercept.address();
    let ca_pem = std::fs::read(intercept.certificate_path()).expect("ca pem");

    let echo = Router::new().route(
        "/v1/code/sessions",
        get(
            |Extension(origin): Extension<InterceptedOrigin>| async move {
                format!("{}:{}", origin.host, origin.port)
            },
        ),
    );
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(intercept.serve(echo, async move {
        let _ = stop_rx.await;
    }));

    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy}")).expect("proxy"))
        .add_root_certificate(reqwest::Certificate::from_pem(&ca_pem).expect("cert"))
        .build()
        .expect("client");
    let response = client
        .get("https://api.anthropic.com/v1/code/sessions")
        .send()
        .await
        .expect("request");
    assert!(response.status().is_success(), "{:?}", response.status());
    assert_eq!(response.text().await.unwrap(), "api.anthropic.com:443");

    let _ = stop_tx.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}

#[tokio::test]
async fn any_other_host_is_tunnelled_untouched() {
    // An origin that is not HTTP at all, so anything that reads or rewrites
    // the stream would corrupt it.
    let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_addr = origin.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = origin.accept().await.unwrap();
        let mut buf = [0u8; 4];
        socket.read_exact(&mut buf).await.unwrap();
        socket.write_all(b"pong").await.unwrap();
    });

    let ca_dir = temp_dir("tunnel");
    let intercept = Intercept::bind(
        "127.0.0.1:0",
        &["api.anthropic.com".to_string()],
        &ca_dir,
        &["http/1.1".to_string()],
        8,
    )
    .await
    .expect("bind");
    let proxy = intercept.address();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(intercept.serve(router(), async move {
        let _ = stop_rx.await;
    }));

    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream
        .write_all(
            format!("CONNECT {origin_addr} HTTP/1.1\r\nHost: {origin_addr}\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&head).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&head)
    );

    // Bytes that are not TLS and not HTTP cross unchanged.
    stream.write_all(b"ping").await.unwrap();
    let mut pong = [0u8; 4];
    stream.read_exact(&mut pong).await.unwrap();
    assert_eq!(&pong, b"pong");

    let _ = stop_tx.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}

#[tokio::test]
async fn a_request_that_is_not_connect_is_refused_in_http() {
    let ca_dir = temp_dir("refuse");
    let intercept = Intercept::bind(
        "127.0.0.1:0",
        &["api.anthropic.com".to_string()],
        &ca_dir,
        &["http/1.1".to_string()],
        8,
    )
    .await
    .expect("bind");
    let proxy = intercept.address();

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(intercept.serve(router(), async move {
        let _ = stop_rx.await;
    }));

    // Somebody opened the port in a browser. A closed socket would read as a
    // dead process; a status line says what happened.
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 405"), "{answer}");

    let _ = stop_tx.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}

/// Send `CONNECT target` to the listener at `proxy` and return its status
/// line and the stream, positioned after the response head.
async fn connect_via(proxy: std::net::SocketAddr, target: &str) -> (String, TcpStream) {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read_exact(&mut byte).await.is_err() {
            break;
        }
        head.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&head)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    (status, stream)
}

/// An intercept listener with nothing to intercept — a plain `CONNECT`
/// proxy — whose tunnels leave through `network`.
async fn tunnel_only(
    name: &str,
    network: kompressor_core::config::NetworkConfig,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    PathBuf,
) {
    let ca_dir = temp_dir(name);
    let intercept = Intercept::bind("127.0.0.1:0", &[], &ca_dir, &["http/1.1".to_string()], 8)
        .await
        .expect("bind")
        .with_outbound(network.outbound().expect("valid"));
    let proxy = intercept.address();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(intercept.serve(router(), async move {
        let _ = stop_rx.await;
    }));
    (proxy, stop_tx, ca_dir)
}

#[tokio::test]
async fn a_tunnelled_host_is_reached_through_the_socks5_proxy() {
    use kompressor_core::config::NetworkConfig;
    use kompressor_core::network::testing::{SocksServer, http_origin};
    let origin = http_origin("through socks");
    let server = SocksServer::with_password("alice", "s3cret");
    let (proxy, stop, ca_dir) = tunnel_only(
        "socks",
        NetworkConfig {
            socks5: server.url(),
            socks5_username: "alice".into(),
            socks5_password: "s3cret".into(),
            no_proxy: vec![],
        },
    )
    .await;

    // What Claude Code or Desktop does with HTTPS_PROXY pointed here, for a
    // host only the SOCKS5 proxy can resolve.
    let (status, mut stream) = connect_via(proxy, &format!("origin.test:{origin}")).await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    assert!(reply.ends_with("through socks"), "{reply}");
    assert_eq!(server.connected(), [format!("origin.test:{origin}")]);

    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}

#[tokio::test]
async fn a_rejected_socks5_password_answers_the_connect_with_502() {
    use kompressor_core::config::NetworkConfig;
    use kompressor_core::network::testing::{Outcome, SocksServer};
    let server = SocksServer::with_password("alice", "s3cret");
    let (proxy, stop, ca_dir) = tunnel_only(
        "socks-refused",
        NetworkConfig {
            socks5: format!("socks5h://alice:wrong@{}", server.address()),
            ..NetworkConfig::default()
        },
    )
    .await;

    let (status, _stream) = connect_via(proxy, "origin.test:443").await;
    assert!(status.starts_with("HTTP/1.1 502"), "{status}");
    assert_eq!(server.seen()[0].outcome, Outcome::AuthRejected);

    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}

#[tokio::test]
async fn a_loopback_tunnel_does_not_use_the_socks5_proxy() {
    use kompressor_core::config::NetworkConfig;
    use kompressor_core::network::testing::{SocksServer, http_origin};
    let origin = http_origin("direct");
    let server = SocksServer::open();
    let (proxy, stop, ca_dir) = tunnel_only(
        "socks-loopback",
        NetworkConfig {
            socks5: server.url(),
            ..NetworkConfig::default()
        },
    )
    .await;

    let (status, mut stream) = connect_via(proxy, &format!("127.0.0.1:{origin}")).await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    assert!(reply.ends_with("direct"), "{reply}");
    assert!(server.seen().is_empty(), "{:?}", server.seen());

    let _ = stop.send(());
    let _ = std::fs::remove_dir_all(&ca_dir);
}
