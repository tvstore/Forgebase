//! Helpers for the webhook tests (M9.2): a receiver that records what it is sent, with or without
//! TLS, and a throwaway certificate authority made with the `openssl` command-line tool.
//!
//! The receiver uses plain `std` threads and blocking sockets, so it works the same under every
//! test runtime and needs no HTTP server crate. Every test starts its own receiver on
//! `127.0.0.1:0`; dropping it stops its thread.

#![allow(dead_code)] // each test file uses a subset

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// One request the receiver got.
#[derive(Debug, Clone)]
pub struct Received {
    /// The request line, for example `POST /hook HTTP/1.1`.
    pub request_line: String,
    /// The headers, with lower-case names.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

impl Received {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("the body is JSON")
    }
}

/// What the receiver answers.
#[derive(Debug, Clone)]
pub enum Reply {
    /// This status with an empty body.
    Status(u16),
    /// 200 with a body of this many bytes.
    Large(usize),
    /// A redirect (this status) to this location.
    Redirect(u16, String),
    /// Accept the request and never answer.
    Hang,
    /// The first answers in order, then the last one forever.
    Sequence(Vec<Reply>),
}

struct Shared {
    requests: Mutex<Vec<Received>>,
    reply: Mutex<Reply>,
    stop: AtomicBool,
    served: Mutex<usize>,
}

pub struct Receiver {
    pub addr: SocketAddr,
    tls: bool,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// A certificate authority and a server certificate for `localhost` and `127.0.0.1`. The files
/// `openssl` wrote are read and removed at once, so nothing is left in the temporary directory.
pub struct Pki {
    pub ca_pem: String,
    cert_pem: String,
    key_pem: String,
}

fn openssl(dir: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the `openssl` command-line tool is needed by the TLS tests");
    assert!(
        output.status.success(),
        "openssl {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

impl Pki {
    /// Makes a fresh CA and a server certificate signed by it.
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "forgebase-pki-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        openssl(
            &dir,
            &[
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
                "ca.key",
            ],
        );
        openssl(
            &dir,
            &[
                "req",
                "-x509",
                "-new",
                "-key",
                "ca.key",
                "-sha256",
                "-days",
                "3650",
                "-subj",
                &format!("/CN=Forgebase test CA {name}"),
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
                "-out",
                "ca.pem",
            ],
        );
        openssl(
            &dir,
            &[
                "ecparam",
                "-name",
                "prime256v1",
                "-genkey",
                "-noout",
                "-out",
                "server.key",
            ],
        );
        openssl(
            &dir,
            &[
                "req",
                "-new",
                "-key",
                "server.key",
                "-subj",
                "/CN=localhost",
                "-out",
                "server.csr",
            ],
        );
        std::fs::write(
            dir.join("server.ext"),
            "basicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n\
             subjectAltName=DNS:localhost,IP:127.0.0.1\n",
        )
        .expect("the extension file");
        openssl(
            &dir,
            &[
                "x509",
                "-req",
                "-in",
                "server.csr",
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-days",
                "3650",
                "-sha256",
                "-extfile",
                "server.ext",
                "-out",
                "server.pem",
            ],
        );
        let read = |file: &str| std::fs::read_to_string(dir.join(file)).expect(file);
        let pki = Self {
            ca_pem: read("ca.pem"),
            cert_pem: read("server.pem"),
            key_pem: read("server.key"),
        };
        let _ = std::fs::remove_dir_all(&dir);
        pki
    }

    fn server_config(&self) -> Arc<rustls::ServerConfig> {
        let chain = CertificateDer::pem_slice_iter(self.cert_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .expect("certificates");
        let key = PrivateKeyDer::from_pem_slice(self.key_pem.as_bytes()).expect("the private key");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("protocol versions")
                .with_no_client_auth()
                .with_single_cert(chain, key)
                .expect("a certificate and key that fit"),
        )
    }
}

impl Receiver {
    /// A plain HTTP receiver.
    pub fn http(reply: Reply) -> Self {
        Self::start(reply, None)
    }

    /// An HTTPS receiver with the certificate of `pki`.
    pub fn https(reply: Reply, pki: &Pki) -> Self {
        Self::start(reply, Some(pki.server_config()))
    }

    fn start(reply: Reply, tls: Option<Arc<rustls::ServerConfig>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("address");
        let shared = Arc::new(Shared {
            requests: Mutex::new(Vec::new()),
            reply: Mutex::new(reply),
            stop: AtomicBool::new(false),
            served: Mutex::new(0),
        });
        let is_tls = tls.is_some();
        let state = Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for stream in listener.incoming() {
                if state.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let state = Arc::clone(&state);
                let tls = tls.clone();
                handlers.push(std::thread::spawn(move || serve(stream, &state, tls)));
            }
            for handler in handlers {
                let _ = handler.join();
            }
        });
        Self {
            addr,
            tls: is_tls,
            shared,
            thread: Some(thread),
        }
    }

    /// The URL of `path` on this receiver, with the IP address as host.
    pub fn url(&self, path: &str) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://127.0.0.1:{}{path}", self.addr.port())
    }

    /// The URL with the name `localhost` as host.
    pub fn localhost_url(&self, path: &str) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://localhost:{}{path}", self.addr.port())
    }

    pub fn set_reply(&self, reply: Reply) {
        *self.shared.reply.lock().expect("lock") = reply;
    }

    /// The requests received so far, oldest first.
    pub fn requests(&self) -> Vec<Received> {
        self.shared.requests.lock().expect("lock").clone()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Unblock `accept`.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn next_reply(shared: &Shared) -> Reply {
    let mut served = shared.served.lock().expect("lock");
    let index = *served;
    *served += 1;
    let reply = shared.reply.lock().expect("lock").clone();
    match reply {
        Reply::Sequence(list) => list
            .get(index)
            .or_else(|| list.last())
            .cloned()
            .unwrap_or(Reply::Status(200)),
        other => other,
    }
}

fn serve(stream: TcpStream, shared: &Shared, tls: Option<Arc<rustls::ServerConfig>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    match tls {
        None => handle(stream, shared),
        Some(config) => {
            let Ok(connection) = rustls::ServerConnection::new(config) else {
                return;
            };
            handle(rustls::StreamOwned::new(connection, stream), shared);
        }
    }
}

fn handle(mut stream: impl Read + Write, shared: &Shared) {
    // Read the head, then the body by Content-Length. A failed handshake or read ends here, with
    // nothing recorded: the test sees no request.
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => data.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    shared.requests.lock().expect("lock").push(Received {
        request_line,
        headers,
        body,
    });
    match next_reply(shared) {
        Reply::Hang => {
            while !shared.stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Reply::Status(status) => respond(&mut stream, status, &[], &[]),
        Reply::Large(size) => respond(&mut stream, 200, &[], &vec![b'x'; size]),
        Reply::Redirect(status, location) => {
            respond(&mut stream, status, &[("location", &location)], &[]);
        }
        Reply::Sequence(_) => unreachable!("resolved by next_reply"),
    }
}

fn respond(stream: &mut impl Write, status: u16, extra: &[(&str, &str)], body: &[u8]) {
    let mut head = format!(
        "HTTP/1.1 {status} Test\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    // The client may close early (it discards the body after a limit): ignore write errors.
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// HMAC-SHA256 written out from RFC 2104, independent of the `hmac` crate the code under test
/// uses.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(key_block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// The signature header a receiver should expect: `sha256=<hex of HMAC(secret, "<ts>.<body>")>`.
pub fn expected_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut message = format!("{timestamp}.").into_bytes();
    message.extend_from_slice(body);
    let mac = hmac_sha256(secret.as_bytes(), &message);
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

#[test]
fn the_independent_hmac_matches_a_known_vector() {
    // RFC 4231, test case 2.
    let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}
