//! A tiny OCI distribution registry on localhost, over plain HTTP, serving
//! images the test pushes into it and recording every request it gets.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

pub(super) fn sha256(data: &[u8]) -> String {
    let hex: String = Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

#[derive(Default)]
struct Content {
    /// `<repo>/<tag or digest>` → (media type, body).
    manifests: HashMap<String, (&'static str, Vec<u8>)>,
    /// digest → body.
    blobs: HashMap<String, Vec<u8>>,
    /// `GET <path>` of every request, in order.
    requests: Vec<String>,
    /// When set, every request whose path contains `.0` gets status `.1`
    /// and an error envelope with code `.2`.
    failure: Option<(String, u16, String)>,
}

pub(super) struct Registry {
    pub(super) host: String,
    content: Arc<Mutex<Content>>,
}

/// One platform's image: its gzipped layers, bottom first, and its config.
pub(super) struct Image {
    pub(super) layers: Vec<Vec<u8>>,
    pub(super) config: serde_json::Value,
    /// The media type the manifest gives every layer.
    pub(super) layer_media_type: &'static str,
}

impl Image {
    /// An image for `architecture` whose `config` section is `config`.
    pub(super) fn new(architecture: &str, layers: Vec<Vec<u8>>, config: serde_json::Value) -> Self {
        Image {
            layers,
            config: serde_json::json!({
                "architecture": architecture,
                "os": "linux",
                "config": config,
                "rootfs": {"type": "layers", "diff_ids": []},
            }),
            layer_media_type: LAYER_GZIP,
        }
    }

    /// The same image, with its layers listed as zstd-compressed.
    pub(super) fn zstd(self) -> Self {
        Image {
            layer_media_type: "application/vnd.oci.image.layer.v1.tar+zstd",
            ..self
        }
    }
}

impl Registry {
    pub(super) fn start() -> Registry {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let content = Arc::new(Mutex::new(Content::default()));
        let shared = content.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &shared);
            }
        });
        Registry { host, content }
    }

    /// Paths requested so far, such as `/v2/app/blobs/sha256:…`.
    pub(super) fn requests(&self) -> Vec<String> {
        self.content.lock().unwrap().requests.clone()
    }

    /// Stores `image`'s blobs and manifest; returns the manifest's digest.
    pub(super) fn put_image(&self, repo: &str, image: &Image) -> String {
        let config = serde_json::to_vec(&image.config).unwrap();
        let layers: Vec<serde_json::Value> = image
            .layers
            .iter()
            .map(|l| {
                serde_json::json!({"mediaType": image.layer_media_type, "digest": self.put_blob(l), "size": l.len()})
            })
            .collect();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": MANIFEST,
            "config": {"mediaType": CONFIG, "digest": self.put_blob(&config), "size": config.len()},
            "layers": layers,
        });
        self.put_manifest(repo, MANIFEST, serde_json::to_vec(&manifest).unwrap())
    }

    /// Stores an index over `images` (`os/architecture`, manifest digest)
    /// and tags it; returns the index's digest.
    pub(super) fn tag_index(&self, repo: &str, tag: &str, images: &[(&str, String)]) -> String {
        let manifests: Vec<serde_json::Value> = images
            .iter()
            .map(|(platform, digest)| {
                let (os, arch) = platform.split_once('/').unwrap();
                let size = self.content.lock().unwrap().manifests[&format!("{repo}/{digest}")]
                    .1
                    .len();
                serde_json::json!({
                    "mediaType": MANIFEST, "digest": digest, "size": size,
                    "platform": {"architecture": arch, "os": os},
                })
            })
            .collect();
        let index =
            serde_json::json!({"schemaVersion": 2, "mediaType": INDEX, "manifests": manifests});
        let digest = self.put_manifest(repo, INDEX, serde_json::to_vec(&index).unwrap());
        self.tag(repo, tag, &digest);
        digest
    }

    /// Points `repo:tag` at the manifest or index `digest`.
    pub(super) fn tag(&self, repo: &str, tag: &str, digest: &str) {
        let mut content = self.content.lock().unwrap();
        let manifest = content.manifests[&format!("{repo}/{digest}")].clone();
        content.manifests.insert(format!("{repo}/{tag}"), manifest);
    }

    /// Answers every manifest request from now on with HTTP `status` and an
    /// OCI error envelope with `code` (`UNAUTHORIZED`, `TOOMANYREQUESTS`).
    pub(super) fn fail_manifests(&self, status: u16, code: &str) {
        self.fail_requests("/manifests/", status, code);
    }

    /// Like [`Registry::fail_manifests`], for requests whose path contains
    /// `part`.
    pub(super) fn fail_requests(&self, part: &str, status: u16, code: &str) {
        self.content.lock().unwrap().failure = Some((part.to_string(), status, code.to_string()));
    }

    /// Replaces the bytes served for blob `digest`, keeping its digest.
    pub(super) fn corrupt_blob(&self, digest: &str) {
        let mut content = self.content.lock().unwrap();
        let blob = content.blobs.get_mut(digest).unwrap();
        blob[0] ^= 0xff;
    }

    fn put_blob(&self, data: &[u8]) -> String {
        let digest = sha256(data);
        self.content
            .lock()
            .unwrap()
            .blobs
            .insert(digest.clone(), data.to_vec());
        digest
    }

    fn put_manifest(&self, repo: &str, media_type: &'static str, body: Vec<u8>) -> String {
        let digest = sha256(&body);
        self.content
            .lock()
            .unwrap()
            .manifests
            .insert(format!("{repo}/{digest}"), (media_type, body));
        digest
    }
}

fn serve(stream: TcpStream, content: &Mutex<Content>) {
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(n) if n > 0 && header != "\r\n" => {}
            _ => break,
        }
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .to_string();
    let mut content = content.lock().unwrap();
    content.requests.push(path.clone());
    if let Some((_, status, code)) = content
        .failure
        .clone()
        .filter(|(part, _, _)| path.contains(part.as_str()))
    {
        drop(content);
        let body = format!(r#"{{"errors":[{{"code":"{code}","message":"{code}"}}]}}"#);
        let mut stream = &stream;
        let _ = write!(
            stream,
            "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        return;
    }
    let found = if path == "/v2/" {
        Some(("application/json", b"{}".to_vec(), None))
    } else if let Some((repo, reference)) = path
        .strip_prefix("/v2/")
        .and_then(|p| p.split_once("/manifests/"))
    {
        content
            .manifests
            .get(&format!("{repo}/{reference}"))
            .map(|(media_type, body)| (*media_type, body.clone(), Some(sha256(body))))
    } else if let Some((_, digest)) = path
        .strip_prefix("/v2/")
        .and_then(|p| p.split_once("/blobs/"))
    {
        content
            .blobs
            .get(digest)
            .map(|body| ("application/octet-stream", body.clone(), None))
    } else {
        None
    };
    drop(content);
    let mut stream = &stream;
    let _ = match found {
        Some((media_type, body, digest)) => {
            let digest_header = digest
                .map(|d| format!("Docker-Content-Digest: {d}\r\n"))
                .unwrap_or_default();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\n\
                 {digest_header}Connection: close\r\n\r\n",
                body.len()
            )
            .and_then(|()| stream.write_all(&body))
        }
        None => {
            let body = br#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"not found"}]}"#;
            write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .and_then(|()| stream.write_all(body))
        }
    };
}
