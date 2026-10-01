//! Pulling images from a registry into a per-user cache of built rootfs.
//!
//! The cache holds one `sha256-<hex>.ext4` and `sha256-<hex>.json` per
//! image manifest digest: the rootfs, and a record of the run config and
//! the references last pulled as that image. Pulls are anonymous,
//! `linux/amd64` only, gzip or plain layers (never zstd), and every blob is
//! checked against its digest by `oci-client` before it's used.

use crate::run_config::{self, RunConfig};
use crate::{Error, Flattened, err};
use oci_client::client::{ClientConfig, ClientProtocol, linux_amd64_resolver};
use oci_client::config::ConfigFile;
use oci_client::errors::{OciDistributionError, OciErrorCode};
use oci_client::manifest::OciManifest;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// The only platform a Node runs.
const PLATFORM: &str = "linux/amd64";

/// A pulled image, built and ready to boot.
#[derive(Debug)]
pub struct CachedImage {
    /// The digest of the image's (single-platform) manifest.
    pub digest: String,
    /// The ext4 rootfs.
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
    pub fn with_plain_http(mut self, registries: Vec<String>) -> ImageCache {
        self.plain_http = registries;
        self
    }

    /// Pulls `reference` (`nginx:alpine`, `ghcr.io/o/app@sha256:…`) and
    /// builds its rootfs with `init` as `/init`, unless it's cached.
    pub async fn pull(&self, reference: &str, init: &[u8]) -> Result<CachedImage, Error> {
        let reference: Reference = reference
            .parse()
            .map_err(|e| err(&format!("image reference {reference:?}"), e))?;
        let digest = self.fetch(&reference, init).await?;
        self.remember(&reference.whole(), &digest)
    }

    /// Every image in the cache.
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
            if let Some(image) = stem
                .split_once('-')
                .and_then(|(algorithm, hex)| self.cached(&format!("{algorithm}:{hex}")))
            {
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
        self.cached(digest)
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
    async fn fetch(&self, reference: &Reference, init: &[u8]) -> Result<String, Error> {
        let client = self.client();
        let auth = RegistryAuth::Anonymous;
        let (manifest, digest) = client
            .pull_manifest(reference, &auth)
            .await
            .map_err(|e| pull_error(reference, &reference.to_string(), e))?;
        // An index costs a second request only when its image isn't cached.
        let (manifest, digest) = match manifest {
            OciManifest::Image(manifest) => (manifest, digest),
            OciManifest::ImageIndex(index) => {
                let Some(digest) = linux_amd64_resolver(&index.manifests) else {
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
                if self.cached(&digest).is_some() {
                    return Ok(digest);
                }
                let image = reference.clone_with_digest(digest.clone());
                match client.pull_manifest(&image, &auth).await {
                    Ok((OciManifest::Image(manifest), _)) => (manifest, digest),
                    Ok((OciManifest::ImageIndex(_), _)) => {
                        return Err(Error(format!(
                            "{reference}'s {PLATFORM} entry is another index, not an image"
                        )));
                    }
                    Err(e) => return Err(pull_error(reference, &image.to_string(), e)),
                }
            }
        };
        if self.cached(&digest).is_some() {
            return Ok(digest);
        }
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
        client
            .pull_blob(reference, &manifest.config, &mut config)
            .await
            .map_err(|e| pull_error(reference, &format!("{reference}'s config"), e))?;
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
            let path = scratch.0.join(format!("blob-{i}"));
            let file = tokio::fs::File::create(&path)
                .await
                .map_err(|e| err(&format!("create {}", path.display()), e))?;
            client
                .pull_blob(reference, layer, file)
                .await
                .map_err(|e| {
                    pull_error(
                        reference,
                        &format!("{reference}'s layer {}", layer.digest),
                        e,
                    )
                })?;
            layers.push(path);
        }

        let flat = Flattened::open(&layers, &scratch.0.join("flat"))?;
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

    /// The image built from manifest `digest`, if it's in the cache. The
    /// rootfs is moved in last, so a record without one is a pull that
    /// didn't finish and is pulled again.
    fn cached(&self, digest: &str) -> Option<CachedImage> {
        let rootfs = self.path(digest, "ext4");
        let record = std::fs::read(self.path(digest, "json")).ok()?;
        let record: Record = serde_json::from_slice(&record).ok()?;
        rootfs.is_file().then(|| CachedImage {
            digest: digest.to_string(),
            rootfs,
            config: record.config,
            references: record.references,
        })
    }

    fn client(&self) -> Client {
        // reqwest is built without a default TLS provider, so `aws-lc-rs`
        // (C and cmake) stays out of the build; use `ring`'s. Fails only if
        // a provider is already installed, which is just as good.
        let _ = rustls::crypto::ring::default_provider().install_default();
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

/// `<pid>-<n>`, different for every call in every process, so concurrent
/// pulls never share a scratch dir or a half-written record.
fn unique() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

/// `linux/amd64` and the like.
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
        flat.read(path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    let user = run_config::resolve_user(
        config.user.as_deref().unwrap_or(""),
        text("/etc/passwd").as_deref(),
        text("/etc/group").as_deref(),
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
