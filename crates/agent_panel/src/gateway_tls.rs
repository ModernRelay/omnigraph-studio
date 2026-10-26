//! The gateway's TLS (spec 013). The agent's CLI reaches its conversation's gateway over HTTPS
//! on `127.0.0.1`, trusting only the certificate the gateway made when it started: its CLI home
//! names that certificate as the one CA (`transport.tls.ca`, `mode: replace`), so the CLI's
//! system roots never apply and no other process on the machine can stand in for the gateway.
//!
//! The certificate is self-signed for the address alone (`IP:127.0.0.1`; the CLI matches an IP
//! endpoint against IP names only) and valid for a year. Its private key is made in memory and
//! never written: it dies with the gateway, and a gateway started again makes a new one. So a
//! shorter validity would protect nothing, and the certificate is never renewed. A gateway that
//! outlived it would see the CLI's TLS error reach the agent.

use std::net::Ipv4Addr;
use std::sync::Arc;

use futures_rustls::rustls::{self, ServerConfig};

/// How long the certificate is valid from the day before the gateway starts: a year after it.
const VALID_FOR: chrono::Days = chrono::Days::new(366);

/// What the gateway serves its TLS with: the certificate (PEM, which the CLI home writes as
/// `ca.pem`) and the server's configuration, which holds the private key.
pub struct Identity {
    pub certificate: String,
    pub config: Arc<ServerConfig>,
}

/// The crypto provider of the gateway's TLS and its tests' clients: Studio's graph builds rustls
/// with both ring and aws-lc-rs, so none is the process default.
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn invalid(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(format!("the gateway's certificate: {error}"))
}

/// A new key and a certificate for it, valid for `127.0.0.1` from the day before today (UTC, so
/// a clock a few hours off still takes it) for a year.
pub fn identity() -> std::io::Result<Identity> {
    let key = rcgen::KeyPair::generate().map_err(invalid)?;
    let mut params = rcgen::CertificateParams::default();
    params.subject_alt_names = vec![rcgen::SanType::IpAddress(Ipv4Addr::LOCALHOST.into())];
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, "Omnigraph Studio agent gateway");
    params.distinguished_name = name;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    // Midnight UTC of `date`, as a certificate's time.
    let day = |date: chrono::NaiveDate| {
        use chrono::Datelike as _;
        rcgen::date_time_ymd(date.year(), date.month() as u8, date.day() as u8)
    };
    let from = chrono::Utc::now().date_naive() - chrono::Days::new(1);
    params.not_before = day(from);
    params.not_after = day(from + VALID_FOR);
    let certificate = params.self_signed(&key).map_err(invalid)?;
    let private = rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der());
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(invalid)?
        .with_no_client_auth()
        .with_single_cert(vec![certificate.der().clone()], private.into())
        .map_err(invalid)?;
    // HTTP/1.1, one request per connection, as the gateway reads them; each CLI run is a new
    // process, so a resumed session would never be used.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(Identity {
        certificate: certificate.pem(),
        config: Arc::new(config),
    })
}

/// A client's configuration that trusts only the certificates in `pem`, as the CLI does with
/// `mode: replace`.
#[cfg(test)]
pub(crate) fn client_trusting(pem: &str) -> Arc<rustls::ClientConfig> {
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots(pem))
            .with_no_client_auth(),
    )
}

#[cfg(test)]
fn roots(pem: &str) -> rustls::RootCertStore {
    use rustls::pki_types::pem::PemObject as _;
    let mut roots = rustls::RootCertStore::empty();
    for certificate in rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes()) {
        roots.add(certificate.unwrap()).unwrap();
    }
    roots
}

/// The gateway's address as a TLS client names it.
#[cfg(test)]
pub(crate) fn loopback() -> rustls::pki_types::ServerName<'static> {
    rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier as _;
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    /// The certificate names `127.0.0.1` alone, as an IP address, and is valid from the day
    /// before it was made for a year: the CLI's verifier, which trusts it as its one CA, takes it
    /// for that address then and refuses it for any other name and outside that time. Each
    /// gateway makes a key of its own.
    #[test]
    fn the_certificate_is_for_the_loopback_address_for_a_year() {
        let identity = identity().unwrap();
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots(&identity.certificate)),
            provider(),
        )
        .build()
        .unwrap();
        let certificate = CertificateDer::from_pem_slice(identity.certificate.as_bytes()).unwrap();
        let day = 24 * 60 * 60;
        let now = UnixTime::now().as_secs();
        let at = |seconds: u64| UnixTime::since_unix_epoch(std::time::Duration::from_secs(seconds));
        let check = |name: &ServerName<'_>, seconds: u64| {
            verifier.verify_server_cert(&certificate, &[], name, &[], at(seconds))
        };
        assert!(check(&loopback(), now).is_ok());
        assert!(check(&loopback(), now + 360 * day).is_ok());
        assert!(check(&loopback(), now + 368 * day).is_err(), "past a year");
        assert!(
            check(&loopback(), now - 3 * day).is_err(),
            "before it was made"
        );
        for other in ["localhost", "127.0.0.2", "::1", "studio.local"] {
            let name = ServerName::try_from(other).unwrap();
            assert!(check(&name, now).is_err(), "{other}");
        }
        assert!(!identity.certificate.contains("PRIVATE KEY"));
        assert_ne!(identity.certificate, super::identity().unwrap().certificate);
    }
}
