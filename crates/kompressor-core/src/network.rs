//! How kompressor reaches the internet: directly, or through a SOCKS5 proxy.
//!
//! Every connection the service opens on its own account goes through one
//! [`Outbound`]: the HTTP client that forwards to the providers, the
//! websocket relay, the intercept listener's blind tunnels and the S3
//! uploads. On a network whose only way out is a SOCKS5 proxy, one setting
//! (`[network] socks5`) is then the difference between all of them working
//! and some of them silently trying a direct route that is not there.
//!
//! Loopback is never proxied: a SOCKS5 proxy's `127.0.0.1` is the proxy's own
//! machine, so a local upstream (a model server, a test double) sent through
//! it would reach the wrong host or nothing.
//!
//! The SOCKS5 client here is RFC 1928 with RFC 1929 username/password
//! authentication — CONNECT only, which is all a TCP stream needs. It is not
//! borrowed from the HTTP client's own (which is internal to it) so the
//! websocket relay and the tunnels, which need a bare TCP stream, use the
//! same handshake and report the same errors.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::ConfigError;

/// A SOCKS5 server and an HTTP origin for tests in this and other crates
/// (feature `test-support`).
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod testing;

/// The `[network]` section.
///
/// ```toml
/// [network]
/// socks5 = "socks5h://proxy.internal:1080"
/// socks5_username = "kompressor"
/// socks5_password = "…"            # or KOMPRESSOR__NETWORK__SOCKS5_PASSWORD
/// no_proxy = ["minio.internal", "10.0.0.0/8"]
/// ```
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// A SOCKS5 proxy every outbound connection goes through. Empty, the
    /// default, connects directly.
    ///
    /// `socks5h://host:port` has the proxy resolve host names — what a network
    /// with no DNS of its own needs, and what keeps the names out of local
    /// DNS. `socks5://host:port` resolves them here and hands the proxy an
    /// address. The port defaults to 1080. Credentials may be written into
    /// the URL (`socks5h://user:pass@host:1080`, percent-encoded), but the two
    /// settings below are easier to keep out of a file.
    pub socks5: String,
    /// The username for the proxy's username/password authentication
    /// (RFC 1929). Overrides one written into `socks5`.
    pub socks5_username: String,
    /// Its password. Replaced by `<redacted>` wherever the configuration is
    /// shown.
    pub socks5_password: String,
    /// Hosts reached directly even with `socks5` set: a name (which also
    /// covers its subdomains; a leading dot is the same thing), an IP address,
    /// a CIDR block (`10.0.0.0/8`), or `*` for everything. Loopback is always
    /// direct and need not be listed.
    pub no_proxy: Vec<String>,
}

impl fmt::Debug for NetworkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NetworkConfig")
            .field("socks5", &redact_url_password(&self.socks5))
            .field("socks5_username", &self.socks5_username)
            .field(
                "socks5_password",
                &if self.socks5_password.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

/// `socks5h://user:secret@host` with the secret replaced, for showing a
/// configured URL. Anything that does not parse is shown unchanged: it holds
/// no password this could find either.
pub fn redact_url_password(value: &str) -> String {
    match url::Url::parse(value) {
        Ok(mut url) if url.password().is_some() => {
            // Set as the URL's own password it would be percent-encoded
            // (`%3Credacted%3E`), so it goes in by hand. The username is
            // percent-encoded, so the first `@` after the scheme ends it.
            let _ = url.set_password(None);
            let shown = url.to_string();
            match shown.split_once("://") {
                Some((scheme, rest)) => match rest.split_once('@') {
                    Some((user, host)) => format!("{scheme}://{user}:<redacted>@{host}"),
                    None => shown,
                },
                None => shown,
            }
        }
        _ => value.to_string(),
    }
}

impl NetworkConfig {
    /// The route this configuration describes, or what is wrong with it.
    pub fn outbound(&self) -> Result<Outbound, ConfigError> {
        let bypass = self
            .no_proxy
            .iter()
            .map(|entry| Bypass::parse(entry))
            .collect::<Result<Vec<_>, _>>()?;
        let value = self.socks5.trim();
        if value.is_empty() {
            if !self.socks5_username.is_empty() || !self.socks5_password.is_empty() {
                return Err(invalid(
                    "network.socks5_username or socks5_password is set but network.socks5 is empty",
                ));
            }
            return Ok(Outbound::direct());
        }
        let url = url::Url::parse(value)
            .map_err(|error| invalid(&format!("network.socks5 is not a URL: {error}")))?;
        let remote_dns = match url.scheme() {
            "socks5h" => true,
            "socks5" => false,
            other => {
                return Err(invalid(&format!(
                    "network.socks5 must start socks5:// or socks5h://, not {other}://"
                )));
            }
        };
        let host = match url.host() {
            Some(url::Host::Domain(name)) => name.to_string(),
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => ip.to_string(),
            None => return Err(invalid("network.socks5 names no host")),
        };
        if !url.path().trim_matches('/').is_empty() || url.query().is_some() {
            return Err(invalid(
                "network.socks5 is a proxy address (socks5h://host:port), not a path",
            ));
        }
        let decode = |part: &str| {
            String::from_utf8(percent_decode(part))
                .map_err(|_| invalid("network.socks5 has credentials that are not UTF-8"))
        };
        let mut username = decode(url.username())?;
        let mut password = decode(url.password().unwrap_or(""))?;
        if !self.socks5_username.is_empty() {
            username = self.socks5_username.clone();
        }
        if !self.socks5_password.is_empty() {
            password = self.socks5_password.clone();
        }
        let auth = match (username.is_empty(), password.is_empty()) {
            (true, true) => None,
            (true, false) => {
                return Err(invalid(
                    "network.socks5 has a password but no username (set network.socks5_username)",
                ));
            }
            // RFC 1929 allows 1–255 bytes of each. An empty password is
            // outside the letter of it but some servers expect exactly that.
            _ => {
                if username.len() > 255 || password.len() > 255 {
                    return Err(invalid(
                        "network.socks5 username and password must each be at most 255 bytes",
                    ));
                }
                Some((username, password))
            }
        };
        Ok(Outbound {
            socks5: Some(Arc::new(Socks5 {
                host,
                port: url.port().unwrap_or(1080),
                remote_dns,
                auth,
            })),
            bypass: Arc::new(bypass),
        })
    }
}

/// `%XX` escapes decoded; anything that is not one is kept as it is.
fn percent_decode(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn invalid(message: &str) -> ConfigError {
    ConfigError::Invalid(message.to_string())
}

/// Where outbound connections go. Cheap to clone; `Default` is direct.
#[derive(Clone, Default)]
pub struct Outbound {
    socks5: Option<Arc<Socks5>>,
    bypass: Arc<Vec<Bypass>>,
}

struct Socks5 {
    host: String,
    port: u16,
    remote_dns: bool,
    auth: Option<(String, String)>,
}

impl fmt::Debug for Outbound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.describe() {
            Some(route) => write!(f, "Outbound({route})"),
            None => write!(f, "Outbound(direct)"),
        }
    }
}

/// How long reaching the proxy and completing the handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

impl Outbound {
    /// Every connection direct.
    pub fn direct() -> Outbound {
        Outbound::default()
    }

    /// Whether a proxy is configured at all.
    pub fn is_direct(&self) -> bool {
        self.socks5.is_none()
    }

    /// The proxy, for logs and status pages: scheme, address and whether it
    /// authenticates — never the password.
    pub fn describe(&self) -> Option<String> {
        let proxy = self.socks5.as_ref()?;
        let scheme = if proxy.remote_dns {
            "socks5h"
        } else {
            "socks5"
        };
        let auth = match &proxy.auth {
            Some((user, _)) => format!(" as {user}"),
            None => String::new(),
        };
        Some(format!(
            "{scheme}://{}{auth}",
            host_port(&proxy.host, proxy.port)
        ))
    }

    /// Whether a connection to `host` goes through the proxy.
    ///
    /// `host` as it appears in a URL: a name, an IPv4 address, or an IPv6
    /// address with or without its brackets.
    pub fn proxies(&self, host: &str) -> bool {
        if self.socks5.is_none() {
            return false;
        }
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if is_loopback(&host) {
            return false;
        }
        !self.bypass.iter().any(|rule| rule.matches(&host))
    }

    /// Open a TCP stream to `host:port` the way this route says to: through
    /// the proxy, or — for a direct route or a bypassed host — `None`, so the
    /// caller dials the way it already does.
    pub async fn connect(&self, host: &str, port: u16) -> Option<io::Result<TcpStream>> {
        if !self.proxies(host) {
            return None;
        }
        let proxy = self.socks5.as_ref()?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let attempt = async {
            let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
                .await
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "could not reach the SOCKS5 proxy at {}: {error}",
                            host_port(&proxy.host, proxy.port)
                        ),
                    )
                })?;
            handshake(&mut stream, proxy, host, port).await?;
            Ok(stream)
        };
        Some(
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, attempt).await {
                Ok(result) => result,
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "the SOCKS5 proxy at {} did not connect to {} within {}s",
                        host_port(&proxy.host, proxy.port),
                        host_port(host, port),
                        HANDSHAKE_TIMEOUT.as_secs()
                    ),
                )),
            },
        )
    }

    /// The proxy as reqwest wants it, with the bypass list attached; `None`
    /// for a direct route.
    fn reqwest_proxy(&self) -> Result<Option<reqwest::Proxy>, reqwest::Error> {
        let Some(proxy) = self.socks5.as_ref() else {
            return Ok(None);
        };
        let scheme = if proxy.remote_dns {
            "socks5h"
        } else {
            "socks5"
        };
        let mut built =
            reqwest::Proxy::all(format!("{scheme}://{}", host_port(&proxy.host, proxy.port)))?;
        if let Some((user, pass)) = &proxy.auth {
            built = built.basic_auth(user, pass);
        }
        // reqwest's NO_PROXY grammar is the one `no_proxy` documents, so the
        // list goes over as written, with loopback added.
        let mut bypass: Vec<String> = vec!["localhost".into(), "127.0.0.0/8".into(), "::1".into()];
        bypass.extend(self.bypass.iter().map(|rule| rule.source.clone()));
        Ok(Some(built.no_proxy(reqwest::NoProxy::from_string(
            &bypass.join(","),
        ))))
    }

    /// Route an async HTTP client's connections this way.
    ///
    /// With a proxy configured, the environment's own proxy variables
    /// (`HTTPS_PROXY`, `ALL_PROXY`, …) are no longer consulted: the
    /// configuration says where traffic goes, and two answers would be one
    /// too many. With none, the builder is left as it was.
    pub fn apply(
        &self,
        builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::ClientBuilder, reqwest::Error> {
        Ok(match self.reqwest_proxy()? {
            Some(proxy) => builder.proxy(proxy),
            None => builder,
        })
    }

    /// [`Outbound::apply`], for a blocking client.
    pub fn apply_blocking(
        &self,
        builder: reqwest::blocking::ClientBuilder,
    ) -> Result<reqwest::blocking::ClientBuilder, reqwest::Error> {
        Ok(match self.reqwest_proxy()? {
            Some(proxy) => builder.proxy(proxy),
            None => builder,
        })
    }
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn is_loopback(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// One `no_proxy` entry.
struct Bypass {
    source: String,
    rule: Rule,
}

enum Rule {
    Everything,
    Network(IpAddr, u8),
    Domain(String),
}

impl Bypass {
    fn parse(entry: &str) -> Result<Bypass, ConfigError> {
        let source = entry.trim().to_string();
        let lower = source.to_ascii_lowercase();
        let rule = if lower == "*" {
            Rule::Everything
        } else if let Some((address, bits)) = lower.split_once('/') {
            let ip: IpAddr = address.trim_matches(['[', ']']).parse().map_err(|_| {
                invalid(&format!(
                    "network.no_proxy entry {source:?} is not a CIDR block"
                ))
            })?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let bits: u8 = bits
                .parse()
                .ok()
                .filter(|bits| *bits <= max)
                .ok_or_else(|| {
                    invalid(&format!(
                        "network.no_proxy entry {source:?} has a bad prefix length"
                    ))
                })?;
            Rule::Network(ip, bits)
        } else if let Ok(ip) = lower.trim_matches(['[', ']']).parse::<IpAddr>() {
            Rule::Network(ip, if ip.is_ipv4() { 32 } else { 128 })
        } else {
            let domain = lower.trim_start_matches('.').trim_end_matches('.');
            if domain.is_empty() || domain.contains(['/', ':', ' ']) {
                return Err(invalid(&format!(
                    "network.no_proxy entry {source:?} is not a host, address or CIDR block"
                )));
            }
            Rule::Domain(domain.to_string())
        };
        Ok(Bypass { source, rule })
    }

    /// `host` is already lowercase, unbracketed and without a trailing dot.
    fn matches(&self, host: &str) -> bool {
        match &self.rule {
            Rule::Everything => true,
            Rule::Domain(domain) => {
                host == domain
                    || host
                        .strip_suffix(domain.as_str())
                        .is_some_and(|rest| rest.ends_with('.'))
            }
            Rule::Network(network, bits) => match (host.parse::<IpAddr>(), network) {
                (Ok(IpAddr::V4(ip)), IpAddr::V4(net)) => {
                    prefix_equal(&ip.octets(), &net.octets(), *bits)
                }
                (Ok(IpAddr::V6(ip)), IpAddr::V6(net)) => {
                    prefix_equal(&ip.octets(), &net.octets(), *bits)
                }
                _ => false,
            },
        }
    }
}

fn prefix_equal(a: &[u8], b: &[u8], bits: u8) -> bool {
    let whole = usize::from(bits / 8);
    if a[..whole] != b[..whole] {
        return false;
    }
    let rest = bits % 8;
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    a[whole] & mask == b[whole] & mask
}

/// The SOCKS5 exchange on an open connection to the proxy, ending with the
/// stream connected through to `host:port`.
async fn handshake(
    stream: &mut TcpStream,
    proxy: &Socks5,
    host: &str,
    port: u16,
) -> io::Result<()> {
    // Greeting: offer no-auth, and username/password when there are any. A
    // proxy that wants credentials we do not have answers 0xFF.
    let greeting: &[u8] = if proxy.auth.is_some() {
        &[5, 2, 0x00, 0x02]
    } else {
        &[5, 1, 0x00]
    };
    stream.write_all(greeting).await?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    if choice[0] != 5 {
        return Err(protocol("the proxy did not answer as SOCKS5"));
    }
    match (choice[1], &proxy.auth) {
        (0x00, _) => {}
        (0x02, Some((user, pass))) => {
            let mut request = Vec::with_capacity(3 + user.len() + pass.len());
            request.push(1);
            request.push(user.len() as u8);
            request.extend_from_slice(user.as_bytes());
            request.push(pass.len() as u8);
            request.extend_from_slice(pass.as_bytes());
            stream.write_all(&request).await?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await?;
            if status[1] != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "the SOCKS5 proxy rejected the username and password",
                ));
            }
        }
        (0x02, None) | (0xff, None) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the SOCKS5 proxy requires a username and password \
                 (set network.socks5_username and network.socks5_password)",
            ));
        }
        (0xff, Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the SOCKS5 proxy accepted none of the offered authentication methods",
            ));
        }
        (other, _) => {
            return Err(protocol(&format!(
                "the proxy chose authentication method {other:#04x}, which was not offered"
            )));
        }
    }

    // CONNECT.
    let mut request = vec![5, 1, 0];
    let address = match host.parse::<IpAddr>() {
        Ok(ip) => Some(ip),
        Err(_) if proxy.remote_dns => None,
        Err(_) => Some(
            tokio::net::lookup_host((host, port))
                .await?
                .next()
                .map(|found: SocketAddr| found.ip())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"))
                })?,
        ),
    };
    match address {
        Some(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Some(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        None => {
            if host.len() > 255 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a host name longer than 255 bytes cannot be sent to a SOCKS5 proxy",
                ));
            }
            request.push(3);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(protocol("the proxy's reply to CONNECT was not SOCKS5"));
    }
    if head[1] != 0 {
        let (kind, why) = match head[1] {
            0x02 => (
                io::ErrorKind::PermissionDenied,
                "not allowed by the proxy's rules",
            ),
            0x03 => (io::ErrorKind::NetworkUnreachable, "network unreachable"),
            0x04 => (io::ErrorKind::HostUnreachable, "host unreachable"),
            0x05 => (io::ErrorKind::ConnectionRefused, "connection refused"),
            0x06 => (io::ErrorKind::TimedOut, "TTL expired"),
            0x07 => (io::ErrorKind::Unsupported, "command not supported"),
            0x08 => (io::ErrorKind::Unsupported, "address type not supported"),
            _ => (io::ErrorKind::Other, "general failure"),
        };
        return Err(io::Error::new(
            kind,
            format!(
                "the SOCKS5 proxy could not connect to {}: {why}",
                host_port(host, port)
            ),
        ));
    }
    // The address the proxy bound, which nothing here needs; read past it.
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            usize::from(len[0])
        }
        _ => return Err(protocol("the proxy's reply has an unknown address type")),
    };
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).await?;
    Ok(())
}

fn protocol(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[cfg(test)]
mod tests {
    use super::testing::{Outcome, SocksServer, closed_port, http_origin};
    use super::*;
    use tokio::net::TcpListener;

    fn network(socks5: &str) -> NetworkConfig {
        NetworkConfig {
            socks5: socks5.into(),
            ..NetworkConfig::default()
        }
    }

    #[test]
    fn nothing_configured_is_direct() {
        let route = NetworkConfig::default().outbound().unwrap();
        assert!(route.is_direct());
        assert!(!route.proxies("api.anthropic.com"));
        assert_eq!(route.describe(), None);
    }

    #[test]
    fn the_scheme_says_who_resolves_names() {
        let remote = network("socks5h://proxy:1081").outbound().unwrap();
        assert_eq!(remote.describe().unwrap(), "socks5h://proxy:1081");
        let local = network("socks5://10.0.0.1").outbound().unwrap();
        assert_eq!(local.describe().unwrap(), "socks5://10.0.0.1:1080");
    }

    #[test]
    fn credentials_come_from_the_url_or_the_settings_and_never_show() {
        let inline = network("socks5h://al%40ice:p%3Ass@proxy:1080")
            .outbound()
            .unwrap();
        let proxy = inline.socks5.as_ref().unwrap();
        assert_eq!(proxy.auth, Some(("al@ice".to_string(), "p:ss".to_string())));
        assert_eq!(inline.describe().unwrap(), "socks5h://proxy:1080 as al@ice");
        assert!(!format!("{inline:?}").contains("p:ss"));

        let config = NetworkConfig {
            socks5: "socks5h://old:old@proxy".into(),
            socks5_username: "bob".into(),
            socks5_password: "hunter2".into(),
            no_proxy: vec![],
        };
        let route = config.outbound().unwrap();
        assert_eq!(
            route.socks5.as_ref().unwrap().auth,
            Some(("bob".to_string(), "hunter2".to_string()))
        );
        assert!(!format!("{config:?}").contains("hunter2"));
        assert!(!format!("{config:?}").contains(":old@"));
    }

    #[test]
    fn bad_settings_are_refused() {
        for bad in [
            "http://proxy:3128",
            "socks4://proxy",
            "proxy:1080",
            "socks5h://proxy:1080/path",
            "socks5h://:pass@proxy",
        ] {
            assert!(network(bad).outbound().is_err(), "{bad}");
        }
        let orphan = NetworkConfig {
            socks5_password: "x".into(),
            ..NetworkConfig::default()
        };
        assert!(orphan.outbound().is_err());
        let bad_bypass = NetworkConfig {
            socks5: "socks5h://proxy".into(),
            no_proxy: vec!["10.0.0.0/40".into()],
            ..NetworkConfig::default()
        };
        assert!(bad_bypass.outbound().is_err());
    }

    #[test]
    fn loopback_and_listed_hosts_go_direct() {
        let config = NetworkConfig {
            socks5: "socks5h://proxy".into(),
            no_proxy: vec![
                ".internal".into(),
                "minio.example.com".into(),
                "10.0.0.0/8".into(),
                "fd00::/8".into(),
                "192.168.1.7".into(),
            ],
            ..NetworkConfig::default()
        };
        let route = config.outbound().unwrap();
        for direct in [
            "localhost",
            "app.localhost",
            "127.0.0.1",
            "127.8.9.10",
            "::1",
            "[::1]",
            "internal",
            "s3.internal",
            "MinIO.Example.Com",
            "eu.minio.example.com",
            "10.20.30.40",
            "[fd12::1]",
            "192.168.1.7",
        ] {
            assert!(!route.proxies(direct), "{direct} should be direct");
        }
        for proxied in [
            "api.anthropic.com",
            "notinternal",
            "example.com",
            "11.0.0.1",
            "192.168.1.8",
            "2001:db8::1",
        ] {
            assert!(route.proxies(proxied), "{proxied} should be proxied");
        }
        let everything = NetworkConfig {
            socks5: "socks5h://proxy".into(),
            no_proxy: vec!["*".into()],
            ..NetworkConfig::default()
        };
        assert!(!everything.outbound().unwrap().proxies("api.anthropic.com"));
    }

    #[test]
    fn bypass_entries_that_are_not_hosts_are_refused() {
        for bad in ["", "a/b", "host:80", "10.0.0.1/x", "::1/129", "two words"] {
            let config = NetworkConfig {
                socks5: "socks5h://proxy".into(),
                no_proxy: vec![bad.into()],
                ..NetworkConfig::default()
            };
            assert!(config.outbound().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn credentials_longer_than_rfc_1929_allows_are_refused() {
        let config = NetworkConfig {
            socks5: "socks5h://proxy".into(),
            socks5_username: "u".repeat(256),
            socks5_password: "p".into(),
            no_proxy: vec![],
        };
        assert!(config.outbound().is_err());
    }

    #[test]
    fn an_ipv6_proxy_is_written_with_brackets() {
        let route = network("socks5h://[2001:db8::1]:1081").outbound().unwrap();
        assert_eq!(route.describe().unwrap(), "socks5h://[2001:db8::1]:1081");
        assert_eq!(host_port("::1", 80), "[::1]:80");
        assert_eq!(host_port("example.com", 80), "example.com:80");
    }

    #[test]
    fn percent_escapes_decode_and_anything_else_is_kept() {
        assert_eq!(percent_decode("a%40b%3a"), b"a@b:");
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("%zz%4"), b"%zz%4");
    }

    #[test]
    fn a_password_in_a_url_is_redacted_for_display() {
        assert_eq!(
            redact_url_password("socks5h://u:secret@proxy:1080"),
            "socks5h://u:<redacted>@proxy:1080"
        );
        assert_eq!(redact_url_password("socks5h://proxy"), "socks5h://proxy");
        assert_eq!(redact_url_password(""), "");
    }

    fn through(server: &SocksServer) -> Outbound {
        network(&server.url()).outbound().unwrap()
    }

    #[tokio::test]
    async fn connects_through_the_proxy_with_a_password_and_remote_dns() {
        let origin = http_origin("hello through socks");
        let server = SocksServer::with_password("alice", "s3cret");
        let route = NetworkConfig {
            socks5: server.url(),
            socks5_username: "alice".into(),
            socks5_password: "s3cret".into(),
            no_proxy: vec![],
        }
        .outbound()
        .unwrap();
        let mut stream = route.connect("origin.test", origin).await.unwrap().unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: origin.test\r\n\r\n")
            .await
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert!(reply.ends_with("hello through socks"), "{reply}");
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].methods, vec![0, 2], "no-auth and username/password");
        assert_eq!(
            seen[0].credentials,
            Some(("alice".to_string(), "s3cret".to_string()))
        );
        // The name, not an address: the proxy resolved it.
        assert_eq!(seen[0].target, format!("origin.test:{origin}"));
    }

    #[tokio::test]
    async fn without_credentials_only_no_auth_is_offered() {
        let origin = http_origin("ok");
        let server = SocksServer::open();
        let mut stream = through(&server)
            .connect("origin.test", origin)
            .await
            .unwrap()
            .unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert!(reply.ends_with("ok"), "{reply}");
        assert_eq!(server.seen()[0].methods, vec![0]);
        assert_eq!(server.seen()[0].credentials, None);
    }

    #[tokio::test]
    async fn socks5_without_h_resolves_here_and_sends_an_address() {
        // The handshake itself, since `connect` never proxies a name that
        // resolves on this machine without a resolver of its own: loopback.
        let server = SocksServer::open();
        let proxy = Socks5 {
            host: "127.0.0.1".into(),
            port: server
                .address()
                .rsplit(':')
                .next()
                .unwrap()
                .parse()
                .unwrap(),
            remote_dns: false,
            auth: None,
        };
        let origin = http_origin("ok");
        let mut stream = TcpStream::connect(server.address()).await.unwrap();
        // "localhost" may resolve to ::1 first, where the origin is not
        // listening; either way what went over the wire is an address.
        let _ = handshake(&mut stream, &proxy, "localhost", origin).await;
        let target = &server.seen()[0].target;
        let host = target.rsplit_once(':').unwrap().0.trim_matches(['[', ']']);
        assert!(
            host.parse::<IpAddr>().is_ok(),
            "sent {target}, not an address"
        );
    }

    #[tokio::test]
    async fn a_wrong_password_says_so() {
        let server = SocksServer::with_password("alice", "s3cret");
        let route = network(&format!("socks5h://alice:wrong@{}", server.address()))
            .outbound()
            .unwrap();
        let error = route
            .connect("origin.test", 443)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("rejected the username"),
            "{error}"
        );
        assert_eq!(server.seen()[0].outcome, Outcome::AuthRejected);
    }

    #[tokio::test]
    async fn a_proxy_that_wants_credentials_we_lack_says_so() {
        let server = SocksServer::with_password("alice", "s3cret");
        let error = through(&server)
            .connect("origin.test", 443)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("requires a username"), "{error}");
        assert_eq!(server.seen()[0].methods, vec![0]);
    }

    #[tokio::test]
    async fn an_unreachable_origin_is_reported_from_the_proxy() {
        let server = SocksServer::open();
        let port = closed_port();
        let error = through(&server)
            .connect("origin.test", port)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert!(
            error
                .to_string()
                .contains(&format!("could not connect to origin.test:{port}")),
            "{error}"
        );
        assert_eq!(server.seen()[0].outcome, Outcome::Unreachable);
    }

    #[tokio::test]
    async fn an_unreachable_proxy_is_an_error_not_a_direct_connection() {
        let origin = http_origin("ok");
        let route = network(&format!("socks5h://127.0.0.1:{}", closed_port()))
            .outbound()
            .unwrap();
        // An address that is not loopback but that this machine can reach
        // directly would show a fallback; there is none, so the error is
        // what is asserted: it names the proxy, not the origin.
        let error = route
            .connect("origin.test", origin)
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("could not reach the SOCKS5 proxy"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn something_that_is_not_a_socks5_proxy_is_reported_as_such() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        });
        let error = network(&format!("socks5h://127.0.0.1:{port}"))
            .outbound()
            .unwrap()
            .connect("origin.test", 80)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains("not answer as SOCKS5"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn loopback_is_left_to_the_caller() {
        let route = network("socks5h://127.0.0.1:9").outbound().unwrap();
        assert!(route.connect("127.0.0.1", 80).await.is_none());
        assert!(route.connect("localhost", 80).await.is_none());
        assert!(route.connect("[::1]", 80).await.is_none());
        assert!(
            Outbound::direct()
                .connect("example.com", 80)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_http_client_goes_through_the_proxy_with_its_password() {
        let origin = http_origin("via reqwest");
        let server = SocksServer::with_password("alice", "s3cret");
        let route = NetworkConfig {
            socks5: server.url(),
            socks5_username: "alice".into(),
            socks5_password: "s3cret".into(),
            no_proxy: vec![],
        }
        .outbound()
        .unwrap();
        let client = route
            .apply(reqwest::Client::builder())
            .unwrap()
            .build()
            .unwrap();
        let body = client
            .get(format!("http://origin.test:{origin}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "via reqwest");
        assert_eq!(server.connected(), [format!("origin.test:{origin}")]);
        assert_eq!(
            server.seen()[0].credentials,
            Some(("alice".to_string(), "s3cret".to_string()))
        );
    }

    #[tokio::test]
    async fn an_http_client_reaches_loopback_directly() {
        let origin = http_origin("direct");
        let server = SocksServer::open();
        let client = through(&server)
            .apply(reqwest::Client::builder())
            .unwrap()
            .build()
            .unwrap();
        for host in ["127.0.0.1", "localhost"] {
            let body = client
                .get(format!("http://{host}:{origin}/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert_eq!(body, "direct");
        }
        assert!(server.seen().is_empty(), "{:?}", server.seen());
    }

    #[tokio::test]
    async fn an_http_client_with_a_wrong_password_fails_rather_than_going_direct() {
        let origin = http_origin("should not be reached");
        let server = SocksServer::with_password("alice", "s3cret");
        let route = network(&format!("socks5h://alice:wrong@{}", server.address()))
            .outbound()
            .unwrap();
        let client = route
            .apply(reqwest::Client::builder())
            .unwrap()
            .build()
            .unwrap();
        let error = client
            .get(format!("http://origin.test:{origin}/"))
            .send()
            .await
            .unwrap_err();
        assert!(error.is_connect(), "{error:?}");
        assert_eq!(server.seen()[0].outcome, Outcome::AuthRejected);
    }

    #[test]
    fn a_blocking_http_client_goes_through_the_proxy() {
        let origin = http_origin("via blocking");
        let server = SocksServer::with_password("alice", "s3cret");
        let route = network(&format!("socks5h://alice:s3cret@{}", server.address()))
            .outbound()
            .unwrap();
        let client = route
            .apply_blocking(reqwest::blocking::Client::builder())
            .unwrap()
            .build()
            .unwrap();
        let body = client
            .get(format!("http://origin.test:{origin}/"))
            .send()
            .unwrap()
            .text()
            .unwrap();
        assert_eq!(body, "via blocking");
        assert_eq!(server.connected(), [format!("origin.test:{origin}")]);
    }

    #[test]
    fn a_direct_route_leaves_a_client_builder_alone() {
        Outbound::direct()
            .apply(reqwest::Client::builder())
            .unwrap()
            .build()
            .unwrap();
        Outbound::direct()
            .apply_blocking(reqwest::blocking::Client::builder())
            .unwrap()
            .build()
            .unwrap();
    }
}
