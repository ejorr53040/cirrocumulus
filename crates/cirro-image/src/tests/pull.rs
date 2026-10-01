//! Pulling through [`ImageCache`] from a fake registry on localhost.

use super::registry::{Image, Registry};
use super::*;
use crate::ImageCache;
use crate::run_config::RunConfig;

const INIT: &[u8] = b"#!guest-init";

fn gzipped(layer: Layer) -> Vec<u8> {
    let dir = tempdir();
    std::fs::read(layer.write(&dir, "layer.tar.gz", true)).unwrap()
}

fn app_layer() -> Vec<u8> {
    gzipped(
        Layer::default()
            .dir("bin")
            .file("bin/app", "app v1")
            .dir("etc")
            .file(
                "etc/passwd",
                "root:x:0:0::/root:/bin/sh\napp:x:101:102::/srv:/bin/sh\n",
            )
            .file("etc/group", "root:x:0:\napp:x:102:\n"),
    )
}

fn app_config() -> serde_json::Value {
    serde_json::json!({
        "Entrypoint": ["/bin/app"],
        "Cmd": ["serve"],
        "Env": ["PATH=/bin"],
        "WorkingDir": "/srv",
        "User": "app",
    })
}

/// A registry with `app:v1`, a linux/amd64 image; returns it and the
/// image's manifest digest.
fn registry_with_app() -> (Registry, String) {
    let registry = Registry::start();
    let digest = registry.put_image("app", &Image::new("amd64", vec![app_layer()], app_config()));
    registry.tag("app", "v1", &digest);
    (registry, digest)
}

fn cache(registry: &Registry) -> ImageCache {
    ImageCache::new(tempdir().join("images")).with_plain_http(vec![registry.host.clone()])
}

fn pull(cache: &ImageCache, reference: &str) -> Result<crate::CachedImage, Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(cache.pull(reference, INIT))
}

fn have_mke2fs() -> bool {
    match check_mke2fs() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("skipping: {e}");
            false
        }
    }
}

#[test]
fn pulling_a_tag_builds_the_rootfs_and_reads_the_run_config() {
    if !have_mke2fs() {
        return;
    }
    let (registry, digest) = registry_with_app();

    let image = pull(&cache(&registry), &format!("{}/app:v1", registry.host)).unwrap();

    assert_eq!(image.digest, digest);
    assert_eq!(debugfs(&image.rootfs, "cat /bin/app"), "app v1");
    assert_eq!(debugfs(&image.rootfs, "cat /init"), "#!guest-init");
    assert_eq!(
        image.config,
        RunConfig {
            entrypoint: vec!["/bin/app".into()],
            cmd: vec!["serve".into()],
            env: vec!["PATH=/bin".into()],
            workdir: Some("/srv".into()),
            user: Some((101, 102)),
        }
    );
}

#[test]
fn pulling_a_cached_image_again_fetches_only_its_manifest() {
    if !have_mke2fs() {
        return;
    }
    let (registry, digest) = registry_with_app();
    let cache = cache(&registry);
    let first = pull(&cache, &format!("{}/app:v1", registry.host)).unwrap();
    let before = registry.requests().len();

    let again = pull(&cache, &format!("{}/app:v1", registry.host)).unwrap();

    let since: Vec<String> = registry.requests()[before..].to_vec();
    assert!(
        since.iter().all(|p| !p.contains("/blobs/")),
        "a cached image downloaded blobs again: {since:?}"
    );
    assert!(
        since.contains(&"/v2/app/manifests/v1".to_string()),
        "{since:?}"
    );
    assert_eq!(again.digest, digest);
    assert_eq!(again.rootfs, first.rootfs);
    assert_eq!(again.config, first.config);
}

#[test]
fn an_index_resolves_to_its_linux_amd64_image() {
    if !have_mke2fs() {
        return;
    }
    let registry = Registry::start();
    let arm = registry.put_image("app", &Image::new("arm64", vec![app_layer()], app_config()));
    let amd = registry.put_image("app", &Image::new("amd64", vec![app_layer()], app_config()));
    registry.tag_index("app", "multi", &[("arm64", arm), ("amd64", amd.clone())]);

    let image = pull(&cache(&registry), &format!("{}/app:multi", registry.host)).unwrap();

    assert_eq!(image.digest, amd);
}

#[test]
fn an_image_without_a_linux_amd64_build_is_refused_by_name() {
    let registry = Registry::start();
    let arm = registry.put_image("app", &Image::new("arm64", vec![app_layer()], app_config()));
    registry.tag("app", "arm-only", &arm);
    registry.tag_index("app", "arm-index", &[("arm64", arm)]);
    let cache = cache(&registry);

    for tag in ["arm-only", "arm-index"] {
        let e = pull(&cache, &format!("{}/app:{tag}", registry.host)).unwrap_err();
        assert!(e.0.contains("linux/amd64"), "{tag}: {e}");
    }
}

#[test]
fn a_blob_that_doesnt_match_its_digest_is_refused_and_nothing_is_cached() {
    let registry = Registry::start();
    let layer = app_layer();
    let digest = registry.put_image(
        "app",
        &Image::new("amd64", vec![layer.clone()], app_config()),
    );
    registry.tag("app", "v1", &digest);
    registry.corrupt_blob(&super::registry::sha256(&layer));
    let dir = tempdir().join("images");
    let cache = ImageCache::new(dir.clone()).with_plain_http(vec![registry.host.clone()]);

    let e = pull(&cache, &format!("{}/app:v1", registry.host)).unwrap_err();

    assert!(e.0.to_lowercase().contains("digest"), "{e}");
    let left: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "left in the cache: {left:?}");
}

#[test]
fn pulling_a_cached_index_again_fetches_only_the_index() {
    if !have_mke2fs() {
        return;
    }
    let registry = Registry::start();
    let arm = registry.put_image("app", &Image::new("arm64", vec![app_layer()], app_config()));
    let amd = registry.put_image("app", &Image::new("amd64", vec![app_layer()], app_config()));
    registry.tag_index("app", "multi", &[("arm64", arm), ("amd64", amd)]);
    let cache = cache(&registry);
    pull(&cache, &format!("{}/app:multi", registry.host)).unwrap();
    let before = registry.requests().len();

    pull(&cache, &format!("{}/app:multi", registry.host)).unwrap();

    let since: Vec<String> = registry.requests()[before..]
        .iter()
        .filter(|p| p.as_str() != "/v2/")
        .cloned()
        .collect();
    assert_eq!(since, ["/v2/app/manifests/multi"]);
}
