//! The connection: plain TCP, or TLS with rustls when built with the `tls` or
//! `tls-rustcrypto` feature.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

use crate::client::Url;

#[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

pub enum Stream {
    Plain(TcpStream),
    #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
    Tls(Box<TlsStream>),
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

/// Makes connections TLS connections when the URL is `https://`.
pub struct Connector {
    #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
    config: Option<std::sync::Arc<rustls::ClientConfig>>,
}

impl Connector {
    /// A connector for `url`, trusting the certificates in `ca_file` (PEM), or else those
    /// in `SSL_CERT_FILE` or the system's CA bundle.
    pub fn new(url: &Url, ca_file: Option<PathBuf>) -> Result<Connector, String> {
        #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
        {
            let config = if url.tls { Some(imp::config(ca_file)?) } else { None };
            Ok(Connector { config })
        }
        #[cfg(not(any(feature = "tls", feature = "tls-rustcrypto")))]
        {
            let _ = ca_file;
            if url.tls {
                return Err(format!(
                    "{url}: this build has no TLS; build graphbuff-cli with `--features tls` (or `tls-rustcrypto`)"
                ));
            }
            Ok(Connector {})
        }
    }

    pub fn wrap(&self, url: &Url, tcp: TcpStream) -> io::Result<Stream> {
        #[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
        if let Some(config) = &self.config {
            let name = rustls::pki_types::ServerName::try_from(url.host.clone())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            let conn = rustls::ClientConnection::new(config.clone(), name).map_err(io::Error::other)?;
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            // Handshake now, so certificate problems show as connection errors.
            while stream.conn.is_handshaking() {
                stream.conn.complete_io(&mut stream.sock)?;
            }
            return Ok(Stream::Tls(Box::new(stream)));
        }
        let _ = url;
        Ok(Stream::Plain(tcp))
    }
}

#[cfg(any(feature = "tls", feature = "tls-rustcrypto"))]
mod imp {
    use std::path::PathBuf;
    use std::sync::Arc;

    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;

    fn provider() -> rustls::crypto::CryptoProvider {
        #[cfg(feature = "tls")]
        return rustls::crypto::ring::default_provider();
        #[cfg(not(feature = "tls"))]
        return rustls_rustcrypto::provider();
    }

    /// CA bundles of common systems.
    const SYSTEM_BUNDLES: &[&str] = &[
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/ca-bundle.pem",
        "/etc/ssl/cert.pem",
        "/usr/local/etc/openssl/cert.pem",
    ];

    pub fn config(ca_file: Option<PathBuf>) -> Result<Arc<rustls::ClientConfig>, String> {
        let mut roots = rustls::RootCertStore::empty();
        let chosen = ca_file.or_else(|| std::env::var_os("SSL_CERT_FILE").map(PathBuf::from));
        match chosen {
            Some(file) => {
                let certs = CertificateDer::pem_file_iter(&file)
                    .and_then(|it| it.collect::<Result<Vec<_>, _>>())
                    .map_err(|e| format!("reading certificates from {}: {e}", file.display()))?;
                let (added, _) = roots.add_parsable_certificates(certs);
                if added == 0 {
                    return Err(format!("no usable certificate in {}", file.display()));
                }
            }
            None => {
                for file in SYSTEM_BUNDLES {
                    if let Ok(it) = CertificateDer::pem_file_iter(file) {
                        roots.add_parsable_certificates(it.filter_map(Result::ok));
                    }
                }
                if roots.is_empty() {
                    return Err("no CA certificates found to check the server's certificate; give them with --cacert".into());
                }
            }
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Arc::new(config))
    }
}
