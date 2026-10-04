//! Certificates from an ACME CA (Let's Encrypt by default) for Apps whose
//! hostnames are public, proved over HTTP-01: the CA fetches a token from
//! `http://<host>/.well-known/acme-challenge/<token>`, which the HTTP edge
//! answers (see [`crate::Router::acme_challenge`]), so the edge must be
//! reachable on port 80 under that hostname (ADR 0008).
//!
//! Kept in the agent's state dir, root-only: the account, one per ACME
//! directory (`acme-account-<hash>.json`), and each hostname's key and
//! chain together in one file (`acme-<host>.pem`), so they can't be
//! replaced one without the other. A hostname's file outlives its App's
//! `rm`: running it again reuses the certificate instead of ordering one
//! against the CA's rate limits.

use crate::fs::write_whole;
use crate::tls::HostCertificate;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::sign::CertifiedKey;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::sync::OnceCell;
use tracing::{info, warn};

/// A certificate is ordered again once it has less than this left.
const RENEW_WITHIN: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long to wait for the CA to validate a hostname, and to issue.
const ORDER_TIMEOUT: Duration = Duration::from_secs(90);

/// How long after a failed order the first retry waits; each further
/// failure doubles it, up to [`MAX_RETRY_AFTER`]. Let's Encrypt allows a
/// handful of failed validations per hostname an hour.
const FIRST_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// Top-level domains that are never public: reserved (RFC 2606, 6761, 6762,
/// 7686, 8375, 9476), ICANN's `.internal`, and the private-use names
/// networks commonly use. Their hostnames get the Node CA's certificates.
const NOT_PUBLIC: [&str; 12] = [
    "test",
    "example",
    "invalid",
    "localhost",
    "local",
    "internal",
    "lan",
    "arpa",
    "onion",
    "alt",
    "home",
    "corp",
];

/// Whether `host` can have a certificate from a public ACME CA.
pub fn is_public(host: &str) -> bool {
    host.contains('.')
        && host.rsplit('.').next().is_some_and(|tld| {
            !NOT_PUBLIC.contains(&tld) && !tld.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Where to get certificates, and who for.
pub struct AcmeConfig {
    /// The ACME directory URL.
    pub directory: String,
    /// The account's contact address.
    pub email: String,
    /// A CA certificate (PEM) to trust for the directory, besides the
    /// system's: for a test CA such as Pebble.
    pub root: Option<PathBuf>,
}

/// The ACME client for a Node, and the certificates it has.
pub struct Acme {
    dir: PathBuf,
    config: AcmeConfig,
    account: OnceCell<Account>,
    provider: Arc<CryptoProvider>,
    /// Key authorizations by token, for challenges under way.
    challenges: Mutex<HashMap<String, String>>,
    certificates: Mutex<HashMap<String, HostCertificate>>,
    /// Hostnames with an order under way, so none is ordered twice at once.
    ordering: Mutex<HashSet<String>>,
    /// Hostnames whose last order failed: when to try again, and how long
    /// the wait after that one was.
    failed: Mutex<HashMap<String, (Instant, Duration)>>,
}

impl Acme {
    /// The client for the state dir `dir`, with the certificates it holds.
    pub fn load(dir: &Path, config: AcmeConfig) -> io::Result<Arc<Acme>> {
        let acme = Acme {
            dir: dir.to_path_buf(),
            config,
            account: OnceCell::new(),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            challenges: Mutex::new(HashMap::new()),
            certificates: Mutex::new(HashMap::new()),
            ordering: Mutex::new(HashSet::new()),
            failed: Mutex::new(HashMap::new()),
        };
        for entry in std::fs::read_dir(dir)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(host) = name
                .strip_prefix("acme-")
                .and_then(|n| n.strip_suffix(".pem"))
            else {
                continue;
            };
            match acme.read_certificate(host) {
                Ok(certificate) => {
                    acme.certificates
                        .lock()
                        .unwrap()
                        .insert(host.to_string(), certificate);
                }
                Err(e) => warn!(host, "the stored ACME certificate can't be used: {e}"),
            }
        }
        Ok(Arc::new(acme))
    }

    /// The key authorization for an HTTP-01 challenge token, while its
    /// challenge is under way.
    pub fn challenge(&self, token: &str) -> Option<String> {
        self.challenges.lock().unwrap().get(token).cloned()
    }

    /// `host`'s certificate, if it has an unexpired one.
    pub fn certificate(&self, host: &str) -> Option<Arc<CertifiedKey>> {
        let certificates = self.certificates.lock().unwrap();
        let certificate = certificates.get(host)?;
        certificate
            .lasts(Duration::ZERO)
            .then(|| certificate.certified.clone())
    }

    /// Orders a certificate for `host` unless it has one with more than
    /// [`RENEW_WITHIN`] left, an order for it is under way, or its last
    /// order failed too recently to try again.
    pub async fn ensure(&self, host: &str) -> Result<(), String> {
        let fresh = self
            .certificates
            .lock()
            .unwrap()
            .get(host)
            .is_some_and(|c| c.lasts(RENEW_WITHIN));
        let waiting = self
            .failed
            .lock()
            .unwrap()
            .get(host)
            .is_some_and(|(retry_at, _)| Instant::now() < *retry_at);
        if fresh || waiting {
            return Ok(());
        }
        let Some(mut order) = PendingOrder::start(self, host) else {
            return Ok(());
        };
        match self.order(&mut order).await {
            Ok(certificate) => {
                self.failed.lock().unwrap().remove(host);
                self.certificates
                    .lock()
                    .unwrap()
                    .insert(host.to_string(), certificate);
                info!(host, "got a certificate from the ACME CA");
                Ok(())
            }
            Err(e) => {
                let mut failed = self.failed.lock().unwrap();
                let wait = failed.get(host).map_or(FIRST_RETRY_AFTER, |(_, last)| {
                    (*last * 2).min(MAX_RETRY_AFTER)
                });
                failed.insert(host.to_string(), (Instant::now() + wait, wait));
                Err(format!("{e}; trying again in {} min", wait.as_secs() / 60))
            }
        }
    }

    /// Orders a certificate for the hostname `order` is for, and stores it.
    async fn order(&self, order: &mut PendingOrder<'_>) -> Result<HostCertificate, String> {
        let host = order.host.clone();
        let account = self.account().await?;
        let identifiers = [Identifier::Dns(host.clone())];
        let mut placed = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(|e| format!("place the order: {e}"))?;
        let mut authorizations = placed.authorizations();
        while let Some(authorization) = authorizations.next().await {
            let mut authorization =
                authorization.map_err(|e| format!("read an authorization: {e}"))?;
            match authorization.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                status => return Err(format!("the authorization is {status:?}")),
            }
            let mut challenge = authorization
                .challenge(ChallengeType::Http01)
                .ok_or("the CA offered no HTTP-01 challenge")?;
            order.answer(&challenge.token, challenge.key_authorization().as_str());
            challenge
                .set_ready()
                .await
                .map_err(|e| format!("start the challenge: {e}"))?;
        }
        let retries = RetryPolicy::new().timeout(ORDER_TIMEOUT);
        let status = placed
            .poll_ready(&retries)
            .await
            .map_err(|e| format!("wait for validation: {e}"))?;
        if status != OrderStatus::Ready {
            return Err(format!(
                "the CA didn't validate {host} (order {status:?}): is the HTTP edge reachable \
                 at http://{host}/ on port 80?"
            ));
        }
        let key = placed
            .finalize()
            .await
            .map_err(|e| format!("finalize the order: {e}"))?;
        let chain = placed
            .poll_certificate(&retries)
            .await
            .map_err(|e| format!("fetch the certificate: {e}"))?;
        // Key and chain in one file, written whole: a crash can't leave a
        // new key next to an old chain.
        write_whole(&self.path(&host), format!("{key}{chain}").as_bytes(), 0o600)
            .map_err(|e| format!("store the certificate: {e}"))?;
        self.read_certificate(&host)
    }

    /// The Node's ACME account with the configured directory, made the
    /// first time it is needed and kept.
    async fn account(&self) -> Result<&Account, String> {
        self.account
            .get_or_try_init(|| async {
                let builder = match &self.config.root {
                    Some(root) => Account::builder_with_root(root),
                    None => Account::builder(),
                }
                .map_err(|e| format!("set up the ACME client: {e}"))?;
                // One file per directory, so pointing the agent at another
                // CA makes an account there instead of reusing this one.
                let digest = Sha256::digest(self.config.directory.as_bytes());
                let id: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
                let path = self.dir.join(format!("acme-account-{id}.json"));
                match std::fs::read_to_string(&path) {
                    Ok(json) => {
                        let credentials: AccountCredentials = serde_json::from_str(&json)
                            .map_err(|e| format!("read {}: {e}", path.display()))?;
                        return builder
                            .from_credentials(credentials)
                            .await
                            .map_err(|e| format!("load the ACME account: {e}"));
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("read {}: {e}", path.display())),
                }
                let contact = format!("mailto:{}", self.config.email);
                let (account, credentials) = builder
                    .create(
                        &NewAccount {
                            contact: &[&contact],
                            terms_of_service_agreed: true,
                            only_return_existing: false,
                        },
                        self.config.directory.clone(),
                        None,
                    )
                    .await
                    .map_err(|e| format!("make an ACME account: {e}"))?;
                let json = serde_json::to_string(&credentials).map_err(|e| e.to_string())?;
                write_whole(&path, json.as_bytes(), 0o600)
                    .map_err(|e| format!("store the ACME account: {e}"))?;
                info!(directory = self.config.directory, "made an ACME account");
                Ok(account)
            })
            .await
    }

    /// Where `host`'s key and chain are kept.
    fn path(&self, host: &str) -> PathBuf {
        self.dir.join(format!("acme-{host}.pem"))
    }

    /// `host`'s stored certificate, ready to serve.
    fn read_certificate(&self, host: &str) -> Result<HostCertificate, String> {
        let pem = std::fs::read(self.path(host)).map_err(|e| format!("read it: {e}"))?;
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&pem)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("parse the chain: {e}"))?;
        let leaf = chain.first().ok_or("it holds no certificate")?;
        let (_, parsed) = x509_parser::parse_x509_certificate(leaf)
            .map_err(|e| format!("parse the certificate: {e}"))?;
        let not_after = parsed.validity().not_after.timestamp();
        let key = PrivateKeyDer::from_pem_slice(&pem).map_err(|e| format!("parse the key: {e}"))?;
        let signing_key = self
            .provider
            .key_provider
            .load_private_key(key)
            .map_err(|e| format!("load the key: {e}"))?;
        Ok(HostCertificate {
            certified: Arc::new(CertifiedKey::new(chain, signing_key)),
            expires: UNIX_EPOCH + Duration::from_secs(u64::try_from(not_after).unwrap_or(0)),
        })
    }
}

/// An order under way for one hostname: while it lives no other order for
/// the hostname starts, and the challenges it answers are served. Dropped,
/// however the order ended (panics and cancellation included), it takes
/// both back.
struct PendingOrder<'a> {
    acme: &'a Acme,
    host: String,
    tokens: Vec<String>,
}

impl<'a> PendingOrder<'a> {
    fn start(acme: &'a Acme, host: &str) -> Option<PendingOrder<'a>> {
        acme.ordering
            .lock()
            .unwrap()
            .insert(host.to_string())
            .then(|| PendingOrder {
                acme,
                host: host.to_string(),
                tokens: Vec::new(),
            })
    }

    /// Serves `key_authorization` for `token` until the order ends.
    fn answer(&mut self, token: &str, key_authorization: &str) {
        self.acme
            .challenges
            .lock()
            .unwrap()
            .insert(token.to_string(), key_authorization.to_string());
        self.tokens.push(token.to_string());
    }
}

impl Drop for PendingOrder<'_> {
    fn drop(&mut self) {
        let mut challenges = self.acme.challenges.lock().unwrap();
        for token in &self.tokens {
            challenges.remove(token);
        }
        self.acme.ordering.lock().unwrap().remove(&self.host);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hostnames_under_public_top_level_domains_are_public() {
        for public in ["app.example.com", "cirro.dev", "a.b.co.uk"] {
            assert!(is_public(public), "{public}");
        }
        for private in [
            "web.test",
            "nas.local",
            "db.internal",
            "printer.lan",
            "localhost",
            "x.example",
            "10.0.0.1",
            "router.home",
            "hidden.onion",
        ] {
            assert!(!is_public(private), "{private}");
        }
    }
}
