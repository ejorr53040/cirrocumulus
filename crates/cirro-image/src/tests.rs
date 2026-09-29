use super::*;
use flate2::Compression;
use flate2::write::GzEncoder;
use std::sync::atomic::{AtomicU32, Ordering};

fn tempdir() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("cirro-image-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One layer's entries, written as a tar by [`Layer::write`].
#[derive(Default)]
struct Layer {
    entries: Vec<(Header, String, Vec<u8>)>,
}

impl Layer {
    fn entry(mut self, kind: EntryType, path: &str, mode: u32, uid: u64, data: &[u8]) -> Self {
        let mut header = Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_uid(uid);
        header.set_gid(uid);
        header.set_size(data.len() as u64);
        self.entries.push((header, path.to_string(), data.to_vec()));
        self
    }

    fn file(self, path: &str, data: &str) -> Self {
        self.entry(EntryType::Regular, path, 0o644, 0, data.as_bytes())
    }

    fn dir(self, path: &str) -> Self {
        self.entry(EntryType::Directory, path, 0o755, 0, b"")
    }

    fn link(mut self, kind: EntryType, path: &str, target: &str) -> Self {
        let mut header = Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_link_name(target).unwrap();
        self.entries.push((header, path.to_string(), Vec::new()));
        self
    }

    /// Writes the layer into `dir`, gzipped or not, and returns its path.
    fn write(&self, dir: &Path, name: &str, gzip: bool) -> PathBuf {
        let path = dir.join(name);
        let file = File::create(&path).unwrap();
        let out: Box<dyn Write> = if gzip {
            Box::new(GzEncoder::new(file, Compression::fast()))
        } else {
            Box::new(file)
        };
        let mut builder = tar::Builder::new(out);
        for (header, entry_path, data) in &self.entries {
            let mut header = header.clone();
            // Written raw, so a test can put `..` or `/` in a path.
            header.as_gnu_mut().unwrap().name[..entry_path.len()]
                .copy_from_slice(entry_path.as_bytes());
            header.set_cksum();
            builder.append(&header, data.as_slice()).unwrap();
        }
        builder.into_inner().unwrap().flush().unwrap();
        path
    }
}

fn flatten(layers: &[Layer]) -> Result<Flattened, Error> {
    let dir = tempdir();
    let paths: Vec<PathBuf> = layers
        .iter()
        .enumerate()
        .map(|(i, l)| l.write(&dir, &format!("in-{i}.tar"), i % 2 == 1))
        .collect();
    Flattened::open(&paths, &dir.join("scratch"))
}

fn paths(flat: &Flattened) -> Vec<&str> {
    flat.tree.keys().map(String::as_str).collect()
}

#[test]
fn a_later_layer_replaces_a_file() {
    let flat = flatten(&[
        Layer::default().dir("etc").file("etc/motd", "old"),
        Layer::default().file("etc/motd", "new"),
    ])
    .unwrap();

    assert_eq!(flat.read("/etc/motd").unwrap(), b"new");
}

#[test]
fn leading_dot_slash_and_slash_are_the_same_path() {
    let flat = flatten(&[
        Layer::default().file("./a", "1"),
        Layer::default().file("/a", "2"),
    ])
    .unwrap();

    assert_eq!(paths(&flat), ["a"]);
    assert_eq!(flat.read("a").unwrap(), b"2");
}

#[test]
fn a_whiteout_removes_a_file_or_a_whole_directory() {
    let flat = flatten(&[
        Layer::default()
            .file("keep", "k")
            .file("gone", "g")
            .dir("dir")
            .file("dir/inner", "i"),
        Layer::default().file(".wh.gone", "").file(".wh.dir", ""),
    ])
    .unwrap();

    assert_eq!(paths(&flat), ["keep"]);
}

#[test]
fn a_whiteout_hides_only_lower_layers() {
    let flat = flatten(&[Layer::default().file("x", "same layer").file(".wh.x", "")]).unwrap();

    assert_eq!(flat.read("x").unwrap(), b"same layer");
}

#[test]
fn an_opaque_dir_hides_lower_contents_but_keeps_its_own() {
    let flat = flatten(&[
        Layer::default()
            .dir("d")
            .file("d/old", "o")
            .file("other", "x"),
        Layer::default()
            .dir("d")
            .file("d/.wh..wh..opq", "")
            .file("d/new", "n"),
    ])
    .unwrap();

    assert_eq!(paths(&flat), ["d", "d/new", "other"]);
}

#[test]
fn a_file_over_a_directory_removes_what_was_in_it() {
    let flat = flatten(&[
        Layer::default().dir("d").file("d/a", "a"),
        Layer::default().file("d", "now a file"),
    ])
    .unwrap();

    assert_eq!(paths(&flat), ["d"]);
    assert_eq!(flat.read("d").unwrap(), b"now a file");
}

#[test]
fn a_directory_over_a_directory_merges() {
    let flat = flatten(&[
        Layer::default().dir("d").file("d/a", "a"),
        Layer::default().dir("d").file("d/b", "b"),
    ])
    .unwrap();

    assert_eq!(paths(&flat), ["d", "d/a", "d/b"]);
}

#[test]
fn a_path_that_leaves_the_root_is_refused() {
    let e = flatten(&[Layer::default().file("../escape", "x")])
        .expect_err("a .. path should be refused");

    assert!(e.0.contains("leaves the image root"), "{e}");
}

#[test]
fn a_hardlink_target_that_leaves_the_root_is_refused() {
    let e = flatten(&[Layer::default().link(EntryType::Link, "l", "../../etc/shadow")])
        .expect_err("a .. hardlink target should be refused");

    assert!(e.0.contains("leaves the image root"), "{e}");
}

#[test]
fn zstd_layers_are_refused_by_name() {
    let dir = tempdir();
    let layer = dir.join("zstd.tar.zst");
    std::fs::write(&layer, [0x28, 0xb5, 0x2f, 0xfd, 0, 0]).unwrap();

    let e = Flattened::open(&[layer], &dir.join("scratch")).expect_err("zstd should be refused");

    assert!(e.0.contains("zstd"), "{e}");
}

#[test]
fn parses_mke2fs_versions() {
    assert_eq!(
        parse_mke2fs_version("mke2fs 1.47.4 (6-Mar-2025)\n\tUsing EXT2FS Library"),
        Some((1, 47, 4))
    );
    assert_eq!(
        parse_mke2fs_version("mke2fs 1.47 (1-Jan-2023)"),
        Some((1, 47, 0))
    );
    assert_eq!(parse_mke2fs_version("garbage"), None);
}

// Building ext4 needs e2fsprogs 1.47.1 or later; these skip without it.

fn debugfs(image: &Path, request: &str) -> String {
    let output = Command::new("debugfs")
        .args(["-R", request])
        .arg(image)
        .output()
        .expect("run debugfs");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn build(layers: &[Layer]) -> Option<PathBuf> {
    if let Err(e) = check_mke2fs() {
        eprintln!("skipping: {e}");
        return None;
    }
    let flat = flatten(layers).unwrap();
    let dir = tempdir();
    let image = dir.join("rootfs.ext4");
    flat.build_ext4(b"#!guest-init", &dir, &image).unwrap();
    Some(image)
}

#[test]
fn the_image_keeps_owners_modes_and_links() {
    let Some(image) = build(&[Layer::default()
        .dir("usr")
        .dir("usr/bin")
        .entry(EntryType::Regular, "usr/bin/su", 0o4755, 0, b"su")
        .entry(EntryType::Regular, "usr/bin/app", 0o755, 101, b"app")
        .link(EntryType::Symlink, "bin", "usr/bin")
        .link(EntryType::Link, "usr/bin/app2", "usr/bin/app")])
    else {
        return;
    };

    let su = debugfs(&image, "stat /usr/bin/su");
    assert!(su.contains("Mode:  04755"), "{su}");
    let app = debugfs(&image, "stat /usr/bin/app");
    assert!(
        app.contains("User:   101") && app.contains("Group:   101"),
        "{app}"
    );
    assert!(
        app.contains("Links: 2"),
        "the hardlink should share the inode: {app}"
    );
    let bin = debugfs(&image, "stat /bin");
    assert!(
        bin.contains("Type: symlink") && bin.contains("usr/bin"),
        "{bin}"
    );
}

#[test]
fn every_guest_gets_init_mountpoints_and_dns() {
    let Some(image) = build(&[Layer::default()
        .file("hello", "hi")
        .file("etc/resolv.conf", "nameserver 127.0.0.53\n")])
    else {
        return;
    };

    assert_eq!(debugfs(&image, "cat /init"), "#!guest-init");
    assert!(debugfs(&image, "stat /init").contains("Mode:  0755"));
    assert!(debugfs(&image, "cat /etc/resolv.conf").contains("nameserver 1.1.1.1"));
    assert!(debugfs(&image, "cat /etc/hosts").contains("localhost"));
    let root = debugfs(&image, "ls -l /");
    for dir in ["proc", "sys", "dev", "etc", "hello"] {
        assert!(root.contains(dir), "no /{dir} in:\n{root}");
    }
}

#[test]
fn a_hardlink_to_a_whited_out_file_fails_the_build() {
    if check_mke2fs().is_err() {
        return;
    }
    let flat = flatten(&[
        Layer::default().file("target", "t"),
        Layer::default()
            .link(EntryType::Link, "link", "target")
            .file(".wh.target", ""),
    ])
    .unwrap();
    let dir = tempdir();

    let e = flat
        .build_ext4(b"", &dir, &dir.join("rootfs.ext4"))
        .expect_err("a dangling hardlink should fail the build");

    assert!(e.0.contains("isn't a file in the image"), "{e}");
}
