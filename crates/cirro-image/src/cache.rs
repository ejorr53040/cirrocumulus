//! Pulling images from a registry into a per-user cache of built rootfs.
//!
//! The cache holds one `sha256-<hex>.ext4` and `sha256-<hex>.json` per
//! image manifest digest: the rootfs, and a record of the run config and
//! the references last pulled as that image. Pulls are anonymous, or carry
//! a bearer token from the registry's own token service (`registry_auth`),
//! `linux/amd64` only, gzip or plain layers (never zstd), and every blob is
//! checked against its digest by `oci-client` before it's used.

use crate::run_config::{self, RunConfig};
use crate::{Error, Flattened, LIMITS, Limits, err, human};
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::config::ConfigFile;
use oci_client::errors::{OciDistributionError, OciErrorCode};
use oci_client::manifest::{OciDescriptor, OciImageManifest, OciManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use reqwest::Url;
use reqwest::header::{HeaderValue, WWW_AUTHENTICATE};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};
use tokio::io::AsyncWrite;

/// The only platform a Node runs.
const PLATFORM: &str = "linux/amd64";

/// A pulled image, built and ready to boot.
#[derive(Debug)]
pub struct CachedImage {
    /// The digest of the image's (single-platform) manifest.
    pub digest: String,
    pub rootfs: PathBuf,
    /// What the image runs, and how, unless `cirro run` says otherwise.
    pub config: RunConfig,
    /// The references (`docker.io/library/nginx:alpine`) last pulled as
    /// this image, so they can be found again without a registry. Empty
    /// once every tag has moved on to another image.
    pub references: Vec<String>,
}

/// `<digest>.json`: what the cache knows about an image besides its rootfs.
#[derive(Serialize, Deserialize)]
struct Record {
    references: Vec<String>,
    config: RunConfig,
}

/// Built images, one per manifest digest, in a directory the caller owns.
pub struct ImageCache {
    dir: PathBuf,
    plain_http: Vec<String>,
}

impl ImageCache {
    /// The cache in `dir`, which is created on the first pull. Registries
    /// are reached over HTTPS.
    pub fn new(dir: PathBuf) -> ImageCache {
        ImageCache {
            dir,
            plain_http: Vec::new(),
        }
    }

    /// Reach these registries (`host:port`) over plain HTTP instead of HTTPS.
    #[cfg(test)]
    pub(crate) fn with_plain_http(mut self, registries: Vec<String>) -> ImageCache {
        self.plain_http = registries;
        self
    }

    /// Pulls `reference` (`nginx:alpine`, `ghcr.io/o/app@sha256:…`) and
    /// builds its rootfs with `init` as `/init`, unless it's cached.
    pub async fn pull(&self, reference: &str, init: &[u8]) -> Result<CachedImage, Error> {
        self.pull_within(reference, init, LIMITS).await
    }

    /// [`ImageCache::pull`], with `limits` in place of [`LIMITS`].
    pub(crate) async fn pull_within(
        &self,
        reference: &str,
        init: &[u8],
        limits: Limits,
    ) -> Result<CachedImage, Error> {
        let reference: Reference = reference
            .parse()
            .map_err(|e| err(&format!("image reference {reference:?}"), e))?;
        let digest = self.fetch(&reference, init, limits).await?;
        self.remember(&reference.whole(), &digest)
    }

    pub fn list(&self) -> Result<Vec<CachedImage>, Error> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(err(&format!("read {}", self.dir.display()), e)),
        };
        let mut images = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| err(&format!("read {}", self.dir.display()), e))?;
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            let Some((algorithm, hex)) = stem.split_once('-') else {
                continue;
            };
            if let Some(image) = self.cached(&format!("{algorithm}:{hex}"))? {
                images.push(image);
            }
        }
        images.sort_by(|a, b| (&a.references, &a.digest).cmp(&(&b.references, &b.digest)));
        Ok(images)
    }

    /// Removes the cached image `image` names: a reference it was pulled as
    /// (`nginx:alpine`), or its digest or the start of one (`sha256:1a2b…`,
    /// `1a2b3c`).
    pub fn remove(&self, image: &str) -> Result<CachedImage, Error> {
        let images = self.list()?;
        let reference = image.parse::<Reference>().ok().map(|r| r.whole());
        let mut matches: Vec<CachedImage> = images
            .into_iter()
            .filter(|i| {
                let by_reference = reference.as_ref().is_some_and(|r| i.references.contains(r));
                let hex = image.strip_prefix("sha256:").unwrap_or(image);
                let by_digest = hex.len() >= 4
                    && hex.chars().all(|c| c.is_ascii_hexdigit())
                    && i.digest
                        .strip_prefix("sha256:")
                        .is_some_and(|d| d.starts_with(hex));
                by_reference || by_digest
            })
            .collect();
        let found = match matches.len() {
            0 => return Err(Error(format!("no cached image {image}"))),
            1 => matches.remove(0),
            _ => {
                let digests: Vec<&str> = matches.iter().map(|i| i.digest.as_str()).collect();
                return Err(Error(format!(
                    "{image} matches more than one cached image: {}",
                    digests.join(", ")
                )));
            }
        };
        // The rootfs goes first: a record without one is an unfinished pull,
        // which the cache already ignores.
        for path in [&found.rootfs, &self.path(&found.digest, "json")] {
            std::fs::remove_file(path)
                .map_err(|e| err(&format!("remove {}", path.display()), e))?;
        }
        Ok(found)
    }

    /// Records that `reference` names the cached image `digest`, and no
    /// longer whichever image it named before.
    fn remember(&self, reference: &str, digest: &str) -> Result<CachedImage, Error> {
        for mut image in self.list()? {
            let had = image.references.len();
            image.references.retain(|r| r != reference);
            if image.digest == digest {
                image.references.push(reference.to_string());
                image.references.sort();
            }
            if image.references.len() != had || image.digest == digest {
                self.write_record(&image)?;
            }
        }
        self.cached(digest)?
            .ok_or_else(|| Error(format!("{digest} vanished from {}", self.dir.display())))
    }

    /// Writes `image`'s record in one step, so a reader never sees half.
    fn write_record(&self, image: &CachedImage) -> Result<(), Error> {
        let record = Record {
            references: image.references.clone(),
            config: image.config.clone(),
        };
        let path = self.path(&image.digest, "json");
        let partial = path.with_extension(format!("json.{}", unique()));
        let bytes = serde_json::to_vec_pretty(&record).map_err(|e| err("encode a record", e))?;
        std::fs::write(&partial, bytes)
            .and_then(|()| std::fs::rename(&partial, &path))
            .map_err(|e| err(&format!("write {}", path.display()), e))
    }

    /// Resolves `reference` to an image manifest digest and builds that
    /// image, unless it's cached already.
    async fn fetch(
        &self,
        reference: &Reference,
        init: &[u8],
        limits: Limits,
    ) -> Result<String, Error> {
        let mut client = self.client();
        let auth = self.registry_auth(reference).await?;
        let mut authed = Instant::now();
        let (digest, manifest) = self.resolve(&client, reference, &auth).await?;
        let Some(manifest) = manifest else {
            return Ok(digest);
        };
        if manifest
            .layers
            .iter()
            .any(|l| l.media_type.contains("zstd"))
        {
            return Err(Error(format!(
                "can't pull {reference}: its layers are zstd-compressed, which isn't supported yet"
            )));
        }

        std::fs::create_dir_all(&self.dir)
            .map_err(|e| err(&format!("create {}", self.dir.display()), e))?;
        let scratch = Scratch::new(&self.dir)?;

        let mut config = Vec::new();
        let what = format!("{reference}'s config");
        download(
            &client,
            reference,
            &manifest.config,
            &mut config,
            limits.config,
            &what,
        )
        .await?;
        let config: ConfigFile = serde_json::from_slice(&config)
            .map_err(|e| err(&format!("{reference}'s config"), e))?;
        let platform = platform(&config.os, &config.architecture);
        if platform != PLATFORM {
            return Err(Error(format!(
                "{reference} is a {platform} image; only {PLATFORM} images run here"
            )));
        }

        let mut layers = Vec::new();
        for (i, layer) in manifest.layers.iter().enumerate() {
            // oci-client keeps using a bearer token it was handed after the
            // registry lets it lapse, so a long pull gets a new one between
            // layers.
            if authed.elapsed() > TOKEN_REFRESH {
                client = self.client();
                let auth = self.registry_auth(reference).await?;
                client
                    .store_auth_if_needed(reference.resolve_registry(), &auth)
                    .await;
                authed = Instant::now();
            }
            let path = scratch.0.join(format!("blob-{i}"));
            let file = tokio::fs::File::create(&path)
                .await
                .map_err(|e| err(&format!("create {}", path.display()), e))?;
            let what = format!("{reference}'s layer {}", layer.digest);
            download(&client, reference, layer, file, limits.blob, &what).await?;
            layers.push(path);
        }

        let flat = Flattened::open_within(&layers, &scratch.0.join("flat"), limits)?;
        let config = run_config(&config, &flat)?;
        let built = scratch.0.join("rootfs.ext4");
        flat.build_ext4(init, &scratch.0, &built)?;

        let rootfs = self.path(&digest, "ext4");
        self.write_record(&CachedImage {
            digest: digest.clone(),
            rootfs: rootfs.clone(),
            config,
            references: Vec::new(),
        })?;
        std::fs::rename(&built, &rootfs)
            .map_err(|e| err(&format!("move the rootfs to {}", rootfs.display()), e))?;
        Ok(digest)
    }

    /// `reference`'s `linux/amd64` image manifest digest and, unless that
    /// image is cached, its manifest. A cached tag costs one manifest
    /// request, or two for an index, after `registry_auth`'s probe.
    async fn resolve(
        &self,
        client: &Client,
        reference: &Reference,
        auth: &RegistryAuth,
    ) -> Result<(String, Option<OciImageManifest>), Error> {
        let (manifest, digest) = client
            .pull_manifest(reference, auth)
            .await
            .map_err(|e| pull_error(reference, &reference.to_string(), e))?;
        let manifest = match manifest {
            OciManifest::Image(manifest) => manifest,
            OciManifest::ImageIndex(index) => {
                let entry = index.manifests.iter().find(|m| {
                    m.platform
                        .as_ref()
                        .is_some_and(|p| platform(&p.os, &p.architecture) == PLATFORM)
                });
                let Some(digest) = entry.map(|m| m.digest.clone()) else {
                    let found: Vec<String> = index
                        .manifests
                        .iter()
                        .filter_map(|m| m.platform.as_ref())
                        .map(|p| platform(&p.os, &p.architecture))
                        // Attestation manifests, not images.
                        .filter(|p| p != "unknown/unknown")
                        .collect();
                    return Err(Error(format!(
                        "{reference} has no {PLATFORM} image, only {}",
                        found.join(", ")
                    )));
                };
                // The registry's own words, about to name a file in the cache.
                let hex = digest.strip_prefix("sha256:").unwrap_or("");
                if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(Error(format!(
                        "{reference}'s {PLATFORM} entry {digest:?} isn't a digest (sha256:<64 hex>)"
                    )));
                }
                // An index costs a second request only when its image isn't cached.
                if self.cached(&digest)?.is_some() {
                    return Ok((digest, None));
                }
                let image = reference.clone_with_digest(digest.clone());
                return match client.pull_manifest(&image, auth).await {
                    Ok((OciManifest::Image(manifest), _)) => Ok((digest, Some(manifest))),
                    Ok((OciManifest::ImageIndex(_), _)) => Err(Error(format!(
                        "{reference}'s {PLATFORM} entry is another index, not an image"
                    ))),
                    Err(e) => Err(pull_error(reference, &image.to_string(), e)),
                };
            }
        };
        let cached = self.cached(&digest)?.is_some();
        Ok((digest, (!cached).then_some(manifest)))
    }

    /// How `reference`'s registry wants pulls authenticated. A registry that
    /// challenges for a bearer token gets one from the realm it names, if that
    /// realm is its own (see [`allowed_realm`]).
    ///
    /// It is always a bearer token, empty when the probe got no challenge:
    /// with anything else oci-client probes again and fetches a token from
    /// whatever realm that second answer names, unchecked, so a registry
    /// that challenged only oci-client's probe would pick the realm itself.
    /// A registry that wants no token ignores the empty one.
    async fn registry_auth(&self, reference: &Reference) -> Result<RegistryAuth, Error> {
        let registry = reference.resolve_registry();
        let scheme = if self.plain_http.iter().any(|r| r == registry) {
            "http"
        } else {
            "https"
        };
        let probe = Url::parse(&format!("{scheme}://{registry}/v2/"))
            .map_err(|e| err(&format!("{reference}'s registry {registry}"), e))?;
        let http = http_client()?;
        let res = http
            .get(probe.clone())
            .send()
            .await
            .map_err(|e| err(&format!("ask {registry} for a token"), e))?;
        let challenge = res
            .headers()
            .get_all(WWW_AUTHENTICATE)
            .iter()
            .find_map(bearer_challenge);
        let Some((realm, service)) = challenge else {
            return Ok(RegistryAuth::Bearer(String::new()));
        };
        let Some(realm) = allowed_realm(&probe, &realm) else {
            return Err(Error(format!(
                "{reference}: {registry} asks for a token from {realm}, which isn't on its domain"
            )));
        };
        let mut query = vec![(
            "scope",
            format!("repository:{}:pull", reference.repository()),
        )];
        if let Some(service) = service {
            query.push(("service", service));
        }
        let mut res = http
            .get(realm.clone())
            .query(&query)
            .send()
            .await
            .map_err(|e| err(&format!("{reference}: token from {realm}"), e))?;
        if res.status() != reqwest::StatusCode::OK {
            return Err(Error(format!(
                "{reference}: {registry}'s token service {realm} refused ({})",
                res.status()
            )));
        }
        let mut body = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|e| err(&format!("{reference}: token from {realm}"), e))?
        {
            if body.len() + chunk.len() > TOKEN_LIMIT {
                return Err(Error(format!(
                    "{reference}: the token from {realm} is bigger than {TOKEN_LIMIT} bytes"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let token: TokenResponse = serde_json::from_slice(&body)
            .map_err(|e| err(&format!("{reference}: token from {realm}"), e))?;
        let token = token
            .token
            .or(token.access_token)
            .ok_or_else(|| Error(format!("{reference}: {realm} sent no token")))?;
        // oci-client unwraps the header it builds from the token.
        if HeaderValue::from_str(&token).is_err() {
            return Err(Error(format!(
                "{reference}: {realm} sent a token that can't go in a header"
            )));
        }
        Ok(RegistryAuth::Bearer(token))
    }

    /// The image built from manifest `digest`, if it's in the cache. The
    /// rootfs is moved in last, so a record without one is a pull that
    /// didn't finish and is pulled again. A record that can't be read is
    /// an error, not a reason to pull again over it.
    fn cached(&self, digest: &str) -> Result<Option<CachedImage>, Error> {
        let rootfs = self.path(digest, "ext4");
        let path = self.path(digest, "json");
        let record = match std::fs::read(&path) {
            Ok(record) => record,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(err(&format!("read {}", path.display()), e)),
        };
        let record: Record = serde_json::from_slice(&record).map_err(|e| {
            err(
                &format!(
                    "{} is corrupt (remove it to pull the image again)",
                    path.display()
                ),
                e,
            )
        })?;
        Ok(rootfs.is_file().then(|| CachedImage {
            digest: digest.to_string(),
            rootfs,
            config: record.config,
            references: record.references,
        }))
    }

    fn client(&self) -> Client {
        install_ring();
        Client::new(ClientConfig {
            protocol: ClientProtocol::HttpsExcept(self.plain_http.clone()),
            // `pull` resolves an index itself, to check the cache first.
            platform_resolver: None,
            ..Default::default()
        })
    }

    /// `<dir>/sha256-<hex>.<extension>` for a manifest digest.
    fn path(&self, digest: &str, extension: &str) -> PathBuf {
        self.dir
            .join(format!("{}.{extension}", digest.replace(':', "-")))
    }
}

/// Most a token service may answer with; a token is a few kilobytes.
const TOKEN_LIMIT: usize = 64 << 10;

/// How long a pull uses one registry token before asking for another. A
/// token lasts 60 s unless its service says otherwise (distribution spec),
/// and `TokenResponse` doesn't read `expires_in`, so this stays under that.
const TOKEN_REFRESH: Duration = Duration::from_secs(50);

#[derive(Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

/// The client for the probe and the token request. It doesn't follow
/// redirects: a redirect would send the request to a host the realm check
/// never saw.
fn http_client() -> Result<reqwest::Client, Error> {
    install_ring();
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| err("start an HTTP client", e))
}

/// reqwest is built without a default TLS provider, so `aws-lc-rs` (C and
/// cmake) stays out of the build; use `ring`'s. Fails only if a provider is
/// already installed, which is just as good.
fn install_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The realm and service of a `Bearer` challenge, if `value` is one.
fn bearer_challenge(value: &HeaderValue) -> Option<(String, Option<String>)> {
    let (scheme, params) = value.to_str().ok()?.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let (mut realm, mut service) = (None, None);
    for param in params.split(',') {
        let Some((key, val)) = param.split_once('=') else {
            continue;
        };
        let val = val.trim().trim_matches('"').to_string();
        match key.trim() {
            k if k.eq_ignore_ascii_case("realm") => realm = Some(val),
            k if k.eq_ignore_ascii_case("service") => service = Some(val),
            _ => {}
        }
    }
    Some((realm?, service))
}

/// Registries whose token service is on another host of their own. Any
/// other registry's realm must be on the registry's own origin.
const TOKEN_HOSTS: &[(&str, &str)] = &[
    ("index.docker.io", "auth.docker.io"),
    ("registry-1.docker.io", "auth.docker.io"),
    ("registry.gitlab.com", "gitlab.com"),
];

/// `realm` as a URL, if a token may be requested from it: the registry's
/// own scheme, host and port, or the token host [`TOKEN_HOSTS`] names for it.
/// Hosts that merely share a domain don't share an owner (tenants of one
/// PaaS domain, names under `co.uk`), so a hostile registry could otherwise
/// point the pull at one of them.
fn allowed_realm(registry: &Url, realm: &str) -> Option<Url> {
    let realm = Url::parse(realm).ok()?;
    let (host, registry_host) = (realm.host_str()?, registry.host_str()?);
    let same_origin =
        host == registry_host && realm.port_or_known_default() == registry.port_or_known_default();
    let token_host = realm.port().is_none()
        && TOKEN_HOSTS
            .iter()
            .any(|&(r, t)| r == registry_host && t == host);
    (realm.scheme() == registry.scheme() && (same_origin || token_host)).then_some(realm)
}

/// What went wrong pulling `what` (a manifest or blob of `reference`),
/// said for a person.
fn pull_error(reference: &Reference, what: &str, e: OciDistributionError) -> Error {
    let codes: Vec<&OciErrorCode> = match &e {
        OciDistributionError::RegistryError { envelope, .. } => {
            envelope.errors.iter().map(|e| &e.code).collect()
        }
        _ => Vec::new(),
    };
    if codes.contains(&&OciErrorCode::ManifestUnknown) {
        let wanted = match (reference.digest(), reference.tag()) {
            (Some(digest), _) => format!("manifest {digest}"),
            (None, tag) => format!("tag {}", tag.unwrap_or("latest")),
        };
        return Error(format!(
            "no image {reference}: the registry has no {wanted} for {}",
            reference.repository()
        ));
    }
    let limited = codes.contains(&&OciErrorCode::Toomanyrequests)
        || matches!(e, OciDistributionError::ServerError { code: 429, .. });
    if limited {
        let allowance = if reference.registry() == "docker.io" {
            " (Docker Hub allows about 100 pulls per 6 hours per IP address)"
        } else {
            ""
        };
        return Error(format!(
            "can't pull {reference}: the registry's anonymous pull limit was \
             reached{allowance}; try again later, or run an image that's already cached \
             (cirro image ls)"
        ));
    }
    let unauthorized = matches!(e, OciDistributionError::UnauthorizedError { .. })
        || codes.contains(&&OciErrorCode::NameUnknown)
        || codes.contains(&&OciErrorCode::Unauthorized)
        || codes.contains(&&OciErrorCode::Denied);
    if unauthorized {
        return Error(format!(
            "no image {reference}: the registry has no repository {}, or it's private \
             (only public images can be pulled)",
            reference.repository()
        ));
    }
    err(&format!("pull {what}"), e)
}

/// Downloads `blob` into `out`, which takes at most `max` bytes. A blob the
/// manifest says is too big isn't requested at all, and one that turns out
/// to be is cut off at `max` rather than read to the end. Only the registry
/// is asked: a descriptor's `urls` (a foreign layer's other homes) would
/// let the registry point the pull at any URL.
async fn download<W: AsyncWrite + Unpin>(
    client: &Client,
    reference: &Reference,
    blob: &OciDescriptor,
    out: W,
    max: u64,
    what: &str,
) -> Result<(), Error> {
    let too_big = || Error(format!("{what} is bigger than {}", human(max)));
    if !u64::try_from(blob.size).is_ok_and(|size| size <= max) {
        return Err(too_big());
    }
    let mut out = Capped {
        inner: out,
        left: max,
        over: false,
    };
    let blob = OciDescriptor {
        urls: None,
        ..blob.clone()
    };
    let pulled = client.pull_blob(reference, &blob, &mut out).await;
    if out.over {
        return Err(too_big());
    }
    pulled.map_err(|e| pull_error(reference, what, e))
}

/// Passes writes on to `inner` until `left` bytes have gone through. The
/// write that would pass that is refused, and `over` records why.
struct Capped<W> {
    inner: W,
    left: u64,
    over: bool,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Capped<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.len() as u64 > self.left {
            self.over = true;
            return Poll::Ready(Err(std::io::Error::other("over the size limit")));
        }
        let n = ready!(Pin::new(&mut self.inner).poll_write(cx, buf))?;
        self.left -= n as u64;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// `<pid>-<n>`, different for every call in every process, so concurrent
/// pulls never share a scratch dir or a half-written record.
pub(crate) fn unique() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

fn platform(os: &impl std::fmt::Display, architecture: &impl std::fmt::Display) -> String {
    format!("{os}/{architecture}")
}

/// What Docker would run, from the image's config, with its
/// user resolved against the image's own `/etc/passwd` and `/etc/group`.
fn run_config(file: &ConfigFile, flat: &Flattened) -> Result<RunConfig, Error> {
    let Some(config) = &file.config else {
        return Ok(RunConfig::default());
    };
    let text = |path| {
        Ok::<_, Error>(
            flat.read(path)?
                .map(|b| String::from_utf8_lossy(&b).into_owned()),
        )
    };
    let user = run_config::resolve_user(
        config.user.as_deref().unwrap_or(""),
        text("/etc/passwd")?.as_deref(),
        text("/etc/group")?.as_deref(),
    )?;
    Ok(RunConfig {
        entrypoint: config.entrypoint.clone().unwrap_or_default(),
        cmd: config.cmd.clone().unwrap_or_default(),
        env: config.env.clone().unwrap_or_default(),
        workdir: config.working_dir.clone().filter(|d| !d.is_empty()),
        user,
    })
}

/// A directory in the cache for one pull's downloads and intermediate
/// files, removed when the pull ends, however it ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(cache: &Path) -> Result<Scratch, Error> {
        let path = cache.join(format!(".pull-{}", unique()));
        // Left by an earlier process with this pid that didn't get to clean up.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).map_err(|e| err("create the pull's scratch dir", e))?;
        Ok(Scratch(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod capped_tests {
    use super::Capped;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn a_write_past_the_limit_is_refused_and_flagged() {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let mut out = Capped {
                    inner: Vec::new(),
                    left: 4,
                    over: false,
                };
                out.write_all(b"abc").await.unwrap();
                out.write_all(b"d").await.unwrap();
                assert!(out.write_all(b"ef").await.is_err());
                assert!(out.over);
                assert_eq!(out.inner, b"abcd");
            });
    }
}

#[cfg(test)]
mod realm_tests {
    use super::{allowed_realm, bearer_challenge};
    use reqwest::Url;
    use reqwest::header::HeaderValue;

    #[test]
    fn a_realm_on_the_registrys_domain_is_allowed() {
        let probe = Url::parse("https://registry-1.docker.io/v2/").unwrap();
        assert!(allowed_realm(&probe, "https://auth.docker.io/token").is_some());
        assert!(allowed_realm(&probe, "https://registry-1.docker.io/token").is_some());
    }

    #[test]
    fn a_realm_elsewhere_or_on_another_scheme_is_refused() {
        let probe = Url::parse("https://registry-1.docker.io/v2/").unwrap();
        assert!(allowed_realm(&probe, "https://evil.example/token").is_none());
        assert!(allowed_realm(&probe, "http://auth.docker.io/token").is_none());
        assert!(allowed_realm(&probe, "not a url").is_none());
    }

    #[test]
    fn addresses_match_only_themselves() {
        let probe = Url::parse("https://10.0.0.1/v2/").unwrap();
        assert!(allowed_realm(&probe, "https://10.0.0.1/token").is_some());
        assert!(allowed_realm(&probe, "https://10.0.0.2/token").is_none());
    }

    #[test]
    fn a_realm_on_a_neighbour_under_a_shared_domain_or_another_port_is_refused() {
        // Tenants of one PaaS domain, or hosts under a multi-label public
        // suffix, share their last two labels without sharing an owner.
        let probe = Url::parse("https://evil.herokuapp.com/v2/").unwrap();
        assert!(allowed_realm(&probe, "https://victim.herokuapp.com/token").is_none());
        let probe = Url::parse("https://evil-reg.co.uk/v2/").unwrap();
        assert!(allowed_realm(&probe, "https://intranet.acme.co.uk/token").is_none());
        let probe = Url::parse("https://registry.example:5000/v2/").unwrap();
        assert!(allowed_realm(&probe, "https://registry.example:6000/token").is_none());
    }

    #[test]
    fn a_bearer_challenge_gives_its_realm_and_service() {
        let value = HeaderValue::from_static(
            r#"Bearer realm="https://auth.example/token",service="registry.example""#,
        );
        assert_eq!(
            bearer_challenge(&value),
            Some((
                "https://auth.example/token".to_string(),
                Some("registry.example".to_string())
            ))
        );
    }

    #[test]
    fn a_basic_challenge_gives_nothing() {
        let value = HeaderValue::from_static(r#"Basic realm="registry""#);
        assert_eq!(bearer_challenge(&value), None);
    }
}
