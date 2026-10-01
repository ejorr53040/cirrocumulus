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
    registry.tag_index(
        "app",
        "multi",
        &[("linux/arm64", arm), ("linux/amd64", amd.clone())],
    );

    let image = pull(&cache(&registry), &format!("{}/app:multi", registry.host)).unwrap();

    assert_eq!(image.digest, amd);
}

#[test]
fn an_image_without_a_linux_amd64_build_is_refused_by_name() {
    let registry = Registry::start();
    let arm = registry.put_image("app", &Image::new("arm64", vec![app_layer()], app_config()));
    registry.tag("app", "arm-only", &arm);
    // Docker Hub lists attestations as `unknown/unknown`: not platforms.
    registry.tag_index(
        "app",
        "arm-index",
        &[("linux/arm64", arm.clone()), ("unknown/unknown", arm)],
    );
    let cache = cache(&registry);

    let reference = format!("{}/app:arm-only", registry.host);
    let e = pull(&cache, &reference).unwrap_err();
    assert_eq!(
        e.0,
        format!("{reference} is a linux/arm64 image; only linux/amd64 images run here")
    );
    let reference = format!("{}/app:arm-index", registry.host);
    let e = pull(&cache, &reference).unwrap_err();
    assert_eq!(
        e.0,
        format!("{reference} has no linux/amd64 image, only linux/arm64")
    );
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
    registry.tag_index(
        "app",
        "multi",
        &[("linux/arm64", arm), ("linux/amd64", amd)],
    );
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

#[test]
fn a_tag_the_registry_doesnt_have_is_no_image() {
    let (registry, _) = registry_with_app();
    let reference = format!("{}/app:v2", registry.host);

    let e = pull(&cache(&registry), &reference).unwrap_err();

    assert_eq!(
        e.0,
        format!("no image {reference}: the registry has no tag v2 for app")
    );
}

/// Docker Hub answers a repository it doesn't have, and a private one,
/// with 401 to an anonymous pull.
#[test]
fn an_unauthorized_repository_is_no_image_or_a_private_one() {
    let (registry, _) = registry_with_app();
    registry.fail_manifests(401, "UNAUTHORIZED");
    let reference = format!("{}/ghost:latest", registry.host);

    let e = pull(&cache(&registry), &reference).unwrap_err();

    assert_eq!(
        e.0,
        format!(
            "no image {reference}: the registry has no repository ghost, or it's private \
             (only public images can be pulled)"
        )
    );
}

#[test]
fn hitting_the_pull_limit_says_so_and_when_to_retry() {
    let (registry, _) = registry_with_app();
    registry.fail_manifests(429, "TOOMANYREQUESTS");
    let reference = format!("{}/app:v1", registry.host);

    let e = pull(&cache(&registry), &reference).unwrap_err();

    assert!(e.0.starts_with(&format!("can't pull {reference}: ")), "{e}");
    assert!(e.0.contains("pull limit"), "{e}");
    // Docker Hub's allowance is Docker Hub's, not every registry's.
    assert!(!e.0.contains("Docker Hub"), "{e}");
}

#[test]
fn zstd_layers_are_refused_before_any_download() {
    let registry = Registry::start();
    let layer = app_layer();
    let image = Image::new("amd64", vec![layer.clone()], app_config()).zstd();
    let digest = registry.put_image("app", &image);
    registry.tag("app", "zstd", &digest);
    let reference = format!("{}/app:zstd", registry.host);

    let e = pull(&cache(&registry), &reference).unwrap_err();

    assert_eq!(
        e.0,
        format!(
            "can't pull {reference}: its layers are zstd-compressed, which isn't supported yet"
        )
    );
    let layer_path = format!("/v2/app/blobs/{}", super::registry::sha256(&layer));
    assert!(
        !registry.requests().contains(&layer_path),
        "the layer was downloaded"
    );
}

/// A second image of `app`, with no user to resolve.
fn v2_image() -> Image {
    let layer = gzipped(Layer::default().dir("bin").file("bin/app", "app v2"));
    Image::new(
        "amd64",
        vec![layer],
        serde_json::json!({"Cmd": ["/bin/app"]}),
    )
}

#[test]
fn the_cache_lists_each_image_under_the_references_it_was_pulled_as() {
    if !have_mke2fs() {
        return;
    }
    let (registry, v1) = registry_with_app();
    registry.tag("app", "stable", &v1);
    let cache = cache(&registry);
    let host = &registry.host;
    pull(&cache, &format!("{host}/app:v1")).unwrap();
    pull(&cache, &format!("{host}/app:stable")).unwrap();
    // `stable` moves to a new image, and takes its reference with it.
    let v2 = registry.put_image("app", &v2_image());
    registry.tag("app", "stable", &v2);
    pull(&cache, &format!("{host}/app:stable")).unwrap();

    let listed: Vec<(String, Vec<String>)> = cache
        .list()
        .unwrap()
        .into_iter()
        .map(|i| (i.digest, i.references))
        .collect();

    // Listed in reference order.
    assert_eq!(
        listed,
        [
            (v2, vec![format!("{host}/app:stable")]),
            (v1, vec![format!("{host}/app:v1")]),
        ]
    );
}

#[test]
fn an_image_is_removed_by_reference_or_by_a_digest_prefix() {
    if !have_mke2fs() {
        return;
    }
    let (registry, v1) = registry_with_app();
    let v2 = registry.put_image("app", &v2_image());
    registry.tag("app", "v2", &v2);
    let cache = cache(&registry);
    let host = &registry.host;
    let first = pull(&cache, &format!("{host}/app:v1")).unwrap();
    pull(&cache, &format!("{host}/app:v2")).unwrap();

    let removed = cache.remove(&format!("{host}/app:v1")).unwrap();
    assert_eq!(removed.digest, v1);
    assert!(!first.rootfs.exists(), "the rootfs is still there");

    let hex = v2.strip_prefix("sha256:").unwrap();
    assert_eq!(cache.remove(&hex[..12]).unwrap().digest, v2);
    assert!(cache.list().unwrap().is_empty());

    let e = cache.remove("nothing-like-it").unwrap_err();
    assert_eq!(e.0, "no cached image nothing-like-it");
}

/// `nginx:alpine` is an index: its image's manifest and blobs come after
/// the first request, and fail just as clearly.
#[test]
fn the_pull_limit_is_reported_whichever_request_hits_it() {
    let registry = Registry::start();
    let amd = registry.put_image("app", &Image::new("amd64", vec![app_layer()], app_config()));
    registry.tag_index("app", "multi", &[("linux/amd64", amd)]);
    let cache = cache(&registry);
    let reference = format!("{}/app:multi", registry.host);

    for part in ["/manifests/sha256:", "/blobs/"] {
        registry.fail_requests(part, 429, "TOOMANYREQUESTS");
        let e = pull(&cache, &reference).unwrap_err();
        assert!(
            e.0.starts_with(&format!("can't pull {reference}: ")),
            "{part}: {e}"
        );
        assert!(e.0.contains("pull limit"), "{part}: {e}");
    }
}

#[test]
fn a_repository_the_registry_names_unknown_is_no_image() {
    let (registry, _) = registry_with_app();
    registry.fail_manifests(404, "NAME_UNKNOWN");
    let reference = format!("{}/ghost:latest", registry.host);

    let e = pull(&cache(&registry), &reference).unwrap_err();

    assert!(
        e.0.starts_with(&format!(
            "no image {reference}: the registry has no repository ghost"
        )),
        "{e}"
    );
}
