//! Tools that lamasery downloads itself: a pinned release file, checked
//! against its sha256 and unpacked into `dist/` in the tool's own folder.
//! The folder's other contents (the tool's config and state) are left alone.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// A pinned download: changing either value makes lamasery fetch it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub url: String,
    pub sha256: String,
}

/// Records which download `dist/` holds.
const MARKER: &str = ".lamasery-download";

pub fn dist_dir(tool_dir: &Path) -> PathBuf {
    tool_dir.join("dist")
}

fn marker(d: &Download) -> String {
    format!("{} {}\n", d.sha256, d.url)
}

/// Whether the tool's folder already holds this download.
pub fn is_current(tool_dir: &Path, d: &Download) -> bool {
    dist_dir(tool_dir).is_dir()
        && fs::read_to_string(tool_dir.join(MARKER)).is_ok_and(|s| s == marker(d))
}

/// Download, check and unpack `d` into `<tool_dir>/dist`, replacing what was
/// there. `progress` gets the bytes received so far and the total, if known.
pub fn install(
    tool_dir: &Path,
    d: &Download,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<()> {
    fs::create_dir_all(tool_dir)
        .with_context(|| format!("cannot create {}", tool_dir.display()))?;
    let part = tool_dir.join(".download.part");
    let result = fetch(&d.url, &part, progress).and_then(|sha| {
        if !sha.eq_ignore_ascii_case(d.sha256.trim()) {
            bail!(
                "checksum mismatch for {}:\n  expected sha256 {}\n  got      sha256 {sha}",
                d.url,
                d.sha256.trim()
            );
        }
        let fresh = tool_dir.join("dist.new");
        if fresh.exists() {
            fs::remove_dir_all(&fresh)?;
        }
        unpack(&part, &file_name(&d.url)?, &fresh)?;
        let dist = dist_dir(tool_dir);
        if dist.exists() {
            fs::remove_dir_all(&dist)
                .with_context(|| format!("cannot remove the old {}", dist.display()))?;
        }
        fs::rename(&fresh, &dist)?;
        fs::write(tool_dir.join(MARKER), marker(d))?;
        Ok(())
    });
    let _ = fs::remove_file(&part);
    result
}

/// Stream `url` into `dest`, returning its sha256 in hex.
fn fetch(url: &str, dest: &Path, progress: &mut dyn FnMut(u64, Option<u64>)) -> Result<String> {
    if !url.starts_with("https://") {
        bail!("download url must use https: {url}");
    }
    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("cannot download {url}"))?;
    let total = resp.body().content_length();
    let mut body = resp.into_body().into_reader();
    let mut out =
        fs::File::create(dest).with_context(|| format!("cannot write {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).with_context(|| format!("download of {url} failed")),
        };
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        done += n as u64;
        progress(done, total);
    }
    out.sync_all()?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The last path segment of `url`, without a query string.
fn file_name(url: &str) -> Result<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('/').next() {
        Some(name) if !name.is_empty() => Ok(name.to_string()),
        _ => bail!("cannot tell the file name from {url}"),
    }
}

/// `.tar.gz` / `.tgz` archives are unpacked into `dest`; anything else is
/// taken to be a single executable and copied there under its own name.
fn unpack(file: &Path, name: &str, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        let gz = flate2::read::GzDecoder::new(fs::File::open(file)?);
        let mut archive = tar::Archive::new(gz);
        archive.set_preserve_permissions(true);
        // `unpack` skips entries that would land outside `dest` (`..`, absolute paths).
        archive
            .unpack(dest)
            .with_context(|| format!("cannot unpack {name}"))?;
    } else if [".zip", ".tar", ".tar.xz", ".tar.bz2", ".tar.zst", ".7z"]
        .iter()
        .any(|ext| name.ends_with(ext))
    {
        bail!("{name}: only .tar.gz archives (or a single executable) are supported");
    } else {
        let target = dest.join(name);
        fs::copy(file, &target)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Write `content` to `path` unless it already holds exactly that.
pub fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if fs::read_to_string(path).is_ok_and(|old| old == content) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    fs::write(path, content).with_context(|| format!("cannot write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names() {
        assert_eq!(
            file_name("https://x/releases/download/v1/pi-linux-x64.tar.gz").unwrap(),
            "pi-linux-x64.tar.gz"
        );
        assert_eq!(file_name("https://x/tool?raw=1").unwrap(), "tool");
        assert!(file_name("https://x/").is_err());
    }

    #[test]
    fn unpacks_tar_gz_and_tracks_marker() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp/install");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        // A small .tar.gz with an executable in a subfolder.
        let archive = base.join("t.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                fs::File::create(&archive).unwrap(),
                flate2::Compression::fast(),
            );
            let mut tar = tar::Builder::new(gz);
            let data = b"#!/bin/sh\necho hi\n";
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, "pi/pi", &data[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let dest = base.join("dist");
        unpack(&archive, "t.tar.gz", &dest).unwrap();
        let bin = dest.join("pi/pi");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&bin).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let d = Download {
            url: "https://x/t.tar.gz".into(),
            sha256: "ab".into(),
        };
        assert!(!is_current(&base, &d));
        fs::write(base.join(MARKER), marker(&d)).unwrap();
        assert!(is_current(&base, &d));
        let newer = Download {
            sha256: "cd".into(),
            ..d.clone()
        };
        assert!(!is_current(&base, &newer));

        assert!(unpack(&archive, "t.zip", &base.join("z")).is_err());
    }

    /// Downloads pi from GitHub: `cargo test -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn installs_pi_release() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp/pi-release");
        let _ = fs::remove_dir_all(&base);
        let d = Download {
            url:
                "https://github.com/earendil-works/pi/releases/download/v1.6.0/pi-linux-x64.tar.gz"
                    .into(),
            sha256: "ef77dbe4379b3bdeb425a1e1b862fa3c915c255000dfe4965c98eee96a5765d1".into(),
        };
        let bad = Download {
            sha256: "0".repeat(64),
            ..d.clone()
        };
        let err = install(&base, &bad, &mut |_, _| {}).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert!(!is_current(&base, &bad) && !dist_dir(&base).exists());

        install(&base, &d, &mut |_, _| {}).unwrap();
        assert!(is_current(&base, &d));
        let out = std::process::Command::new(dist_dir(&base).join("pi/pi"))
            .arg("--version")
            .env("PI_OFFLINE", "1")
            .env("PI_CODING_AGENT_DIR", base.join("agent"))
            .output()
            .unwrap();
        let version = String::from_utf8_lossy(&out.stdout);
        assert!(version.contains("1.6.0"), "{version}");
    }

    #[test]
    fn writes_only_changes() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp/write");
        let _ = fs::remove_dir_all(&base);
        let p = base.join("a/b.json");
        write_if_changed(&p, "{}").unwrap();
        let t1 = fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_if_changed(&p, "{}").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().modified().unwrap(), t1);
        write_if_changed(&p, "{\"a\": 1}").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"a\": 1}");
    }
}
