//! A SOCKS5 server and an HTTP origin for tests, on real sockets.
//!
//! The server speaks RFC 1928 CONNECT with optional RFC 1929
//! username/password authentication, and resolves every host name ending in
//! `.test` to `127.0.0.1`. Nothing else can resolve those names, so a test
//! that reaches `origin.test` proves the connection went through the proxy
//! *and* that the proxy, not this machine, resolved the name. Loopback
//! targets are never sent to a proxy (see [`super::Outbound::proxies`]), which
//! is why tests name `*.test` hosts rather than `127.0.0.1`.
//!
//! Each runs on a thread of its own with its own runtime, so a blocking test
//! (the S3 client) and an async one use it the same way. The threads live for
//! the rest of the test process.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How one connection to the test proxy ended its handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Connected through to the target.
    Connected,
    /// The client offered no method the server accepts.
    NoAcceptableMethod,
    /// The username or password was wrong.
    AuthRejected,
    /// The target refused or could not be reached; the client was told so.
    Unreachable,
}

/// What the test proxy saw of one connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The authentication methods the client offered.
    pub methods: Vec<u8>,
    /// The username and password sent, when there were any.
    pub credentials: Option<(String, String)>,
    /// `host:port` as the client asked for it: a name when the proxy was
    /// asked to resolve it, an address otherwise. Empty when the handshake
    /// ended before the request.
    pub target: String,
    pub outcome: Outcome,
}

/// A SOCKS5 proxy for tests. See the module documentation.
pub struct SocksServer {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl SocksServer {
    /// Start one that accepts anyone, without credentials.
    pub fn open() -> SocksServer {
        SocksServer::start(None)
    }

    /// Start one that demands `username` and `password`.
    pub fn with_password(username: &str, password: &str) -> SocksServer {
        SocksServer::start(Some((username.to_string(), password.to_string())))
    }

    fn start(credentials: Option<(String, String)>) -> SocksServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let record = seen.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).expect("listener");
                while let Ok((client, _)) = listener.accept().await {
                    let credentials = credentials.clone();
                    let record = record.clone();
                    tokio::spawn(async move {
                        let _ = serve(client, credentials, record).await;
                    });
                }
            });
        });
        SocksServer { port, seen }
    }

    /// `127.0.0.1:port`.
    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// `socks5h://127.0.0.1:port`: the proxy resolves names.
    pub fn url(&self) -> String {
        format!("socks5h://{}", self.address())
    }

    /// Every connection so far, in the order their handshakes ended. Each is
    /// recorded before the server answers, so a client that has its answer can
    /// read it here.
    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The targets of the connections that went through.
    pub fn connected(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter(|seen| seen.outcome == Outcome::Connected)
            .map(|seen| seen.target)
            .collect()
    }
}

async fn serve(
    mut client: TcpStream,
    credentials: Option<(String, String)>,
    record: Arc<Mutex<Vec<Seen>>>,
) -> std::io::Result<()> {
    let mut seen = Seen {
        methods: Vec::new(),
        credentials: None,
        target: String::new(),
        outcome: Outcome::NoAcceptableMethod,
    };
    let push = |seen: &Seen| {
        record
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(seen.clone())
    };

    let mut head = [0u8; 2];
    client.read_exact(&mut head).await?;
    let mut methods = vec![0u8; usize::from(head[1])];
    client.read_exact(&mut methods).await?;
    seen.methods = methods.clone();
    match &credentials {
        Some((user, pass)) => {
            if !methods.contains(&2) {
                push(&seen);
                client.write_all(&[5, 0xff]).await?;
                return Ok(());
            }
            client.write_all(&[5, 2]).await?;
            let mut version_len = [0u8; 2];
            client.read_exact(&mut version_len).await?;
            let mut got_user = vec![0u8; usize::from(version_len[1])];
            client.read_exact(&mut got_user).await?;
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut got_pass = vec![0u8; usize::from(len[0])];
            client.read_exact(&mut got_pass).await?;
            let got = (
                String::from_utf8_lossy(&got_user).into_owned(),
                String::from_utf8_lossy(&got_pass).into_owned(),
            );
            seen.credentials = Some(got.clone());
            if got != (user.clone(), pass.clone()) {
                seen.outcome = Outcome::AuthRejected;
                push(&seen);
                client.write_all(&[1, 1]).await?;
                return Ok(());
            }
            client.write_all(&[1, 0]).await?;
        }
        None => {
            if !methods.contains(&0) {
                push(&seen);
                client.write_all(&[5, 0xff]).await?;
                return Ok(());
            }
            client.write_all(&[5, 0]).await?;
        }
    }

    let mut request = [0u8; 4];
    client.read_exact(&mut request).await?;
    let host = match request[3] {
        1 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip).await?;
            Ipv4Addr::from(ip).to_string()
        }
        4 => {
            let mut ip = [0u8; 16];
            client.read_exact(&mut ip).await?;
            Ipv6Addr::from(ip).to_string()
        }
        _ => {
            let mut len = [0u8; 1];
            client.read_exact(&mut len).await?;
            let mut name = vec![0u8; usize::from(len[0])];
            client.read_exact(&mut name).await?;
            String::from_utf8_lossy(&name).into_owned()
        }
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);
    seen.target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let resolved: IpAddr = if host.ends_with(".test") {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        match host.parse() {
            Ok(ip) => ip,
            Err(_) => {
                seen.outcome = Outcome::Unreachable;
                push(&seen);
                client.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                return Ok(());
            }
        }
    };
    match TcpStream::connect((resolved, port)).await {
        Ok(mut upstream) => {
            seen.outcome = Outcome::Connected;
            push(&seen);
            client.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await?;
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        }
        Err(_) => {
            seen.outcome = Outcome::Unreachable;
            push(&seen);
            client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        }
    }
    Ok(())
}

/// An HTTP/1.1 server on `127.0.0.1` that answers every request with `200`
/// and `body`, and closes the connection. Returns its port.
pub fn http_origin(body: &'static str) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                // Read the head; the requests sent here carry no body.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    port
}

/// A port nothing listens on (bound, then released).
pub fn closed_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    listener.local_addr().expect("addr").port()
}
