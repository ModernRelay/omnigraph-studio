//! A certificate authority of a test's own, and an HTTPS server it vouches for, for the tests of
//! a CLI target that trusts its own authority (`transport.tls.ca`, spec 003).

use std::net::Ipv4Addr;
use std::sync::Arc;

use futures::{AsyncReadExt as _, AsyncWriteExt as _};
use futures_rustls::rustls::{self, ServerConfig};

/// A certificate authority: its certificate, as PEM, and what it signs with.
pub(crate) struct Authority {
    pub(crate) pem: String,
    certificate: rcgen::Certificate,
    key: rcgen::KeyPair,
}

impl Authority {
    pub(crate) fn new(name: &str) -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let mut subject = rcgen::DistinguishedName::new();
        subject.push(rcgen::DnType::CommonName, name);
        params.distinguished_name = subject;
        let certificate = params.self_signed(&key).unwrap();
        Self {
            pem: certificate.pem(),
            certificate,
            key,
        }
    }

    /// Serves HTTPS on `127.0.0.1`, with a certificate this authority signed, on a thread of its
    /// own, and answers every request `200` with the body `ok`; the port it listens on. A client
    /// that does not trust the authority ends at the handshake.
    pub(crate) fn serve(&self) -> u16 {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::default();
        params.subject_alt_names = vec![rcgen::SanType::IpAddress(Ipv4Addr::LOCALHOST.into())];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params
            .signed_by(&key, &self.certificate, &self.key)
            .unwrap();
        let private = rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der());
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certificate.der().clone()], private.into())
                .unwrap();
        let acceptor = futures_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            smol::block_on(async move {
                let listener = smol::net::TcpListener::try_from(listener).unwrap();
                while let Ok((stream, _)) = listener.accept().await {
                    let acceptor = acceptor.clone();
                    smol::spawn(async move {
                        let Ok(mut tls) = acceptor.accept(stream).await else {
                            return;
                        };
                        let mut request = Vec::new();
                        let mut read = [0u8; 1024];
                        while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                            match tls.read(&mut read).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => request.extend_from_slice(&read[..n]),
                            }
                        }
                        tls.write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await
                        .ok();
                        tls.close().await.ok();
                    })
                    .detach();
                }
            });
        });
        port
    }
}
