//! HTTPS and Bolt over TLS (`--features tls` or `tls-rustcrypto`). The certificate in `tests/tls/` is for
//! `localhost` and 127.0.0.1, issued by the test CA in `tests/tls/ca.pem`.
#![cfg(any(feature = "tls", feature = "tls-rustcrypto"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use graphbuff::DatabaseManager;
use graphbuff_server::packstream::{self, PackValue};
use graphbuff_server::{Server, ServerConfig, TlsConfig};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};

fn file(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("tls").join(name)
}

fn start(allow_plaintext: bool) -> Server {
    Server::start(
        Arc::new(DatabaseManager::in_memory()),
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            bolt: Some("127.0.0.1:0".into()),
            tls: Some(TlsConfig {
                allow_plaintext,
                ..TlsConfig::new(file("cert.pem"), file("key.pem"))
            }),
            ..ServerConfig::default()
        },
    )
    .unwrap()
}

type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

fn provider() -> rustls::crypto::CryptoProvider {
    #[cfg(feature = "tls")]
    return rustls::crypto::ring::default_provider();
    #[cfg(not(feature = "tls"))]
    return rustls_rustcrypto::provider();
}

fn client_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from_pem_file(file("ca.pem")).unwrap()).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

fn connect(addr: SocketAddr, name: &str) -> Tls {
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let conn = rustls::ClientConnection::new(client_config(), ServerName::try_from(name.to_string()).unwrap()).unwrap();
    rustls::StreamOwned::new(conn, tcp)
}

/// Sends one keep-alive request and reads the reply's status and body.
fn request(s: &mut BufReader<Tls>, method: &str, path: &str, body: &str) -> (u16, String) {
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.get_mut().write_all(req.as_bytes()).unwrap();
    let mut line = String::new();
    s.read_line(&mut line).unwrap();
    let status = line.split(' ').nth(1).unwrap().parse().unwrap();
    let mut len = 0;
    loop {
        let mut h = String::new();
        s.read_line(&mut h).unwrap();
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((n, v)) = h.split_once(':')
            && n.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; len];
    s.read_exact(&mut body).unwrap();
    (status, String::from_utf8(body).unwrap())
}

#[test]
fn https_requests() {
    let server = start(false);
    let mut s = BufReader::new(connect(server.local_addr(), "localhost"));
    let (status, body) = request(&mut s, "POST", "/query", r#"{"query":"CREATE (n {x: 1}) RETURN n.x AS x"}"#);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""rows":[[1]]"#), "{body}");
    // The same connection keeps working, with a body larger than one TLS record.
    let big = "x".repeat(100_000);
    let (status, body) = request(&mut s, "POST", "/query", &format!(r#"{{"query":"RETURN size('{big}') AS n"}}"#));
    assert_eq!(status, 200);
    assert!(body.contains("100000"), "{body}");
    let (status, body) = request(&mut s, "POST", "/query", r#"{"query":"UNWIND range(1, 20000) AS i RETURN i"}"#);
    assert_eq!(status, 200);
    assert!(body.len() > 100_000);

    // By IP address too (the certificate names 127.0.0.1).
    let mut s = BufReader::new(connect(server.local_addr(), "127.0.0.1"));
    assert_eq!(request(&mut s, "GET", "/health", "").0, 200);
}

#[test]
fn plaintext_clients_are_refused_unless_allowed() {
    let server = start(false);
    let mut tcp = TcpStream::connect(server.local_addr()).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    tcp.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut reply = String::new();
    let _ = tcp.read_to_string(&mut reply);
    assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
    assert!(reply.contains("HTTPS"), "{reply}");

    // Plain Bolt: the connection just closes.
    let mut tcp = TcpStream::connect(server.bolt_addr().unwrap()).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    tcp.write_all(&[0x60, 0x60, 0xB0, 0x17, 0, 4, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    let mut b = [0u8; 4];
    assert!(matches!(tcp.read(&mut b), Ok(0) | Err(_)));

    let server = start(true);
    let mut tcp = TcpStream::connect(server.local_addr()).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    tcp.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut reply = String::new();
    tcp.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    let mut s = BufReader::new(connect(server.local_addr(), "localhost"));
    assert_eq!(request(&mut s, "GET", "/health", "").0, 200);
}

#[test]
fn wrong_server_names_fail_the_handshake() {
    let server = start(false);
    let mut s = connect(server.local_addr(), "example.com");
    assert!(s.write_all(b"GET /health HTTP/1.1\r\n\r\n").and_then(|_| s.flush()).is_err() || {
        let mut b = [0u8; 1];
        s.read(&mut b).is_err()
    });
}

fn bolt_send(s: &mut Tls, tag: u8, fields: Vec<PackValue>) {
    let mut buf = Vec::new();
    packstream::encode(&mut buf, &PackValue::Struct { tag, fields });
    let mut out = (buf.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(&buf);
    out.extend_from_slice(&[0, 0]);
    s.write_all(&out).unwrap();
}

fn bolt_recv(s: &mut Tls) -> (u8, Vec<PackValue>) {
    let mut message = Vec::new();
    loop {
        let mut size = [0u8; 2];
        s.read_exact(&mut size).unwrap();
        let n = u16::from_be_bytes(size) as usize;
        if n == 0 {
            break;
        }
        let start = message.len();
        message.resize(start + n, 0);
        s.read_exact(&mut message[start..]).unwrap();
    }
    match packstream::decode(&message).unwrap() {
        PackValue::Struct { tag, fields } => (tag, fields),
        other => panic!("{other:?}"),
    }
}

#[test]
fn bolt_over_tls() {
    let server = start(false);
    let mut s = connect(server.bolt_addr().unwrap(), "localhost");
    s.write_all(&[0x60, 0x60, 0xB0, 0x17, 0, 4, 4, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
    let mut v = [0u8; 4];
    s.read_exact(&mut v).unwrap();
    assert_eq!(v, [0, 0, 4, 5]);
    bolt_send(&mut s, 0x01, vec![PackValue::map([("user_agent", PackValue::str("t/1"))])]);
    assert_eq!(bolt_recv(&mut s).0, 0x70);
    bolt_send(&mut s, 0x6A, vec![PackValue::map([("scheme", PackValue::str("none"))])]);
    assert_eq!(bolt_recv(&mut s).0, 0x70);
    let empty = || PackValue::map(std::iter::empty::<(&str, PackValue)>());
    bolt_send(&mut s, 0x10, vec![PackValue::str("UNWIND range(1, 5000) AS i RETURN i"), empty(), empty()]);
    assert_eq!(bolt_recv(&mut s).0, 0x70);
    bolt_send(&mut s, 0x3F, vec![PackValue::map([("n", PackValue::Int(-1))])]);
    let mut rows = 0;
    loop {
        match bolt_recv(&mut s).0 {
            0x71 => rows += 1,
            0x70 => break,
            other => panic!("unexpected message 0x{other:02X}"),
        }
    }
    assert_eq!(rows, 5000);
    bolt_send(&mut s, 0x02, vec![]);
}

#[test]
fn bad_certificate_files_fail_to_start() {
    let config = |cert: &str, key: &str| ServerConfig {
        bind: "127.0.0.1:0".into(),
        tls: Some(TlsConfig::new(file(cert), file(key))),
        ..ServerConfig::default()
    };
    let mgr = Arc::new(DatabaseManager::in_memory());
    assert!(Server::start(Arc::clone(&mgr), config("missing.pem", "key.pem")).is_err());
    assert!(Server::start(Arc::clone(&mgr), config("cert.pem", "ca.pem")).is_err());
    assert!(Server::start(mgr, config("key.pem", "key.pem")).is_err());
}
