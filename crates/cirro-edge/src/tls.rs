//! TLS for the edge: a CA of the Node's own, made once and kept in the
//! agent's state dir, and a certificate per hostname it signs on demand,
//! picked by the client's SNI.
//!
//! The CA has no name constraints: whoever can run an App picks its
//! hostname, so a client that trusts the CA (`cirro node ca`) trusts the
//! Node for any name at all, not just its Apps'. Trust it only on machines
//! that would trust the Node's operators that far.

use crate::acme::{Acme, is_public};
use crate::fs::write_whole;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    date_time_ymd,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ServerConfig, version};
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tracing::{debug, info, warn};

/// How long a certificate the CA signs for a hostname is valid.
const LEAF_VALIDITY: Duration = Duration::from_secs(397 * 24 * 60 * 60);

/// A hostname's certificate is signed again once it has less than this left.
const RESIGN_WITHIN: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The Node's CA: its certificate, and the issuer that signs hostnames'
/// certificates with its key.
pub struct NodeCa {
    cert_pem: String,
    issuer: Issuer<'static, KeyPair>,
}

impl NodeCa {
    /// Loads the CA from `edge-ca.pem` and `edge-ca.key` in `dir`, making it
    /// first if there is none. The key is written root-only, and each file
    /// appears whole or not at all, the key first.
    pub fn load_or_create(dir: &Path) -> io::Result<NodeCa> {
        let (cert_path, key_path) = (dir.join("edge-ca.pem"), dir.join("edge-ca.key"));
        let key = match std::fs::read_to_string(&key_path) {
            Ok(pem) => KeyPair::from_pem(&pem).map_err(|e| {
                io::Error::other(format!(
                    "{} isn't a key the edge's CA can use ({e}); remove it and {} to make a \
                     new CA",
                    key_path.display(),
                    cert_path.display()
                ))
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if cert_path.exists() {
                    return Err(io::Error::other(format!(
                        "{} has no key next to it at {}; remove it to make a new CA",
                        cert_path.display(),
                        key_path.display()
                    )));
                }
                let key = KeyPair::generate().map_err(io::Error::other)?;
                write_whole(&key_path, key.serialize_pem().as_bytes(), 0o600)?;
                key
            }
            Err(e) => {
                return Err(io::Error::other(format!(
                    "read {}: {e}",
                    key_path.display()
                )));
            }
        };
        let cert_pem = match std::fs::read_to_string(&cert_path) {
            Ok(pem) => pem,
            // A key without its certificate: an earlier start stopped
            // between the two. The certificate is made from the key.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let cert = ca_params()
                    .self_signed(&key)
                    .map_err(|e| io::Error::other(format!("make the Node's CA: {e}")))?;
                write_whole(&cert_path, cert.pem().as_bytes(), 0o644)?;
                info!(path = %cert_path.display(), "made the Node's CA for the TLS edge");
                cert.pem()
            }
            Err(e) => {
                return Err(io::Error::other(format!(
                    "read {}: {e}",
                    cert_path.display()
                )));
            }
        };
        let issuer = Issuer::from_ca_cert_pem(&cert_pem, key).map_err(|e| {
            io::Error::other(format!(
                "{} isn't a CA certificate ({e}); remove it and its key to make a new CA",
                cert_path.display()
            ))
        })?;
        Ok(NodeCa { cert_pem, issuer })
    }

    /// The CA certificate, as PEM, for clients to trust.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// A certificate for `host`, signed by the CA.
    fn certify(&self, host: &str, provider: &CryptoProvider) -> Result<HostCertificate, String> {
        let key = KeyPair::generate().map_err(|e| e.to_string())?;
        let mut params =
            CertificateParams::new(vec![host.to_string()]).map_err(|e| e.to_string())?;
        params.distinguished_name.push(DnType::CommonName, host);
        let now = SystemTime::now();
        let not_after = now + LEAF_VALIDITY;
        // An hour early, for clients whose clocks are a little behind.
        params.not_before = (now - Duration::from_secs(60 * 60)).into();
        params.not_after = not_after.into();
        let cert = params
            .signed_by(&key, &self.issuer)
            .map_err(|e| e.to_string())?;
        let signing_key = provider
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                key.serialize_der(),
            )))
            .map_err(|e| e.to_string())?;
        let certified =
            CertifiedKey::new(vec![CertificateDer::from(cert.der().to_vec())], signing_key);
        Ok(HostCertificate {
            certified: Arc::new(certified),
            expires: not_after,
        })
    }
}

/// A hostname's certificate, with its key, ready to serve.
pub(crate) struct HostCertificate {
    pub(crate) certified: Arc<CertifiedKey>,
    pub(crate) expires: SystemTime,
}

impl HostCertificate {
    /// Whether it is still valid `margin` from now.
    pub(crate) fn lasts(&self, margin: Duration) -> bool {
        self.expires > SystemTime::now() + margin
    }
}

/// The CA certificate's parameters, used once, to make it.
fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, "Cirrocumulus Node CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.not_before = date_time_ymd(2026, 1, 1);
    params.not_after = date_time_ymd(2046, 1, 1);
    params
}

/// The edge's TLS configuration: a certificate for each hostname `routed`
/// says an App has. With `acme`, a public hostname's comes from the ACME
/// CA, and until it has one the handshake fails; every other hostname's is
/// signed by `ca` the first time a client asks for it. A hostname no App
/// has gets no certificate, so the handshake fails.
pub fn server_config(
    ca: Arc<NodeCa>,
    acme: Option<Arc<Acme>>,
    routed: impl Fn(&str) -> bool + Send + Sync + 'static,
) -> io::Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&version::TLS13, &version::TLS12])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(HostCertificates {
            ca,
            acme,
            provider,
            routed: Box::new(routed),
            signed: Mutex::new(HashMap::new()),
        }));
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Picks, and signs when it must, the certificate for a client's SNI.
struct HostCertificates {
    ca: Arc<NodeCa>,
    acme: Option<Arc<Acme>>,
    provider: Arc<CryptoProvider>,
    routed: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// Each hostname's certificate.
    signed: Mutex<HashMap<String, HostCertificate>>,
}

impl std::fmt::Debug for HostCertificates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostCertificates")
    }
}

impl ResolvesServerCert for HostCertificates {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let Some(name) = hello.server_name() else {
            // Every App is reached by hostname, so a client that names none
            // (or an IP address) has nothing to be given.
            debug!("TLS client sent no SNI hostname");
            return None;
        };
        let host = name.trim_end_matches('.').to_ascii_lowercase();
        if !(self.routed)(&host) {
            return None;
        }
        if let Some(acme) = &self.acme
            && is_public(&host)
        {
            return acme.certificate(&host);
        }
        let mut signed = self.signed.lock().unwrap();
        if let Some(certificate) = signed.get(&host)
            && certificate.lasts(RESIGN_WITHIN)
        {
            return Some(certificate.certified.clone());
        }
        match self.ca.certify(&host, &self.provider) {
            Ok(certificate) => {
                let certified = certificate.certified.clone();
                signed.insert(host, certificate);
                Some(certified)
            }
            Err(e) => {
                warn!(host, "sign a certificate: {e}");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cirro-edge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_ca_is_made_once_and_loaded_after() {
        let dir = tempdir("once");
        let made = NodeCa::load_or_create(&dir).unwrap();
        let loaded = NodeCa::load_or_create(&dir).unwrap();
        assert_eq!(made.cert_pem(), loaded.cert_pem());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_key_left_without_its_certificate_gets_one() {
        let dir = tempdir("key-only");
        NodeCa::load_or_create(&dir).unwrap();
        let key = std::fs::read(dir.join("edge-ca.key")).unwrap();
        std::fs::remove_file(dir.join("edge-ca.pem")).unwrap();

        let ca = NodeCa::load_or_create(&dir).unwrap();
        assert!(ca.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));
        assert_eq!(std::fs::read(dir.join("edge-ca.key")).unwrap(), key);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_certificate_left_without_its_key_is_refused_with_what_to_do() {
        let dir = tempdir("cert-only");
        NodeCa::load_or_create(&dir).unwrap();
        std::fs::remove_file(dir.join("edge-ca.key")).unwrap();

        let err = NodeCa::load_or_create(&dir)
            .err()
            .expect("refused")
            .to_string();
        assert!(
            err.contains("edge-ca.pem") && err.contains("remove it"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
