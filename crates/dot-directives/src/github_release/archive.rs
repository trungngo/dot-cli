//! Taking executables out of a downloaded asset. Only the requested files are read, by file
//! name, wherever they are in the archive (the shallowest wins); nothing else is written, so an
//! archive's paths (`../`, absolute, links) never matter. When an archive has no file of the
//! wanted name, one named after it plus a platform (`yq_linux_amd64` for `yq`, or
//! `Godot_v4.3-stable_linux.x86_64` for `godot`: the name's case does not matter there) is
//! taken. A `bin:` mapping names the file by a glob instead (`godot: 'Godot_v*'`).

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use dot_core::config::Extract;

/// One executable to take out of an archive.
#[derive(Debug, Clone)]
pub struct Want {
    /// The file name it is installed as (and written to in the output directory).
    pub installed: String,
    /// File names accepted in the archive, e.g. `fzf` and `fzf.exe`.
    pub names: Vec<String>,
    /// A glob over file names that picks the file instead (`bin: { godot: 'Godot_v*' }`).
    pub glob: Option<globset::GlobMatcher>,
}

/// Writes each wanted executable found in `archive` (a `format` file named `asset`) to
/// `out_dir/<installed>`, and returns those paths in `wanted` order.
pub fn extract(
    archive: &Path,
    asset: &str,
    format: Extract,
    wanted: &[Want],
    out_dir: &Path,
) -> Result<Vec<PathBuf>> {
    let mut found = Found::new(wanted, out_dir);
    match format {
        Extract::Raw | Extract::Auto => {
            let [w] = wanted else {
                bail!("{asset} is a single file, so it can only be installed as one executable");
            };
            let dest = out_dir.join(&w.installed);
            std::fs::rename(archive, &dest)
                .or_else(|_| std::fs::copy(archive, &dest).map(|_| ()))
                .with_context(|| format!("moving {} into place", archive.display()))?;
            return Ok(vec![dest]);
        }
        Extract::TarGz => {
            let f = open(archive)?;
            found.tar(flate2::read::GzDecoder::new(BufReader::new(f)))?;
        }
        Extract::TarXz => {
            let tar = out_dir.join(".dot-asset.tar");
            {
                let mut input = BufReader::new(open(archive)?);
                let mut output = BufWriter::new(
                    File::create(&tar).with_context(|| format!("creating {}", tar.display()))?,
                );
                lzma_rs::xz_decompress(&mut input, &mut output)
                    .map_err(|e| anyhow::anyhow!("decompressing {asset}: {e}"))?;
                io::Write::flush(&mut output)?;
            }
            found.tar(BufReader::new(open(&tar)?))?;
            let _ = std::fs::remove_file(&tar);
        }
        Extract::Zip => found.zip(open(archive)?)?,
    }
    found.finish(asset)
}

/// Whether `entry` is `wanted` named for a platform: `yq_linux_amd64` or `tool-darwin-arm64`
/// (with `.exe` when `wanted` has it) for `yq` / `tool`.
fn platform_named(entry: &str, wanted: &str) -> bool {
    let (entry, wanted) = match (entry.strip_suffix(".exe"), wanted.strip_suffix(".exe")) {
        (Some(e), Some(w)) => (e, w),
        (None, None) => (entry, wanted),
        _ => return false,
    };
    let (entry, wanted) = (entry.to_ascii_lowercase(), wanted.to_ascii_lowercase());
    entry
        .strip_prefix(wanted.as_str())
        .and_then(|rest| rest.strip_prefix(['_', '-', '.']))
        .is_some_and(|rest| !rest.is_empty() && super::asset::names_platform(rest))
}

fn open(p: &Path) -> Result<File> {
    File::open(p).with_context(|| format!("opening {}", p.display()))
}

struct Found<'a> {
    wanted: &'a [Want],
    out_dir: &'a Path,
    /// Depth of the entry written for each wanted file.
    depth: Vec<Option<usize>>,
    /// Whether that entry only has the platform-suffixed name (see [`platform_named`]); an
    /// entry with the exact name replaces it.
    by_platform: Vec<bool>,
    /// Regular files seen, for the error message.
    seen: Vec<String>,
}

impl<'a> Found<'a> {
    fn new(wanted: &'a [Want], out_dir: &'a Path) -> Self {
        Self {
            wanted,
            out_dir,
            depth: vec![None; wanted.len()],
            by_platform: vec![false; wanted.len()],
            seen: Vec::new(),
        }
    }

    /// Considers one regular file of the archive; `read` supplies its content.
    fn entry(&mut self, path: &Path, read: &mut dyn Read) -> Result<()> {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return Ok(());
        };
        let depth = path.components().count();
        if self.seen.len() < 200 {
            self.seen.push(path.to_string_lossy().into_owned());
        }
        for (i, w) in self.wanted.iter().enumerate() {
            let exact = match &w.glob {
                Some(g) => g.is_match(&name),
                None => w.names.contains(&name),
            };
            let take = if exact {
                self.by_platform[i] || !self.depth[i].is_some_and(|d| d <= depth)
            } else {
                w.glob.is_none()
                    && self.depth[i].is_none()
                    && w.names.iter().any(|n| platform_named(&name, n))
            };
            if !take {
                continue;
            }
            let dest = self.out_dir.join(&w.installed);
            let mut out =
                File::create(&dest).with_context(|| format!("creating {}", dest.display()))?;
            io::copy(read, &mut out).with_context(|| format!("extracting {}", path.display()))?;
            self.depth[i] = Some(depth);
            self.by_platform[i] = !exact;
            // One entry supplies one wanted file; its content is consumed.
            break;
        }
        Ok(())
    }

    fn tar(&mut self, r: impl Read) -> Result<()> {
        let mut archive = tar::Archive::new(r);
        for entry in archive.entries().context("reading the archive")? {
            let mut entry = entry.context("reading the archive")?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path = entry.path().context("reading the archive")?.into_owned();
            self.entry(&path, &mut entry)?;
        }
        Ok(())
    }

    fn zip(&mut self, f: File) -> Result<()> {
        let mut archive = zip::ZipArchive::new(BufReader::new(f)).context("reading the zip")?;
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).context("reading the zip")?;
            if !file.is_file() || file.is_symlink() {
                continue;
            }
            let Some(path) = file.enclosed_name() else {
                continue;
            };
            self.entry(&path, &mut file)?;
        }
        Ok(())
    }

    fn finish(self, asset: &str) -> Result<Vec<PathBuf>> {
        let missing: Vec<&str> = self
            .wanted
            .iter()
            .zip(&self.depth)
            .filter(|(_, d)| d.is_none())
            .map(|(w, _)| w.names[0].as_str())
            .collect();
        if !missing.is_empty() {
            let mut listing = self.seen.clone();
            listing.truncate(20);
            bail!(
                "{} not found in {asset} (set `bin:` to the executable's file name, or map the name \
                 to a glob: `bin: {{ tool: 'tool_*' }}`); it contains: {}{}",
                missing
                    .iter()
                    .map(|m| format!("`{m}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if listing.is_empty() {
                    "no files".to_string()
                } else {
                    listing.join(", ")
                },
                if self.seen.len() > 20 { ", ..." } else { "" }
            );
        }
        Ok(self
            .wanted
            .iter()
            .map(|w| self.out_dir.join(&w.installed))
            .collect())
    }
}

#[cfg(test)]
pub mod tests {
    use std::io::Write;

    use super::*;

    /// Builders for archive fixtures, shared with the directive's tests.
    pub fn tar_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(&mut h, path, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    pub fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&tar_bytes(files)).unwrap();
        e.finish().unwrap()
    }

    pub fn tar_xz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let tar = tar_bytes(files);
        let mut out = Vec::new();
        lzma_rs::xz_compress(&mut &tar[..], &mut out).unwrap();
        out
    }

    pub fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (path, data) in files {
            w.start_file(*path, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn want(name: &str) -> Want {
        Want {
            installed: name.to_string(),
            names: vec![name.to_string(), format!("{name}.exe")],
            glob: None,
        }
    }

    fn run(bytes: &[u8], format: Extract, wanted: &[Want]) -> Result<Vec<(String, String)>> {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("asset");
        std::fs::write(&archive, bytes).unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let paths = extract(&archive, "asset.x", format, wanted, &out)?;
        Ok(paths
            .iter()
            .map(|p| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    std::fs::read_to_string(p).unwrap(),
                )
            })
            .collect())
    }

    const FILES: [(&str, &[u8]); 4] = [
        ("tool-1.0/README.md", b"readme"),
        ("tool-1.0/deep/nested/tool", b"deep"),
        ("tool-1.0/tool", b"shallow"),
        ("tool-1.0/helper.exe", b"helper"),
    ];

    #[test]
    fn every_format_finds_the_shallowest_match() {
        let wanted = [want("tool"), want("helper")];
        let expect = vec![
            ("tool".to_string(), "shallow".to_string()),
            ("helper".to_string(), "helper".to_string()),
        ];
        assert_eq!(
            run(&tar_gz(&FILES), Extract::TarGz, &wanted).unwrap(),
            expect
        );
        assert_eq!(
            run(&tar_xz(&FILES), Extract::TarXz, &wanted).unwrap(),
            expect
        );
        assert_eq!(
            run(&zip_bytes(&FILES), Extract::Zip, &wanted).unwrap(),
            expect
        );
    }

    #[test]
    fn raw_asset_is_the_executable() {
        assert_eq!(
            run(b"binary", Extract::Raw, &[want("tool")]).unwrap(),
            [("tool".to_string(), "binary".to_string())]
        );
        let e = run(b"x", Extract::Raw, &[want("a"), want("b")]).unwrap_err();
        assert!(e.to_string().contains("only be installed as one"), "{e}");
    }

    #[test]
    fn a_platform_named_executable_stands_in_for_the_wanted_one() {
        // mikefarah/yq's tarballs hold `./yq_linux_amd64`, a man page and a script.
        let yq: [(&str, &[u8]); 3] = [
            ("./yq_linux_amd64", b"yq-binary"),
            ("yq.1", b"man"),
            ("install-man-page.sh", b"sh"),
        ];
        assert_eq!(
            run(&tar_gz(&yq), Extract::TarGz, &[want("yq")]).unwrap(),
            [("yq".to_string(), "yq-binary".to_string())]
        );
        // The exact name wins wherever it is; names that do not add a platform never count.
        let both: [(&str, &[u8]); 3] = [
            ("tool-darwin-arm64", b"suffixed"),
            ("deep/er/tool", b"exact"),
            ("tool-completion.bash", b"no"),
        ];
        assert_eq!(
            run(&zip_bytes(&both), Extract::Zip, &[want("tool")]).unwrap(),
            [("tool".to_string(), "exact".to_string())]
        );
        let neither: [(&str, &[u8]); 2] = [("tool-completion.bash", b"no"), ("tool-1.0", b"no")];
        assert!(run(&tar_gz(&neither), Extract::TarGz, &[want("tool")]).is_err());
        assert!(platform_named("yq_windows_amd64.exe", "yq.exe"));
        assert!(!platform_named("yq_windows_amd64.exe", "yq"));
    }

    #[test]
    fn an_executable_can_be_renamed_and_picked_by_a_glob() {
        // Godot's zip: one versioned, capitalised executable.
        let godot: [(&str, &[u8]); 2] = [
            ("Godot_v4.7.2-stable_linux.x86_64", b"godot-binary"),
            ("README.txt", b"readme"),
        ];
        // By glob, installed under the name given.
        let mut w = want("godot");
        w.glob = Some(globset::Glob::new("Godot_v*").unwrap().compile_matcher());
        assert_eq!(
            run(&zip_bytes(&godot), Extract::Zip, &[w]).unwrap(),
            [("godot".to_string(), "godot-binary".to_string())]
        );
        // A plain name matches a platform-named file whatever its case.
        assert_eq!(
            run(&zip_bytes(&godot), Extract::Zip, &[want("godot")]).unwrap(),
            [("godot".to_string(), "godot-binary".to_string())]
        );
        // A glob that matches nothing says so.
        let mut w = want("godot");
        w.glob = Some(globset::Glob::new("godot-*").unwrap().compile_matcher());
        let e = run(&zip_bytes(&godot), Extract::Zip, &[w])
            .unwrap_err()
            .to_string();
        assert!(e.contains("`godot` not found"), "{e}");
    }

    #[test]
    fn missing_executable_lists_the_contents() {
        let e = run(&tar_gz(&FILES), Extract::TarGz, &[want("nope")]).unwrap_err();
        let e = e.to_string();
        assert!(e.contains("`nope` not found in asset.x"), "{e}");
        assert!(e.contains("tool-1.0/README.md"), "{e}");
    }

    #[test]
    fn traversal_paths_are_never_written() {
        // The entry is read by file name only; its path does not decide where it goes.
        let mut raw = tar_bytes(&[("x/tool", b"ok")]);
        // Rewrite the stored name to an escaping path of the same length.
        let pos = raw.windows(6).position(|w| w == b"x/tool").unwrap();
        raw[pos..pos + 6].copy_from_slice(b"../too");
        let mut h = tar::Header::new_old();
        h.as_mut_bytes().copy_from_slice(&raw[..512]);
        h.set_cksum();
        raw[..512].copy_from_slice(h.as_bytes());
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        let bytes = gz.finish().unwrap();
        let got = run(&bytes, Extract::TarGz, &[want("too")]).unwrap();
        assert_eq!(got, [("too".to_string(), "ok".to_string())]);
    }

    #[test]
    fn corrupt_archives_fail() {
        assert!(run(b"not gzip", Extract::TarGz, &[want("a")]).is_err());
        assert!(run(b"not xz", Extract::TarXz, &[want("a")]).is_err());
        assert!(run(b"not zip", Extract::Zip, &[want("a")]).is_err());
    }
}
