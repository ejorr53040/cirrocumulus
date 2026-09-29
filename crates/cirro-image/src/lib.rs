//! OCI image layers → a bootable ext4 rootfs for a Cirrocumulus guest (M4).
//!
//! [`Flattened::open`] reads an image's layers, bottom first, and works out
//! the final tree: later layers replace earlier entries, and `.wh.<name>`
//! whiteouts and `.wh..wh..opq` opaque directories remove them, as the OCI
//! image spec's layer rules say. [`Flattened::build_ext4`] writes that tree,
//! plus what every guest needs (guest-init as `/init`, its mountpoints, DNS),
//! as one tar in path order and has `mke2fs -d` turn it into ext4.
//!
//! Nothing here needs root. `mke2fs` (e2fsprogs 1.47.1 or later) takes each
//! file's owner and mode from the tar, so a build run as any user keeps the
//! image's ownership and setuid bits. Image layers are untrusted input, which
//! is why this runs in the unprivileged CLI and never in the Node agent.

pub mod run_config;

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use tar::{EntryType, Header};

/// The oldest `mke2fs` that can build a filesystem from a tarball.
pub const MIN_MKE2FS: (u32, u32, u32) = (1, 47, 1);

/// Written as the guest's `/etc/resolv.conf`: the guest gets its address
/// from the kernel command line, which carries no DNS server.
const RESOLV_CONF: &[u8] = b"nameserver 1.1.1.1\nnameserver 8.8.8.8\n";
const HOSTS: &[u8] = b"127.0.0.1\tlocalhost\n::1\tlocalhost\n";

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err(context: &str, e: impl std::fmt::Display) -> Error {
    Error(format!("{context}: {e}"))
}

/// Where one entry of the final tree comes from.
#[derive(Debug)]
struct Source {
    /// Index into `Flattened::layers`.
    layer: usize,
    header: Header,
    /// Offset of the entry's data in its uncompressed layer.
    data: u64,
    link_name: Option<PathBuf>,
}

/// An image's layers merged into its final tree.
#[derive(Debug)]
pub struct Flattened {
    /// The uncompressed layers, bottom first, in the scratch dir.
    layers: Vec<PathBuf>,
    /// Every path in the final tree (normalised: relative, no `.` or `..`),
    /// in path order, so a directory always precedes what's in it.
    tree: BTreeMap<String, Source>,
}

impl Flattened {
    /// Merges `layers` (bottom first; each a tar, plain or gzip-compressed).
    /// Their uncompressed copies go in `scratch`, which the caller owns and
    /// removes after [`Flattened::build_ext4`].
    pub fn open(layers: &[PathBuf], scratch: &Path) -> Result<Flattened, Error> {
        std::fs::create_dir_all(scratch).map_err(|e| err("create the scratch dir", e))?;
        let mut flat = Flattened {
            layers: Vec::new(),
            tree: BTreeMap::new(),
        };
        for (i, layer) in layers.iter().enumerate() {
            let plain = scratch.join(format!("layer-{i}.tar"));
            decompress(layer, &plain)?;
            flat.layers.push(plain);
            flat.apply(i)
                .map_err(|e| Error(format!("layer {}: {e}", i + 1)))?;
        }
        Ok(flat)
    }

    /// Applies layer `i` on top of the tree built from the layers below it.
    fn apply(&mut self, i: usize) -> Result<(), Error> {
        let file = File::open(&self.layers[i]).map_err(|e| err("open", e))?;
        let mut archive = tar::Archive::new(BufReader::new(file));
        let mut entries = Vec::new();
        for entry in archive.entries_with_seek().map_err(|e| err("read", e))? {
            let entry = entry.map_err(|e| err("read an entry", e))?;
            let path = entry.path().map_err(|e| err("read a path", e))?;
            let path = normalise(&path)?;
            let link_name = entry
                .link_name()
                .map_err(|e| err(&format!("read {path}'s link"), e))?
                .map(|l| l.into_owned());
            entries.push((
                path,
                Source {
                    layer: i,
                    header: entry.header().clone(),
                    data: entry.raw_file_position(),
                    link_name,
                },
            ));
        }
        // A layer's whiteouts hide only what's below it, so they go first.
        for (path, _) in &entries {
            let (dir, name) = split(path);
            if name == ".wh..wh..opq" {
                self.remove_children(dir);
            } else if let Some(hidden) = name.strip_prefix(".wh.") {
                self.remove(&join(dir, hidden));
            }
        }
        for (path, source) in entries {
            if split(&path).1.starts_with(".wh.") {
                continue;
            }
            let kind = source.header.entry_type();
            if !matches!(
                kind,
                EntryType::Regular
                    | EntryType::Continuous
                    | EntryType::Directory
                    | EntryType::Symlink
                    | EntryType::Link
                    | EntryType::Char
                    | EntryType::Block
                    | EntryType::Fifo
            ) {
                // PAX and GNU long-name records are folded into the entry
                // that follows them by `tar`, so these are the rest.
                return Err(Error(format!(
                    "{path}: unsupported tar entry type {kind:?}"
                )));
            }
            if kind == EntryType::Link {
                let target = source.link_name.as_deref().unwrap_or(Path::new(""));
                normalise(target)?;
            }
            if path.is_empty() {
                continue; // the root itself: `/` always exists
            }
            // Anything but a directory replaces a whole subtree; a directory
            // over a directory merges with it.
            let was_dir = self
                .tree
                .get(&path)
                .is_some_and(|s| s.header.entry_type() == EntryType::Directory);
            if kind != EntryType::Directory || !was_dir {
                self.remove_children(&path);
            }
            self.tree.insert(path, source);
        }
        Ok(())
    }

    fn remove(&mut self, path: &str) {
        self.tree.remove(path);
        self.remove_children(path);
    }

    fn remove_children(&mut self, dir: &str) {
        if dir.is_empty() {
            self.tree.clear();
            return;
        }
        let prefix = format!("{dir}/");
        let children: Vec<String> = self
            .tree
            .range(prefix.clone()..)
            .take_while(|(p, _)| p.starts_with(&prefix))
            .map(|(p, _)| p.clone())
            .collect();
        for child in children {
            self.tree.remove(&child);
        }
    }

    /// The contents of the regular file at `path` in the final tree, if
    /// there is one. `path` may start with `/`.
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let source = self.tree.get(path.trim_start_matches('/'))?;
        if !matches!(
            source.header.entry_type(),
            EntryType::Regular | EntryType::Continuous
        ) {
            return None;
        }
        let mut data = Vec::new();
        self.data(source).ok()?.read_to_end(&mut data).ok()?;
        Some(data)
    }

    fn data(&self, source: &Source) -> io::Result<impl Read> {
        let mut file = File::open(&self.layers[source.layer])?;
        file.seek(SeekFrom::Start(source.data))?;
        Ok(file.take(source.header.entry_size()?))
    }

    /// Writes the final tree, with guest-init as `/init` and the rest of
    /// what a guest needs, as an ext4 image at `dest`. `scratch` holds the
    /// intermediate tar.
    pub fn build_ext4(&self, init: &[u8], scratch: &Path, dest: &Path) -> Result<(), Error> {
        check_mke2fs()?;
        let tree = self.with_guest_files(init);
        let tar_path = scratch.join("rootfs.tar");
        let bytes = self
            .write_tar(&tree, &tar_path)
            .map_err(|e| err("write the rootfs tar", e))?;

        // Room for ext4's own metadata and for the app to write a little.
        let size = (bytes + bytes / 4).max(64 << 20);
        let image =
            File::create(dest).map_err(|e| err(&format!("create {}", dest.display()), e))?;
        image
            .set_len(size)
            .map_err(|e| err(&format!("size {}", dest.display()), e))?;
        let output = Command::new("mke2fs")
            .args(["-q", "-F", "-t", "ext4", "-d"])
            .arg(&tar_path)
            .arg(dest)
            .output()
            .map_err(|e| err("run mke2fs", e))?;
        let _ = std::fs::remove_file(&tar_path);
        if !output.status.success() {
            let _ = std::fs::remove_file(dest);
            return Err(Error(format!(
                "mke2fs failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    /// The tree to write: the image's own, the files every guest gets
    /// (replacing the image's), and a directory entry for every parent the
    /// layers didn't list.
    fn with_guest_files(&self, init: &[u8]) -> BTreeMap<String, Entry<'_>> {
        let mut tree: BTreeMap<String, Entry<'_>> = self
            .tree
            .iter()
            .map(|(path, source)| (path.clone(), Entry::Layer(source)))
            .collect();
        tree.insert("init".into(), Entry::Guest(init.to_vec(), 0o755));
        tree.insert(
            "etc/resolv.conf".into(),
            Entry::Guest(RESOLV_CONF.to_vec(), 0o644),
        );
        tree.insert("etc/hosts".into(), Entry::Guest(HOSTS.to_vec(), 0o644));
        for dir in ["proc", "sys", "dev"] {
            tree.entry(dir.into()).or_insert(Entry::Dir);
        }
        let parents: Vec<String> = tree
            .keys()
            .flat_map(|path| {
                path.match_indices('/')
                    .map(|(i, _)| path[..i].to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        for parent in parents {
            tree.entry(parent).or_insert(Entry::Dir);
        }
        tree
    }

    /// Writes `tree` as a tar in path order, hardlinks last so each one's
    /// target already exists. Returns the space the files need, roughly.
    fn write_tar(&self, tree: &BTreeMap<String, Entry<'_>>, dest: &Path) -> io::Result<u64> {
        let mut builder = tar::Builder::new(BufWriter::new(File::create(dest)?));
        let mut bytes = 0u64;
        let mut hardlinks = Vec::new();
        for (path, entry) in tree {
            // Every inode costs at least a block.
            bytes += 4096;
            match entry {
                Entry::Dir => {
                    let mut header = Header::new_gnu();
                    header.set_entry_type(EntryType::Directory);
                    header.set_mode(0o755);
                    header.set_size(0);
                    builder.append_data(&mut header, path, io::empty())?;
                }
                Entry::Guest(contents, mode) => {
                    let mut header = Header::new_gnu();
                    header.set_entry_type(EntryType::Regular);
                    header.set_mode(*mode);
                    header.set_size(contents.len() as u64);
                    bytes += contents.len() as u64;
                    builder.append_data(&mut header, path, contents.as_slice())?;
                }
                Entry::Layer(source) => {
                    let mut header = source.header.clone();
                    match header.entry_type() {
                        EntryType::Link => hardlinks.push((path, source)),
                        EntryType::Symlink => {
                            let target = source.link_name.clone().unwrap_or_default();
                            builder.append_link(&mut header, path, target)?;
                        }
                        EntryType::Regular | EntryType::Continuous => {
                            bytes += header.entry_size()?;
                            builder.append_data(&mut header, path, self.data(source)?)?;
                        }
                        _ => {
                            header.set_size(0);
                            builder.append_data(&mut header, path, io::empty())?;
                        }
                    }
                }
            }
        }
        for (path, source) in hardlinks {
            let target = source.link_name.as_deref().unwrap_or(Path::new(""));
            let target = normalise(target).map_err(|e| io::Error::other(e.0))?;
            let mut header = source.header.clone();
            // `mke2fs` gives the shared inode the link entry's metadata, so
            // it must match the target's.
            match tree.get(&target) {
                Some(Entry::Layer(t)) => {
                    header.set_mode(t.header.mode()?);
                    header.set_uid(t.header.uid()?);
                    header.set_gid(t.header.gid()?);
                    header.set_mtime(t.header.mtime()?);
                }
                Some(Entry::Guest(_, mode)) => {
                    header.set_mode(*mode);
                    header.set_uid(0);
                    header.set_gid(0);
                }
                Some(Entry::Dir) | None => {
                    return Err(io::Error::other(format!(
                        "{path} is a hard link to {target}, which isn't a file in the image"
                    )));
                }
            }
            builder.append_link(&mut header, path, &target)?;
        }
        builder.into_inner()?.flush()?;
        Ok(bytes)
    }
}

enum Entry<'a> {
    Layer(&'a Source),
    /// A file every guest gets, owned by root, with this mode.
    Guest(Vec<u8>, u32),
    /// A directory the image doesn't list: root-owned, `0755`.
    Dir,
}

/// `path` relative to the image root, without `.` components, or an error
/// if it would leave the root.
fn normalise(path: &Path) -> Result<String, Error> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| Error(format!("{} isn't UTF-8", path.display())))?,
            ),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(Error(format!("{} leaves the image root", path.display())));
            }
        }
    }
    Ok(parts.join("/"))
}

/// `path`'s parent and final component; the parent of a top-level entry is "".
fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Copies `layer` to `dest` as a plain tar, gunzipping it if needed.
fn decompress(layer: &Path, dest: &Path) -> Result<(), Error> {
    let context = || format!("layer {}", layer.display());
    let mut file = File::open(layer).map_err(|e| err(&context(), e))?;
    let mut magic = [0u8; 4];
    let n = file.read(&mut magic).map_err(|e| err(&context(), e))?;
    file.rewind().map_err(|e| err(&context(), e))?;
    let mut out = BufWriter::new(File::create(dest).map_err(|e| err("create scratch layer", e))?);
    let copied = match &magic[..n] {
        [0x1f, 0x8b, ..] => io::copy(&mut GzDecoder::new(BufReader::new(file)), &mut out),
        [0x28, 0xb5, 0x2f, 0xfd] => {
            return Err(Error(format!(
                "{}: zstd-compressed layers aren't supported yet",
                context()
            )));
        }
        _ => io::copy(&mut BufReader::new(file), &mut out),
    };
    copied
        .and_then(|_| out.flush())
        .map_err(|e| err(&context(), e))
}

/// Fails unless `mke2fs` is new enough to build from a tarball.
pub fn check_mke2fs() -> Result<(), Error> {
    let output = Command::new("mke2fs")
        .arg("-V")
        .output()
        .map_err(|e| err("run mke2fs (install e2fsprogs 1.47.1 or later)", e))?;
    let text = String::from_utf8_lossy(&output.stderr);
    let (major, minor, patch) = MIN_MKE2FS;
    match parse_mke2fs_version(&text) {
        Some(version) if version >= MIN_MKE2FS => Ok(()),
        found => Err(Error(format!(
            "building images needs e2fsprogs {major}.{minor}.{patch} or later (mke2fs -d with a \
             tarball); found {}",
            found.map_or("an unrecognised version".into(), |(a, b, c)| format!(
                "{a}.{b}.{c}"
            ))
        ))),
    }
}

/// The version in `mke2fs -V` output, such as `mke2fs 1.47.4 (6-Mar-2025)`.
fn parse_mke2fs_version(text: &str) -> Option<(u32, u32, u32)> {
    let version = text.lines().next()?.split_whitespace().nth(1)?;
    let mut parts = version.split('.').map(|p| p.parse::<u32>());
    let major = parts.next()?.ok()?;
    let minor = parts.next()?.ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests;
