//! Connections: plain TCP, or TLS over TCP (with the `tls` feature).
//!
//! A [`Stream`] can be cloned for a reader and a writer on different threads, like a
//! [`TcpStream`]; TLS clones share one TLS session.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// TLS settings for [`crate::ServerConfig::tls`]. Serving TLS needs the `tls` (ring) or
/// `tls-rustcrypto` feature.
#[derive(Clone, Debug)]
pub struct TlsConfig {
    /// The certificate chain, PEM: the server's certificate first.
    pub cert_chain: PathBuf,
    /// The private key, PEM (PKCS #8, PKCS #1 or SEC1).
    pub private_key: PathBuf,
    /// Also accept clients that do not use TLS, on the same ports. The first byte a
    /// client sends tells which it speaks. Off by default: only TLS.
    pub allow_plaintext: bool,
}

impl TlsConfig {
    pub fn new(cert_chain: impl Into<PathBuf>, private_key: impl Into<PathBuf>) -> Self {
        TlsConfig {
            cert_chain: cert_chain.into(),
            private_key: private_key.into(),
            allow_plaintext: false,
        }
    }
}

pub(crate) enum Stream {
    Plain(TcpStream),
    #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
    Tls(crate::tls::TlsStream),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
            Stream::Tls(s) => s.tcp(),
        }
    }

    pub(crate) fn try_clone(&self) -> io::Result<Stream> {
        Ok(match self {
            Stream::Plain(s) => Stream::Plain(s.try_clone()?),
            #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
            Stream::Tls(s) => Stream::Tls(s.try_clone()?),
        })
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp().local_addr()
    }

    pub(crate) fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.tcp().peer_addr()
    }

    /// Closes the connection (after a TLS `close_notify`, if it can be sent) without
    /// losing what was written: sends a FIN after it, then
    /// reads and drops what the client still sends (until it closes, for at most `linger`,
    /// or while `keep_going`), then closes. Closing a socket with unread input sends a
    /// reset instead, and a client that gets one may drop the reply still in its own
    /// receive buffer (Windows does), e.g. the failure for a message that was too large.
    pub(crate) fn close_gracefully(&self, linger: Duration, keep_going: &dyn Fn() -> bool) {
        #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
        if let Stream::Tls(s) = self {
            s.close_notify();
        }
        let tcp = self.tcp();
        if tcp.shutdown(Shutdown::Write).is_ok() {
            let deadline = Instant::now() + linger;
            let mut buf = [0u8; 4096];
            while Instant::now() < deadline && keep_going() {
                match (&mut &*tcp).read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
                    Err(_) => break,
                }
            }
        }
        let _ = tcp.shutdown(Shutdown::Both);
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
            Stream::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// What a new connection turned out to be.
pub(crate) enum Accepted {
    Stream(Stream),
    /// A plaintext client on a TLS-only server.
    PlainRefused(TcpStream),
    /// The client sent nothing in time, or the server is stopping.
    Gone,
}

/// The first byte of a TLS connection (a handshake record).
const TLS_HANDSHAKE: u8 = 0x16;

/// Sets up a new connection: TLS if the server has it (and the client starts a TLS
/// handshake), plain TCP otherwise. Read timeouts must already be set on `tcp`.
pub(crate) fn accept(tcp: TcpStream, tls: Option<&Tls>, limit: Duration, stop: &dyn Fn() -> bool) -> Accepted {
    let Some(tls) = tls else {
        return Accepted::Stream(Stream::Plain(tcp));
    };
    // Wait for the client's first byte.
    let started = Instant::now();
    let mut first = [0u8; 1];
    loop {
        match tcp.peek(&mut first) {
            Ok(0) => return Accepted::Gone,
            Ok(_) => break,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => {
                if stop() || started.elapsed() >= limit {
                    return Accepted::Gone;
                }
            }
            Err(_) => return Accepted::Gone,
        }
    }
    if first[0] != TLS_HANDSHAKE {
        return if tls.allow_plaintext() {
            Accepted::Stream(Stream::Plain(tcp))
        } else {
            Accepted::PlainRefused(tcp)
        };
    }
    tls.wrap(tcp)
}

/// A server's TLS setup, loaded from a [`TlsConfig`].
#[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
pub(crate) use crate::tls::Tls;

#[cfg(not(any(feature = "tls", feature = "tls-rustcrypto")))]
pub(crate) enum Tls {}

#[cfg(not(any(feature = "tls", feature = "tls-rustcrypto")))]
impl Tls {
    pub(crate) fn load(_: &TlsConfig) -> io::Result<Tls> {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "TLS needs graphbuff-server built with the `tls` or `tls-rustcrypto` feature (cargo build --features tls)",
        ))
    }

    fn allow_plaintext(&self) -> bool {
        match *self {}
    }

    fn wrap(&self, _: TcpStream) -> Accepted {
        match *self {}
    }
}
