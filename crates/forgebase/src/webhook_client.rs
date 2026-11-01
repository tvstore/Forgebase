//! The HTTP side of outgoing webhooks (M9.2, ADR 0020): the URL rules, the SSRF guard and the
//! signed POST.
//!
//! The rules, in the order they are applied to a delivery:
//!
//! 1. **The URL** ([`parse_url`]): `https`, or `http` only for a webhook added with
//!    `--insecure-http`; a host; no user info (`user:pass@`).
//! 2. **Resolve, then vet** ([`vet`]): the host is resolved here, by us, and **every** address is
//!    checked against [`is_blocked`]. One blocked address refuses the delivery, unless the
//!    webhook has `allow_private`.
//! 3. **Pin** ([`Pinned`]): the connector gets only the vetted addresses and never resolves the
//!    name again, so a DNS answer that changes between the check and the connection (DNS
//!    rebinding) cannot send the request elsewhere. TLS still uses the host name of the URL for
//!    the SNI and for verifying the certificate.
//! 4. **Send**: HTTP/1.1, one request on one fresh connection, no redirect followed, a total
//!    time limit ([`TIMEOUT`]) that covers the DNS lookup, the connection, TLS, the request and
//!    the response, and a response body that is read up to [`BODY_LIMIT`] and thrown away.
//!
//! Nothing here logs or returns a secret, a signature or a URL path or query: a URL often
//! carries a token (Slack style), so messages name only the scheme, the host and the port
//! ([`Target::label`]).

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, Uri, header};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::BodyExt;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::TokioExecutor;
use sha2::Sha256;
use tower::Service;

/// The longest a delivery may take, in total.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);

/// How much of a response body is read before the connection is dropped.
pub(crate) const BODY_LIMIT: usize = 64 * 1024;

/// The longest URL that is accepted.
const MAX_URL_BYTES: usize = 2048;

/// The `User-Agent` of deliveries.
pub(crate) fn user_agent() -> String {
    format!("Forgebase-Webhook/{}", env!("CARGO_PKG_VERSION"))
}

// ---- the blocklist ------------------------------------------------------------------------------

/// Whether an address is one a webhook must not reach (without `allow_private`).
///
/// | Family | Blocked |
/// |---|---|
/// | IPv4 | `0.0.0.0/8`, `10.0.0.0/8`, `100.64.0.0/10`, `127.0.0.0/8`, `169.254.0.0/16` (link-local, with the cloud metadata address `169.254.169.254`), `172.16.0.0/12`, `192.0.0.0/24`, `192.168.0.0/16`, `198.18.0.0/15`, `224.0.0.0/4` (multicast), `240.0.0.0/4` (reserved, with `255.255.255.255`); and, as an explicit extra, `168.63.129.16/32`, the Azure WireServer, which is reachable from Azure virtual machines and is a public address otherwise |
/// | IPv6 | `::/96` (this includes `::` and `::1`), `fc00::/7` (unique local), `fe80::/10` (link-local), `ff00::/8` (multicast); `::ffff:0:0/96` (IPv4-mapped) and `64:ff9b::/96` (the NAT64 well-known prefix) are checked as the IPv4 address they embed |
pub(crate) fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => blocked_v4(v4),
        IpAddr::V6(v6) => blocked_v6(v6),
    }
}

/// The Azure WireServer (host and platform services of an Azure virtual machine): not a private
/// range, so it needs its own entry. Also blocked as an IPv4-mapped or NAT64 address.
const AZURE_WIRESERVER: Ipv4Addr = Ipv4Addr::new(168, 63, 129, 16);

fn blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    a == 0 // 0.0.0.0/8
        || a == 10 // 10.0.0.0/8
        || (a == 100 && (64..=127).contains(&b)) // 100.64.0.0/10
        || a == 127 // 127.0.0.0/8
        || (a == 169 && b == 254) // 169.254.0.0/16
        || ip == AZURE_WIRESERVER // 168.63.129.16/32: reachable from Azure VMs, public otherwise
        || (a == 172 && (16..=31).contains(&b)) // 172.16.0.0/12
        || (a == 192 && b == 0 && c == 0) // 192.0.0.0/24
        || (a == 192 && b == 168) // 192.168.0.0/16
        || (a == 198 && (b == 18 || b == 19)) // 198.18.0.0/15
        || a >= 224 // 224.0.0.0/4 multicast, 240.0.0.0/4 reserved, broadcast
}

fn blocked_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // ::ffff:0:0/96, IPv4-mapped: the embedded IPv4 address decides.
    if s[..5] == [0; 5] && s[5] == 0xffff {
        return blocked_v4(embedded_v4(s[6], s[7]));
    }
    // 64:ff9b::/96, NAT64: the embedded IPv4 address decides.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4] {
        return blocked_v4(embedded_v4(s[6], s[7]));
    }
    s[..6] == [0; 6] // ::/96: the unspecified and loopback addresses and the old IPv4-compatible form
        || (s[0] & 0xfe00) == 0xfc00 // fc00::/7
        || (s[0] & 0xffc0) == 0xfe80 // fe80::/10
        || (s[0] & 0xff00) == 0xff00 // ff00::/8
}

fn embedded_v4(high: u16, low: u16) -> Ipv4Addr {
    let [a, b] = high.to_be_bytes();
    let [c, d] = low.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

// ---- the URL ------------------------------------------------------------------------------------

/// A webhook URL that passed the rules.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    uri: Uri,
    /// The host without the brackets of an IPv6 literal.
    host: String,
    port: u16,
    https: bool,
}

impl Target {
    /// `scheme://host:port`, the only part of the URL that is logged or printed.
    pub(crate) fn label(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        if self.host.contains(':') {
            format!("{scheme}://[{}]:{}", self.host, self.port)
        } else {
            format!("{scheme}://{}:{}", self.host, self.port)
        }
    }
}

/// Checks a webhook URL. `insecure_http` allows `http://`. The error never contains the URL (it
/// may hold a token); it says what is wrong with it.
pub(crate) fn parse_url(url: &str, insecure_http: bool) -> Result<Target, String> {
    if url.len() > MAX_URL_BYTES {
        return Err(format!("the URL is longer than {MAX_URL_BYTES} bytes"));
    }
    let uri: Uri = url
        .parse()
        .map_err(|_| "the URL is not a valid absolute URL".to_owned())?;
    let https = match uri.scheme_str() {
        Some("https") => true,
        Some("http") if insecure_http => false,
        Some("http") => {
            return Err(
                "the URL uses http://; webhooks use https:// (for development only, add the \
                 webhook with --insecure-http)"
                    .to_owned(),
            );
        }
        Some(_) => return Err("the URL scheme must be https".to_owned()),
        None => return Err("the URL must start with https://".to_owned()),
    };
    let Some(authority) = uri.authority() else {
        return Err("the URL has no host".to_owned());
    };
    if authority.as_str().contains('@') {
        return Err(
            "the URL has user info (user:password@): credentials in a URL are logged and leaked; \
             put the token in the path or sign with the secret instead"
                .to_owned(),
        );
    }
    let host = uri.host().unwrap_or_default();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() {
        return Err("the URL has no host".to_owned());
    }
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    Ok(Target {
        host: host.to_owned(),
        port,
        https,
        uri,
    })
}

// ---- resolve, vet, pin --------------------------------------------------------------------------

/// Why a delivery was not sent or got no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The URL or the address is not allowed. Retrying cannot help.
    Refused(String),
    /// A network problem or a timeout. Retrying may help.
    Transport(String),
}

/// Resolves the host of `target` and checks every address. An IP literal is used as it is. With
/// `allow_private` nothing is blocked. The port of the returned addresses is the URL's.
pub(crate) async fn vet(target: &Target, allow_private: bool) -> Result<Vec<SocketAddr>, Failure> {
    let label = target.label();
    let addrs: Vec<SocketAddr> = if let Ok(ip) = target.host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, target.port)]
    } else {
        tokio::net::lookup_host((target.host.as_str(), target.port))
            .await
            .map_err(|_| Failure::Transport(format!("cannot resolve the host of {label}")))?
            .collect()
    };
    if addrs.is_empty() {
        return Err(Failure::Transport(format!(
            "the host of {label} has no address"
        )));
    }
    check_addresses(&label, &addrs, allow_private)?;
    Ok(addrs)
}

/// The refusal for a blocked address (the first one found), or `Ok`.
pub(crate) fn check_addresses(
    label: &str,
    addrs: &[SocketAddr],
    allow_private: bool,
) -> Result<(), Failure> {
    if allow_private {
        return Ok(());
    }
    match addrs.iter().find(|a| is_blocked(a.ip())) {
        Some(blocked) => Err(Failure::Refused(format!(
            "{label} resolves to {}, a loopback, private, link-local or reserved address; add the \
             webhook with --allow-private to allow it",
            blocked.ip()
        ))),
        None => Ok(()),
    }
}

/// A resolver that returns the addresses it was given and nothing else: the connector never
/// looks the name up itself (no DNS rebinding).
#[derive(Clone)]
pub(crate) struct Pinned(pub(crate) Arc<Vec<SocketAddr>>);

impl Service<Name> for Pinned {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _name: Name) -> Self::Future {
        std::future::ready(Ok(self.0.as_ref().clone().into_iter()))
    }
}

// ---- TLS and the test seam ----------------------------------------------------------------------

/// Settings a test replaces for the whole process: the roots it trusts and the time limit. Set
/// once by [`crate::testing::webhook_test_settings`]; environment variables are not used because
/// changing them in a running process is racy.
#[cfg(feature = "testing")]
pub(crate) struct TestSettings {
    pub(crate) tls: Arc<rustls::ClientConfig>,
    pub(crate) timeout: Duration,
}

#[cfg(feature = "testing")]
pub(crate) static TEST_SETTINGS: OnceLock<TestSettings> = OnceLock::new();

fn timeout() -> Duration {
    #[cfg(feature = "testing")]
    if let Some(settings) = TEST_SETTINGS.get() {
        return settings.timeout;
    }
    TIMEOUT
}

/// The TLS configuration: the operating system's roots, loaded once.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, Failure> {
    #[cfg(feature = "testing")]
    if let Some(settings) = TEST_SETTINGS.get() {
        return Ok(Arc::clone(&settings.tls));
    }
    static NATIVE: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    NATIVE
        .get_or_init(|| {
            forgebase_db::tls::native_client_config()
                .map(Arc::new)
                .map_err(|e| format!("cannot set up TLS for webhooks: {e}"))
        })
        .clone()
        .map_err(Failure::Transport)
}

// ---- signing ------------------------------------------------------------------------------------

/// `sha256=<lower-case hex of HMAC-SHA256(secret, "<timestamp>.<body>")>`.
pub(crate) fn signature(secret: &str, timestamp: u64, body: &[u8]) -> String {
    // HMAC accepts a key of any length, so this cannot fail.
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .expect("HMAC takes a key of any length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut text = String::from("sha256=");
    for byte in digest {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

// ---- the request --------------------------------------------------------------------------------

/// What one delivery sends.
pub(crate) struct Outgoing<'a> {
    pub(crate) url: &'a str,
    pub(crate) secret: &'a str,
    pub(crate) allow_private: bool,
    pub(crate) insecure_http: bool,
    pub(crate) event: &'a str,
    pub(crate) delivery: &'a str,
    pub(crate) body: Vec<u8>,
}

/// Sends one delivery: the checks in the order of the module documentation. Returns the HTTP
/// status of the answer, whatever it is; a refusal or a network problem is a [`Failure`]. The
/// whole call, the DNS lookup included, is limited to the time limit.
pub(crate) async fn send(out: Outgoing<'_>) -> Result<u16, Failure> {
    let target = parse_url(out.url, out.insecure_http).map_err(Failure::Refused)?;
    let limit = timeout();
    let label = target.label();
    let sent = async {
        let addrs = vet(&target, out.allow_private).await?;
        send_pinned(&target, addrs, &out).await
    };
    match tokio::time::timeout(limit, sent).await {
        Ok(result) => result,
        Err(_) => Err(Failure::Transport(format!(
            "{label} did not answer within {} s",
            limit.as_secs_f64()
        ))),
    }
}

/// Connects to `addrs` only (the name of the URL is used for the `Host` header and for TLS, never
/// resolved) and posts the delivery.
pub(crate) async fn send_pinned(
    target: &Target,
    addrs: Vec<SocketAddr>,
    out: &Outgoing<'_>,
) -> Result<u16, Failure> {
    let label = target.label();
    let transport = |what: &str| Failure::Transport(format!("{what} {label}"));

    let mut http = HttpConnector::new_with_resolver(Pinned(Arc::new(addrs)));
    // The HTTPS wrapper needs the plain connector to accept https:// URIs.
    http.enforce_http(false);
    let builder = HttpsConnectorBuilder::new().with_tls_config(
        // The roots are only needed for https.
        if target.https {
            (*tls_config()?).clone()
        } else {
            // Never used for an http:// URL; an empty root store is the safe value.
            forgebase_db::tls::client_config(rustls::RootCertStore::empty())
                .map_err(Failure::Transport)?
        },
    );
    let builder = if out.insecure_http {
        builder.https_or_http()
    } else {
        builder.https_only()
    };
    let connector = builder.enable_http1().wrap_connector(http);
    let client: Client<_, Body> = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .build(connector);

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let request = Request::builder()
        .method("POST")
        .uri(target.uri.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::USER_AGENT, user_agent())
        .header("X-Forgebase-Event", out.event)
        .header("X-Forgebase-Delivery", out.delivery)
        .header("X-Forgebase-Timestamp", timestamp.to_string())
        .header(
            "X-Forgebase-Signature",
            signature(out.secret, timestamp, &out.body),
        )
        .body(Body::from(out.body.clone()))
        .map_err(|_| Failure::Refused("the request cannot be built".to_owned()))?;

    let response = client
        .request(request)
        .await
        .map_err(|e| transport(&classify(&e)))?;
    let status = response.status().as_u16();
    // Read and discard at most BODY_LIMIT bytes; the connection is dropped with the client.
    let mut body = response.into_body();
    let mut read = 0usize;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { break };
        if let Some(data) = frame.data_ref() {
            read += data.len();
            if read >= BODY_LIMIT {
                break;
            }
        }
    }
    Ok(status)
}

/// A short, URL-free description of a client error.
fn classify(error: &hyper_util::client::legacy::Error) -> String {
    if error.is_connect() {
        // The source chain of a connect error can name the address and the TLS problem; the
        // text is cut to the first cause that carries no URL.
        let mut text = String::from("cannot connect to");
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            let cause_text = cause.to_string();
            if cause_text.contains("certificate") || cause_text.contains("tls") {
                text = format!("TLS error ({}) with", short(&cause_text));
                break;
            }
            source = cause.source();
        }
        text
    } else {
        "the request to".to_owned()
    }
}

fn short(text: &str) -> String {
    text.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(text: &str) -> IpAddr {
        IpAddr::V4(text.parse().expect("an IPv4 address"))
    }

    fn v6(text: &str) -> IpAddr {
        IpAddr::V6(text.parse().expect("an IPv6 address"))
    }

    /// Each range with its first and last address; the addresses next to the ranges are public.
    #[test]
    fn every_blocked_ipv4_range_with_its_edges() {
        let ranges = [
            ("0.0.0.0/8", "0.0.0.0", "0.255.255.255"),
            ("10.0.0.0/8", "10.0.0.0", "10.255.255.255"),
            ("100.64.0.0/10", "100.64.0.0", "100.127.255.255"),
            ("127.0.0.0/8", "127.0.0.0", "127.255.255.255"),
            ("169.254.0.0/16", "169.254.0.0", "169.254.255.255"),
            ("172.16.0.0/12", "172.16.0.0", "172.31.255.255"),
            ("192.0.0.0/24", "192.0.0.0", "192.0.0.255"),
            ("192.168.0.0/16", "192.168.0.0", "192.168.255.255"),
            ("198.18.0.0/15", "198.18.0.0", "198.19.255.255"),
            ("224.0.0.0/4", "224.0.0.0", "239.255.255.255"),
            ("240.0.0.0/4", "240.0.0.0", "255.255.255.255"),
        ];
        for (name, first, last) in ranges {
            assert!(is_blocked(v4(first)), "{name}: first {first}");
            assert!(is_blocked(v4(last)), "{name}: last {last}");
        }
        for public in [
            "1.0.0.0",
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "126.255.255.255",
            "128.0.0.0",
            "169.253.255.255",
            "169.255.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "191.255.255.255",
            "192.0.1.0",
            "192.167.255.255",
            "192.169.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
        ] {
            assert!(!is_blocked(v4(public)), "{public} is public");
        }
        assert!(is_blocked(v4("169.254.169.254")), "the metadata address");
        // The Azure WireServer is blocked as one address, not as a range: its neighbours are public.
        assert!(is_blocked(v4("168.63.129.16")), "the Azure WireServer");
        for neighbour in [
            "168.63.129.15",
            "168.63.129.17",
            "168.63.128.16",
            "168.63.130.16",
            "168.62.129.16",
            "168.64.129.16",
        ] {
            assert!(!is_blocked(v4(neighbour)), "{neighbour} is public");
        }
        assert!(is_blocked(v4("255.255.255.255")), "broadcast");
    }

    #[test]
    fn every_blocked_ipv6_range_with_its_edges() {
        for blocked in [
            "::",
            "::1",
            "::2",
            "::7f00:1",
            "::ffff:ffff",
            "fc00::",
            "fd12:3456::1",
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fe80::",
            "fe80::1",
            "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "ff00::",
            "ff02::1",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
        ] {
            assert!(is_blocked(v6(blocked)), "{blocked}");
        }
        for public in [
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "fbff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fe00::",
            "fec0::1",
            "::1:0:0",
            "1::",
        ] {
            assert!(!is_blocked(v6(public)), "{public}");
        }
    }

    #[test]
    fn mapped_and_nat64_addresses_are_judged_by_the_ipv4_address_inside() {
        for blocked in [
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:168.63.129.16",
            "64:ff9b::a83f:8110",
            "::ffff:192.168.1.1",
            "::ffff:0.0.0.0",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::10.1.2.3",
        ] {
            assert!(is_blocked(v6(blocked)), "{blocked}");
        }
        for public in ["::ffff:8.8.8.8", "::ffff:1.1.1.1", "64:ff9b::808:808"] {
            assert!(!is_blocked(v6(public)), "{public}");
        }
        // The prefix itself matters: 64:ff9b:1::/48 is not the well-known prefix.
        assert!(!is_blocked(v6("64:ff9b:1::7f00:1")));
    }

    #[tokio::test]
    async fn a_name_that_resolves_to_a_private_address_is_refused_unless_allowed() {
        let target = parse_url("https://localhost/hook", false).expect("valid");
        match vet(&target, false).await {
            Err(Failure::Refused(message)) => {
                assert!(message.contains("--allow-private"), "{message}");
                assert!(!message.contains("/hook"), "no path in the message");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let addrs = vet(&target, true).await.expect("allowed");
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|a| a.port() == 443));
        assert!(addrs.iter().all(|a| a.ip().is_loopback()));
    }

    #[tokio::test]
    async fn ip_literals_are_vetted_without_a_lookup() {
        for blocked in [
            "https://127.0.0.1/",
            "https://10.1.2.3:8443/x",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://[fe80::1]/",
        ] {
            let target = parse_url(blocked, false).expect("valid");
            assert!(
                matches!(vet(&target, false).await, Err(Failure::Refused(_))),
                "{blocked}"
            );
            assert!(vet(&target, true).await.is_ok(), "{blocked} allowed");
        }
        let public = parse_url("https://93.184.216.34:8443/x", false).expect("valid");
        assert_eq!(
            vet(&public, false).await.expect("public"),
            ["93.184.216.34:8443".parse::<SocketAddr>().expect("addr")]
        );
    }

    #[tokio::test]
    async fn the_connector_is_given_only_the_vetted_addresses() {
        let vetted: Vec<SocketAddr> = vec!["203.0.113.7:8443".parse().expect("addr")];
        let mut pinned = Pinned(Arc::new(vetted.clone()));
        // Whatever name it is asked for, it answers with the vetted addresses and nothing else.
        for name in ["example.com", "localhost", "rebind.test"] {
            let name: Name = name.parse().expect("a name");
            let got: Vec<SocketAddr> = pinned.call(name).await.expect("answers").collect();
            assert_eq!(got, vetted);
        }
    }

    /// The delivery connects to the vetted address and never resolves the name itself: a host
    /// that no resolver can answer (`.invalid`, RFC 6761) is reached through the address it is
    /// given. With a connector that looked the name up, this would fail.
    #[tokio::test]
    async fn the_name_of_the_url_is_never_resolved_again() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("address").port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut seen = Vec::new();
            let mut chunk = [0u8; 1024];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).expect("read");
                assert!(n > 0, "the request ended early");
                seen.extend_from_slice(&chunk[..n]);
            }
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                .expect("write");
            String::from_utf8_lossy(&seen).into_owned()
        });
        let target = parse_url(&format!("http://rebind.invalid:{port}/hook"), true).expect("valid");
        // The name does not resolve for us either.
        assert!(matches!(
            vet(&target, true).await,
            Err(Failure::Transport(_))
        ));
        let out = Outgoing {
            url: "unused",
            secret: "s",
            allow_private: true,
            insecure_http: true,
            event: "test",
            delivery: "d-1",
            body: b"{}".to_vec(),
        };
        let vetted: Vec<SocketAddr> = vec![SocketAddr::new(v4("127.0.0.1"), port)];
        assert_eq!(send_pinned(&target, vetted, &out).await, Ok(204));
        let request = server.join().expect("server");
        assert!(request.starts_with("POST /hook HTTP/1.1\r\n"), "{request}");
        // The Host header keeps the name of the URL.
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("host: rebind.invalid:{port}")),
            "{request}"
        );
    }

    #[test]
    fn urls_are_checked() {
        let ok = |url: &str, insecure: bool| parse_url(url, insecure);
        assert!(ok("https://example.com/hook", false).is_ok());
        assert!(ok("https://example.com:8443/hook?token=x", false).is_ok());
        assert!(ok("https://[::1]:9/", false).is_ok());
        assert!(ok("http://example.com/hook", true).is_ok());
        for (bad, insecure) in [
            ("http://example.com/hook", false),
            ("ftp://example.com/", true),
            ("https://user:pass@example.com/", false),
            ("https://user@example.com/", false),
            ("https://example.com@evil.example/", false),
            ("example.com/hook", false),
            ("/hook", false),
            ("https://", false),
            ("", false),
            ("not a url", false),
        ] {
            assert!(ok(bad, insecure).is_err(), "{bad:?}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(ok(&long, false).is_err());
    }

    #[test]
    fn errors_and_labels_hide_the_path_and_the_query() {
        let target = parse_url(
            "https://hooks.example.com:8443/services/T0/B0/TOKEN?k=v",
            false,
        )
        .expect("valid");
        assert_eq!(target.label(), "https://hooks.example.com:8443");
        let v6 = parse_url("https://[2001:db8::1]/x/TOKEN", false).expect("valid");
        assert_eq!(v6.label(), "https://[2001:db8::1]:443");
        for bad in ["http://example.com/TOKEN", "https://u:p@example.com/TOKEN"] {
            let message = parse_url(bad, false).expect_err("refused");
            assert!(!message.contains("TOKEN"), "{message}");
            assert!(!message.contains("example.com"), "{message}");
        }
    }

    /// The signature is HMAC-SHA256 over `timestamp.body`, written as `sha256=<hex>`. The
    /// expected value is the RFC 4231 style vector computed independently with Python's `hmac`:
    /// `hmac.new(b"key", b"1700000000.{}", hashlib.sha256).hexdigest()`.
    #[test]
    fn the_signature_has_the_documented_form() {
        let signed = signature("key", 1_700_000_000, b"{}");
        assert_eq!(
            signed,
            "sha256=9d713ed406bb7076d4123f0dc2c39d2df5c654ed4b0cd56b52c8b4c940bd63ae"
        );
        // Changing the timestamp, the body or the secret changes the signature.
        assert_ne!(signed, signature("key", 1_700_000_001, b"{}"));
        assert_ne!(signed, signature("key", 1_700_000_000, b"{ }"));
        assert_ne!(signed, signature("other", 1_700_000_000, b"{}"));
    }
}
