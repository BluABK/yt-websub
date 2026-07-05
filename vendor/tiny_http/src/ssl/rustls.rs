use crate::connection::Connection;
use crate::util::refined_tcp_stream::Stream as RefinedStream;
use std::error::Error;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::sync::{Arc, Mutex};
use zeroize::Zeroizing;

/// A wrapper around an owned Rustls connection and corresponding stream.
///
/// Uses an internal Mutex to permit disparate reader & writer threads to access the stream independently.
pub(crate) struct RustlsStream(
    Arc<Mutex<rustls::StreamOwned<rustls::ServerConnection, Connection>>>,
);

impl RustlsStream {
    pub(crate) fn peer_addr(&mut self) -> std::io::Result<Option<SocketAddr>> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .sock
            .peer_addr()
    }

    pub(crate) fn shutdown(&mut self, how: Shutdown) -> std::io::Result<()> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .sock
            .shutdown(how)
    }
}

impl Clone for RustlsStream {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Read for RustlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .read(buf)
    }
}

impl Write for RustlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .expect("Failed to lock SSL stream mutex")
            .flush()
    }
}

pub(crate) struct RustlsContext(Arc<rustls::ServerConfig>);

impl RustlsContext {
    pub(crate) fn from_pem(
        certificates: Vec<u8>,
        private_key: Zeroizing<Vec<u8>>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let certificate_chain: Vec<rustls::Certificate> =
            rustls_pemfile::certs(&mut certificates.as_slice())?
                .into_iter()
                .map(|bytes| rustls::Certificate(bytes))
                .collect();

        if certificate_chain.is_empty() {
            return Err("Couldn't extract certificate chain from config.".into());
        }

        let private_key = rustls::PrivateKey({
            // LOCAL PATCH (yt-websub): upstream `.expect()`d on the pemfile parse
            // and indexed `rsa_keys[0]` unconditionally, so a valid-but-unsupported
            // or empty/garbled key file PANICKED. Under panic=abort that aborts at
            // startup, bypassing main.rs's graceful exit(2) and flapping under
            // systemd Restart. Now we try PKCS#8, then PKCS#1 (RSA), and return a
            // descriptive error the caller surfaces instead of aborting.
            //
            // NOTE: this pinned rustls-pemfile 0.2.1 cannot parse a bare SEC1
            // "BEGIN EC PRIVATE KEY" file. certbot/Let's Encrypt already emit
            // PKCS#8 for both RSA and ECDSA, so this only affects hand-generated
            // SEC1 keys (`openssl ecparam -genkey`); convert one with
            // `openssl pkcs8 -topk8 -nocrypt -in ec.pem -out ec.pk8.pem`.
            let der = rustls_pemfile::pkcs8_private_keys(&mut private_key.clone().as_slice())
                .unwrap_or_default()
                .into_iter()
                .next()
                .or_else(|| {
                    rustls_pemfile::rsa_private_keys(&mut private_key.as_slice())
                        .unwrap_or_default()
                        .into_iter()
                        .next()
                });

            match der {
                Some(der) => der,
                None => {
                    return Err("no supported private key found in key file (expected an \
                                unencrypted PKCS#8 or PKCS#1/RSA PEM key; convert a SEC1/EC \
                                key with `openssl pkcs8 -topk8 -nocrypt`)"
                        .into())
                }
            }
        });

        let tls_conf = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(certificate_chain, private_key)?;

        Ok(Self(Arc::new(tls_conf)))
    }

    pub(crate) fn accept(
        &self,
        stream: Connection,
    ) -> Result<RustlsStream, Box<dyn Error + Send + Sync + 'static>> {
        let connection = rustls::ServerConnection::new(self.0.clone())?;
        Ok(RustlsStream(Arc::new(Mutex::new(
            rustls::StreamOwned::new(connection, stream),
        ))))
    }
}

impl From<RustlsStream> for RefinedStream {
    fn from(stream: RustlsStream) -> Self {
        Self::Https(stream)
    }
}
