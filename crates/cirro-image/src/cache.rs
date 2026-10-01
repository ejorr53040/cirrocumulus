//! Pulling images from a registry into a per-user cache of built rootfs.
//!
//! The cache holds one `sha256-<hex>.ext4` and `sha256-<hex>.json` per
//! image manifest digest. Pulls are anonymous, `linux/amd64` only, and every blob
//! is checked against its digest by `oci-client` before it's used.

use crate::run_config::{self, RunConfig};
use crate::{Error, Flattened, err};
use oci_client::client::{ClientConfig, ClientProtocol, linux_amd64_resolver};
use oci_client::config::ConfigFile;
use oci_client::manifest::OciManifest;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
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
    /// builds its rootfs with `init` as `/init`.
    pub async fn pull(&self, reference: &str, init: &[u8]) -> Result<CachedImage, Error> {
        let reference: Reference = reference
            .parse()
            .map_err(|e| err(&format!("image reference {reference:?}"), e))?;
        let client = self.client();
        let auth = RegistryAuth::Anonymous;
        let (manifest, digest) = client
            .pull_manifest(&reference, &auth)
            .await
            .map_err(|e| err(&format!("pull {reference}"), e))?;
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
                        .collect();
                    return Err(Error(format!(
                        "{reference} has no {PLATFORM} image, only {}",
                        found.join(", ")
                    )));
                };
                if let Some(cached) = self.cached(&digest) {
                    return Ok(cached);
                }
                let image = reference.clone_with_digest(digest.clone());
                match client.pull_manifest(&image, &auth).await {
                    Ok((OciManifest::Image(manifest), _)) => (manifest, digest),
                    Ok((OciManifest::ImageIndex(_), _)) => {
                        return Err(Error(format!(
                            "{reference}'s {PLATFORM} entry is another index, not an image"
                        )));
                    }
                    Err(e) => return Err(err(&format!("pull {image}"), e)),
                }
            }
        };
        if let Some(cached) = self.cached(&digest) {
            return Ok(cached);
        }

        std::fs::create_dir_all(&self.dir)
            .map_err(|e| err(&format!("create {}", self.dir.display()), e))?;
        let scratch = Scratch::new(&self.dir)?;

        let mut config = Vec::new();
        client
            .pull_blob(&reference, &manifest.config, &mut config)
            .await
            .map_err(|e| err(&format!("pull {reference}'s config"), e))?;
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
                .pull_blob(&reference, layer, file)
                .await
                .map_err(|e| err(&format!("pull {reference}'s layer {}", layer.digest), e))?;
            layers.push(path);
        }

        let flat = Flattened::open(&layers, &scratch.0.join("flat"))?;
        let config = run_config(&config, &flat)?;
        let built = scratch.0.join("rootfs.ext4");
        flat.build_ext4(init, &scratch.0, &built)?;

        let rootfs = self.path(&digest, "ext4");
        std::fs::write(
            self.path(&digest, "json"),
            serde_json::to_vec_pretty(&config).map_err(|e| err("encode the run config", e))?,
        )
        .map_err(|e| err("write the run config", e))?;
        std::fs::rename(&built, &rootfs)
            .map_err(|e| err(&format!("move the rootfs to {}", rootfs.display()), e))?;
        Ok(CachedImage {
            digest,
            rootfs,
            config,
        })
    }

    /// The image built from manifest `digest`, if it's in the cache. The
    /// rootfs is moved in last, so a config without one is a pull that
    /// didn't finish and is pulled again.
    fn cached(&self, digest: &str) -> Option<CachedImage> {
        let rootfs = self.path(digest, "ext4");
        let config = std::fs::read(self.path(digest, "json")).ok()?;
        let config = serde_json::from_slice(&config).ok()?;
        rootfs.is_file().then(|| CachedImage {
            digest: digest.to_string(),
            rootfs,
            config,
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
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = cache.join(format!(".pull-{}-{n}", std::process::id()));
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
