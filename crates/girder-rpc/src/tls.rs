//! HTTPS for the same axum router the plain listener serves.
//!
//! The certificate and key are read once, before the socket accepts, matching the host webserver:
//! a bad pair refuses startup instead of falling back to HTTP. Handshake failures stay on that
//! connection; the listener keeps accepting.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::serve::Listener;
use base64::Engine as _;
use rustls::ServerConfig;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// A TCP listener that completes a TLS handshake before handing the stream to axum.
pub(crate) struct TlsListener {
    tcp: TcpListener,
    acceptor: TlsAcceptor,
}

impl TlsListener {
    /// Wraps an already-bound socket with the PEM material.
    ///
    /// # Errors
    ///
    /// Returns an error when the PEM has no certificate, the key type is unknown, or the key does
    /// not match the certificate.
    pub(crate) fn new(tcp: TcpListener, cert_pem: &str, key_pem: &str) -> io::Result<Self> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let certs = certificates(cert_pem).map_err(invalid)?;
        let key = private_key(key_pem).map_err(invalid)?;
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(invalid)?;
        Ok(Self {
            tcp,
            acceptor: TlsAcceptor::from(Arc::new(config)),
        })
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, address) = match self.tcp.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::debug!(%error, "tls listener accept failed");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            match self.acceptor.accept(stream).await {
                Ok(tls) => return (tls, address),
                Err(error) => tracing::debug!(%error, "tls handshake failed"),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error.to_string())
}

fn certificates(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs: Vec<CertificateDer<'static>> = pem_blocks(pem)
        .into_iter()
        .filter(|(label, _)| label == "CERTIFICATE" || label.ends_with(" CERTIFICATE"))
        .map(|(_, der)| CertificateDer::from(der))
        .collect();
    if certs.is_empty() {
        return Err("tls certificate PEM holds no CERTIFICATE block".to_owned());
    }
    Ok(certs)
}

fn private_key(pem: &str) -> Result<PrivateKeyDer<'static>, String> {
    let Some((label, der)) = pem_blocks(pem).into_iter().find(|(label, _)| {
        label == "PRIVATE KEY" || label == "RSA PRIVATE KEY" || label == "EC PRIVATE KEY"
    }) else {
        return Err("tls key PEM holds no PRIVATE KEY block".to_owned());
    };
    Ok(match label.as_str() {
        "RSA PRIVATE KEY" => PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(der)),
        "EC PRIVATE KEY" => PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(der)),
        _ => PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(der)),
    })
}

fn pem_blocks(pem: &str) -> Vec<(String, Vec<u8>)> {
    let mut blocks = Vec::new();
    let mut lines = pem.lines();
    while let Some(line) = lines.next() {
        let Some(label) = line
            .trim()
            .strip_prefix("-----BEGIN ")
            .and_then(|rest| rest.strip_suffix("-----"))
        else {
            continue;
        };
        let mut body = String::new();
        for line in lines.by_ref() {
            let line = line.trim();
            if let Some(end) = line.strip_prefix("-----END ") {
                if end.strip_suffix("-----") == Some(label) {
                    if let Ok(der) =
                        base64::engine::general_purpose::STANDARD.decode(body.as_bytes())
                    {
                        blocks.push((label.to_owned(), der));
                    }
                }
                break;
            }
            body.push_str(line);
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustls::ClientConfig;
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpStream;
    use tokio_rustls::TlsConnector;

    use super::{TlsListener, certificates};

    #[tokio::test]
    async fn https_answers_after_a_real_handshake() {
        let dir = std::env::temp_dir().join(format!(
            "girder-tls-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis())
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        let status = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:prime256v1",
                "-keyout",
            ])
            .arg(&key_path)
            .arg("-out")
            .arg(&cert_path)
            .args([
                "-days",
                "1",
                "-nodes",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost",
                "-addext",
                "basicConstraints=CA:FALSE",
            ])
            .status()
            .expect("openssl");
        assert!(status.success(), "openssl certificate");
        let cert_pem = std::fs::read_to_string(&cert_path).expect("cert");
        let key_pem = std::fs::read_to_string(&key_path).expect("key");

        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = tcp.local_addr().expect("address");
        let listener = TlsListener::new(tcp, &cert_pem, &key_pem).expect("tls listener");
        let cancel = tokio_util::sync::CancellationToken::new();
        let stop = cancel.clone();
        let server = tokio::spawn(async move {
            let router = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { stop.cancelled().await })
                .await;
        });

        let tcp = TcpStream::connect(address).await.expect("connect");
        let connector = TlsConnector::from(Arc::new(client_config(&cert_pem)));
        let mut tls = connector
            .connect(ServerName::try_from("localhost").expect("name"), tcp)
            .await
            .expect("handshake");
        tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("write");
        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.expect("read");
        let text = String::from_utf8_lossy(&response);
        assert!(text.contains("200"), "{text}");
        assert!(text.contains("ok"), "{text}");
        cancel.cancel();
        let _ = server.await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn client_config(cert_pem: &str) -> ClientConfig {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        let cert = certificates(cert_pem).expect("cert").remove(0);
        roots.add(cert).expect("trust anchor");
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    }
}
