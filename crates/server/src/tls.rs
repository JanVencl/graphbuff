//! TLS 1.3 and 1.2 with rustls, and the ring or RustCrypto crypto provider.
//!
//! A [`TlsStream`] may be cloned into a reader and a writer on different threads: the
//! clones share the TLS session behind a mutex, which is never held while waiting for
//! the client to send something.

use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, MutexGuard};

use rustls::ServerConnection;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::net::{Accepted, Stream, TlsConfig};

/// The crypto provider the build chose: ring with the `tls` feature, else RustCrypto.
fn provider() -> rustls::crypto::CryptoProvider {
    #[cfg(feature = "tls")]
    return rustls::crypto::ring::default_provider();
    #[cfg(not(feature = "tls"))]
    return rustls_rustcrypto::provider();
}

/// A server's TLS setup.
pub(crate) struct Tls {
    config: Arc<rustls::ServerConfig>,
    allow_plaintext: bool,
}

fn invalid(what: impl std::fmt::Display) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, what.to_string())
}

impl Tls {
    /// Loads the certificate chain and key that `config` names.
    pub(crate) fn load(config: &TlsConfig) -> io::Result<Tls> {
        let certs = CertificateDer::pem_file_iter(&config.cert_chain)
            .and_then(|it| it.collect::<Result<Vec<_>, _>>())
            .map_err(|e| invalid(format!("reading certificates from {}: {e}", config.cert_chain.display())))?;
        if certs.is_empty() {
            return Err(invalid(format!("no certificate in {}", config.cert_chain.display())));
        }
        let key = PrivateKeyDer::from_pem_file(&config.private_key)
            .map_err(|e| invalid(format!("reading the private key from {}: {e}", config.private_key.display())))?;
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(provider()))
            .with_safe_default_protocol_versions()
            .map_err(invalid)?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| invalid(format!("the certificate or key cannot be used: {e}")))?;
        Ok(Tls {
            config: Arc::new(server),
            allow_plaintext: config.allow_plaintext,
        })
    }

    pub(crate) fn allow_plaintext(&self) -> bool {
        self.allow_plaintext
    }

    /// Starts a TLS session on `tcp`; the handshake runs on the first read.
    pub(crate) fn wrap(&self, tcp: TcpStream) -> Accepted {
        match ServerConnection::new(Arc::clone(&self.config)) {
            Ok(conn) => Accepted::Stream(Stream::Tls(TlsStream {
                tcp,
                session: Arc::new(Mutex::new(Session {
                    conn,
                    received: Vec::new(),
                    eof: false,
                })),
            })),
            Err(_) => Accepted::Gone,
        }
    }
}

struct Session {
    conn: ServerConnection,
    /// Bytes read from the socket that rustls has not taken yet.
    received: Vec<u8>,
    /// The client closed its side of the connection.
    eof: bool,
}

impl Session {
    /// Processes received records, and sends what rustls has to send (handshake
    /// messages, alerts), even when processing failed.
    fn process(&mut self, tcp: &TcpStream) -> io::Result<()> {
        let processed = self.conn.process_new_packets().map(|_| ()).map_err(invalid);
        self.send(tcp)?;
        processed
    }

    fn send(&mut self, mut tcp: &TcpStream) -> io::Result<()> {
        while self.conn.wants_write() {
            self.conn.write_tls(&mut tcp)?;
        }
        Ok(())
    }
}

pub(crate) struct TlsStream {
    tcp: TcpStream,
    session: Arc<Mutex<Session>>,
}

impl TlsStream {
    pub(crate) fn tcp(&self) -> &TcpStream {
        &self.tcp
    }

    pub(crate) fn try_clone(&self) -> io::Result<TlsStream> {
        Ok(TlsStream {
            tcp: self.tcp.try_clone()?,
            session: Arc::clone(&self.session),
        })
    }

    fn session(&self) -> MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn close_notify(&self) {
        let mut s = self.session();
        s.conn.send_close_notify();
        let _ = s.send(&self.tcp);
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            {
                let mut s = self.session();
                match s.conn.reader().read(buf) {
                    // Zero: the client closed the session properly.
                    Ok(n) => return Ok(n),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
                if !s.received.is_empty() {
                    let Session { conn, received, .. } = &mut *s;
                    let used = conn.read_tls(&mut received.as_slice())?;
                    received.drain(..used);
                    s.process(&self.tcp)?;
                    continue;
                }
                if s.eof {
                    return Err(ErrorKind::UnexpectedEof.into());
                }
            }
            // Wait for the client without holding the session, so writers can write.
            let mut raw = [0u8; 16 * 1024];
            let n = (&self.tcp).read(&mut raw)?;
            let mut s = self.session();
            if n == 0 {
                // The end of the stream: rustls reports it as a clean close or an error.
                s.eof = true;
                s.conn.read_tls(&mut &[][..])?;
                s.process(&self.tcp)?;
            } else {
                s.received.extend_from_slice(&raw[..n]);
            }
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut s = self.session();
        let n = s.conn.writer().write(buf)?;
        s.send(&self.tcp)?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut s = self.session();
        s.conn.writer().flush()?;
        s.send(&self.tcp)
    }
}
