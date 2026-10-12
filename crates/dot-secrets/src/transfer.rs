//! Moving the `envfile` provider's secrets to another machine: `dot secrets export --age` /
//! `import` (a passphrase-encrypted backup), and [`merge`], which writes secrets resolved on
//! another machine (`dot secrets push`, `dot apply --host --persist-secrets`) into this one's
//! envfile.
//!
//! What moves is the secrets file (`secrets.env`) and every file secret under the files
//! directory. [`Inventory`] lists them, after the same guardrails every read applies (owner-only
//! permissions, not inside an unignored git work tree).
//!
//! The backup is a tar archive (`secrets.env`, `files/<path>`, each `0600`) encrypted with age
//! to a passphrase (the scrypt recipient), so `age -d backup.age | tar -t` works without `dot`.
//! Import checks every entry's path, refuses to overwrite an existing file unless forced, and
//! writes each one owner-only.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use age::secrecy::SecretString;
use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

use crate::SecretValue;
use crate::dotenv;
use crate::envfile::EnvfileProvider;
use crate::fsutil::{check_private, write_private};
use crate::reference::{ENVFILE, SecretRef, check_rel_path};

/// The archive path of the secrets file.
pub const ENV_ENTRY: &str = "secrets.env";
/// The archive directory of file secrets.
pub const FILES_DIR: &str = "files";

/// The files an `envfile` provider holds. Paths only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    /// The secrets file, when it exists.
    pub env_file: Option<PathBuf>,
    /// File secrets: `(relative path with `/`, absolute path)`, sorted.
    pub files: Vec<(String, PathBuf)>,
}

impl Inventory {
    /// Lists `p`'s secrets file and file secrets, checking each against the guardrails.
    pub fn of(p: &EnvfileProvider) -> Result<Self> {
        let env_file = p.path().is_file().then(|| p.path().to_path_buf());
        if let Some(f) = &env_file {
            p.guard_file(f, "secrets file")?;
        }
        let mut files = Vec::new();
        if p.files_dir().is_dir() {
            walk(p.files_dir(), "", &mut files)?;
        }
        files.sort();
        for (_, path) in &files {
            p.guard_file(path, "file secret")?;
        }
        Ok(Self { env_file, files })
    }

    pub fn is_empty(&self) -> bool {
        self.env_file.is_none() && self.files.is_empty()
    }

    /// Every path, the secrets file first.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.env_file
            .iter()
            .map(PathBuf::as_path)
            .chain(self.files.iter().map(|(_, p)| p.as_path()))
    }
}

fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("cannot list {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot list {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            bail!(
                "{}: file secret names must be UTF-8",
                entry.path().display()
            );
        };
        let rel = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        // Follows symlinks, as reading a file secret does.
        let meta =
            fs::metadata(&path).with_context(|| format!("cannot stat {}", path.display()))?;
        if meta.is_dir() {
            walk(&path, &rel, out)?;
        } else if meta.is_file() {
            check_rel_path(&rel).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            out.push((rel, path));
        }
    }
    Ok(())
}

/// The secrets of an [`Inventory`], in memory: `(archive path, bytes)`. Zeroed when dropped;
/// `Debug` shows paths only.
#[derive(Default)]
pub struct Archive {
    entries: Vec<(String, Zeroizing<Vec<u8>>)>,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

impl Archive {
    /// Reads every file of `inv`.
    pub fn read(inv: &Inventory) -> Result<Self> {
        let mut entries = Vec::new();
        let read = |p: &Path| -> Result<Zeroizing<Vec<u8>>> {
            Ok(Zeroizing::new(
                fs::read(p).with_context(|| format!("cannot read {}", p.display()))?,
            ))
        };
        if let Some(env) = &inv.env_file {
            entries.push((ENV_ENTRY.to_string(), read(env)?));
        }
        for (rel, path) in &inv.files {
            entries.push((format!("{FILES_DIR}/{rel}"), read(path)?));
        }
        Ok(Self { entries })
    }

    /// Archive paths, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(n, _)| n.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The archive as tar bytes.
    fn to_tar(&self) -> Result<Zeroizing<Vec<u8>>> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, bytes) in &self.entries {
            let mut h = tar::Header::new_ustar();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o600);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_mtime(0);
            b.append_data(&mut h, name, bytes.as_slice())
                .with_context(|| format!("cannot archive {name}"))?;
        }
        Ok(Zeroizing::new(
            b.into_inner().context("cannot finish the archive")?,
        ))
    }

    /// Parses tar bytes, checking every entry: regular files named `secrets.env` or
    /// `files/<relative path>`, each at most once.
    fn from_tar(bytes: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        let mut a = tar::Archive::new(bytes);
        for entry in a.entries().context("not a dot secrets archive")? {
            let mut entry = entry.context("not a dot secrets archive")?;
            let path = entry
                .path()
                .context("an archive entry has an unreadable name")?
                .to_string_lossy()
                .replace('\\', "/");
            match entry.header().entry_type() {
                tar::EntryType::Regular => {}
                tar::EntryType::Directory => continue,
                t => bail!("archive entry `{path}` is not a regular file ({t:?})"),
            }
            if path != ENV_ENTRY {
                let rel = path
                    .strip_prefix(&format!("{FILES_DIR}/"))
                    .with_context(|| format!("unexpected archive entry `{path}`"))?;
                check_rel_path(rel).map_err(|e| anyhow::anyhow!("archive entry `{path}`: {e}"))?;
            }
            if !seen.insert(path.clone()) {
                bail!("archive entry `{path}` appears twice");
            }
            let mut buf = Zeroizing::new(Vec::new());
            entry
                .read_to_end(&mut buf)
                .with_context(|| format!("cannot read archive entry `{path}`"))?;
            entries.push((path, buf));
        }
        Ok(Self { entries })
    }

    /// The archive encrypted to `passphrase` (age, scrypt recipient). `work_factor` overrides
    /// age's calibrated scrypt cost (log2); tests lower it.
    pub fn encrypt(&self, passphrase: &SecretValue, work_factor: Option<u8>) -> Result<Vec<u8>> {
        let mut recipient = age::scrypt::Recipient::new(secret_string(passphrase)?);
        if let Some(n) = work_factor {
            recipient.set_work_factor(n);
        }
        let tar = self.to_tar()?;
        age::encrypt(&recipient, &tar).context("encryption failed")
    }

    /// Decrypts and parses an archive made by [`Archive::encrypt`].
    pub fn decrypt(ciphertext: &[u8], passphrase: &SecretValue) -> Result<Self> {
        let identity = age::scrypt::Identity::new(secret_string(passphrase)?);
        let tar = Zeroizing::new(age::decrypt(&identity, ciphertext).map_err(|e| match e {
            age::DecryptError::DecryptionFailed | age::DecryptError::NoMatchingKeys => {
                anyhow::anyhow!(
                    "cannot decrypt: wrong passphrase, or not a passphrase-encrypted age file"
                )
            }
            other => anyhow::anyhow!("cannot decrypt: {other}"),
        })?);
        Self::from_tar(&tar)
    }

    /// Where each entry goes for `p`: `(archive path, destination)`.
    pub fn destinations(&self, p: &EnvfileProvider) -> Vec<(String, PathBuf)> {
        self.entries
            .iter()
            .map(|(name, _)| {
                let dest = match name.strip_prefix(&format!("{FILES_DIR}/")) {
                    Some(rel) => p.files_dir().join(rel),
                    None => p.path().to_path_buf(),
                };
                (name.clone(), dest)
            })
            .collect()
    }

    /// Writes every entry to `p`'s files, owner-only. Nothing is written when any destination
    /// exists (unless `force`), fails the git guardrail, or the secrets file does not parse.
    /// Returns the paths written.
    pub fn restore(&self, p: &EnvfileProvider, force: bool) -> Result<Vec<PathBuf>> {
        let dests = self.destinations(p);
        let existing: Vec<String> = dests
            .iter()
            .filter(|(_, d)| d.exists())
            .map(|(_, d)| d.display().to_string())
            .collect();
        if !existing.is_empty() && !force {
            bail!(
                "refusing to overwrite existing secrets (use --force): {}",
                existing.join(", ")
            );
        }
        for ((name, bytes), (_, dest)) in self.entries.iter().zip(&dests) {
            p.check_git(dest, "secret")?;
            if name == ENV_ENTRY {
                dotenvy::from_read_iter(bytes.as_slice())
                    .try_for_each(|item| item.map(drop))
                    .map_err(|_| {
                        anyhow::anyhow!("the archived secrets.env is not valid dotenv (not shown)")
                    })?;
            }
        }
        let mut written = Vec::new();
        for ((_, bytes), (_, dest)) in self.entries.iter().zip(dests) {
            write_private(&dest, bytes)?;
            check_private(&dest, "secret").map_err(anyhow::Error::msg)?;
            written.push(dest);
        }
        Ok(written)
    }
}

/// What [`merge`] (or [`crate::merge_env`]) did with one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// It was not there (or empty): written.
    Added,
    /// It held another value, replaced (`force`).
    Updated,
    /// It already held this value.
    Unchanged,
    /// It holds another value, left alone (no `force`).
    Kept,
}

impl Merge {
    pub fn as_str(self) -> &'static str {
        match self {
            Merge::Added => "added",
            Merge::Updated => "updated",
            Merge::Unchanged => "unchanged",
            Merge::Kept => "kept",
        }
    }
}

/// Writes secrets resolved elsewhere into `p`'s files: `(name, target, value)`, where `target`
/// is an `envfile:KEY` (a line of the secrets file) or `envfile-file:path` (a file under the
/// files directory) ref. An existing different value is replaced only with `force`. Every file
/// is written atomically and owner-only (directories `0700`), after the guardrails every read
/// applies (an existing file that others can read, or a destination inside an unignored git work
/// tree, is refused). Returns what happened to each name, in order; never a value. `dry`: only
/// works out what would happen (the same checks, nothing written).
pub fn merge(
    p: &EnvfileProvider,
    items: &[(&str, &SecretRef, &SecretValue)],
    force: bool,
    dry: bool,
) -> Result<Vec<(String, Merge)>> {
    let mut out = Vec::with_capacity(items.len());
    for (name, target, _) in items {
        if target.provider() != ENVFILE {
            bail!("secret `{name}`: `{target}` is not an envfile ref");
        }
        if target.is_file() {
            check_rel_path(target.locator())
                .map_err(|e| anyhow::anyhow!("secret `{name}`: `{target}`: {e}"))?;
        }
    }
    // Values: one read and (at most) one write of the secrets file.
    let values: Vec<&(&str, &SecretRef, &SecretValue)> =
        items.iter().filter(|(_, t, _)| !t.is_file()).collect();
    let mut results: Vec<Option<Merge>> = vec![None; items.len()];
    if !values.is_empty() {
        let path = p.path();
        let current = if path.exists() {
            p.guard_file(path, "secrets file")?;
            Zeroizing::new(
                fs::read_to_string(path)
                    .with_context(|| format!("cannot read {}", path.display()))?,
            )
        } else {
            Zeroizing::new(String::new())
        };
        let mut have: std::collections::BTreeMap<String, Zeroizing<String>> = Default::default();
        for item in dotenvy::from_read_iter(current.as_bytes()) {
            let (k, v) = item.map_err(|_| {
                anyhow::anyhow!("{} is not valid dotenv (not shown)", path.display())
            })?;
            have.insert(k, Zeroizing::new(v));
        }
        let mut text = Zeroizing::new(current.to_string());
        let mut changed = false;
        for (i, (name, target, value)) in items.iter().enumerate() {
            if target.is_file() {
                continue;
            }
            let Some(v) = value.expose_str() else {
                bail!("secret `{name}`: `{target}` holds text, and this value is not UTF-8");
            };
            let key = target.locator();
            let result = match have.get(key) {
                Some(cur) if cur.as_str() == v => Merge::Unchanged,
                Some(cur) if !cur.is_empty() && !force => Merge::Kept,
                Some(cur) if !cur.is_empty() => Merge::Updated,
                _ => Merge::Added,
            };
            if matches!(result, Merge::Added | Merge::Updated) {
                text = Zeroizing::new(dotenv::upsert(&text, key, v));
                have.insert(key.to_string(), Zeroizing::new(v.to_string()));
                changed = true;
            }
            results[i] = Some(result);
        }
        if changed && !dry {
            p.check_git(path, "secrets file")?;
            write_private(path, text.as_bytes())?;
            check_private(path, "secrets file").map_err(anyhow::Error::msg)?;
        }
    }
    // File secrets: one file each.
    for (i, (_, target, value)) in items.iter().enumerate() {
        if !target.is_file() {
            continue;
        }
        let dest = p.files_dir().join(target.locator());
        let result = if dest.exists() {
            p.guard_file(&dest, "file secret")?;
            let cur = Zeroizing::new(
                fs::read(&dest).with_context(|| format!("cannot read {}", dest.display()))?,
            );
            if cur.as_slice() == value.expose() {
                Merge::Unchanged
            } else if force {
                Merge::Updated
            } else {
                Merge::Kept
            }
        } else {
            Merge::Added
        };
        if matches!(result, Merge::Added | Merge::Updated) && !dry {
            p.check_git(&dest, "file secret")?;
            write_private(&dest, value.expose())?;
            check_private(&dest, "file secret").map_err(anyhow::Error::msg)?;
        }
        results[i] = Some(result);
    }
    for ((name, _, _), r) in items.iter().zip(results) {
        out.push((name.to_string(), r.expect("every item is merged")));
    }
    Ok(out)
}

fn secret_string(passphrase: &SecretValue) -> Result<SecretString> {
    let Some(text) = passphrase.expose_str() else {
        bail!("the passphrase must be UTF-8 text");
    };
    if text.is_empty() {
        bail!("the passphrase is empty");
    }
    Ok(SecretString::from(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Git;

    fn provider(dir: &Path) -> EnvfileProvider {
        EnvfileProvider::new(dir.join("secrets.env"), None)
            .with_git(Git::system().with_program(dir.join("no-git")))
    }

    fn pass(s: &str) -> SecretValue {
        SecretValue::from(s.to_string())
    }

    fn seed(dir: &Path) -> EnvfileProvider {
        let p = provider(dir);
        write_private(p.path(), b"TOKEN=\"t0k-value\"\nOTHER=x\n").unwrap();
        write_private(&p.files_dir().join("vpn/work.ovpn"), b"\x00\x01ovpn\xff").unwrap();
        write_private(&p.files_dir().join("id.pem"), b"pem").unwrap();
        p
    }

    #[test]
    fn inventory_lists_the_env_file_and_every_file_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let p = seed(tmp.path());
        let inv = Inventory::of(&p).unwrap();
        assert_eq!(inv.env_file.as_deref(), Some(p.path()));
        let rels: Vec<&str> = inv.files.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(rels, ["id.pem", "vpn/work.ovpn"]);
        assert_eq!(inv.paths().count(), 3);

        let empty = Inventory::of(&provider(&tmp.path().join("none"))).unwrap();
        assert!(empty.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn inventory_applies_the_guardrails() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = seed(tmp.path());
        let loose = p.files_dir().join("id.pem");
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).unwrap();
        let e = format!("{:#}", Inventory::of(&p).unwrap_err());
        assert!(e.contains("chmod 600") && e.contains("id.pem"), "{e}");
    }

    #[test]
    fn export_import_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let p = seed(&tmp.path().join("a"));
        let archive = Archive::read(&Inventory::of(&p).unwrap()).unwrap();
        assert_eq!(
            format!("{archive:?}"),
            "[\"secrets.env\", \"files/id.pem\", \"files/vpn/work.ovpn\"]"
        );
        let sealed = archive.encrypt(&pass("correct horse"), Some(2)).unwrap();
        assert!(sealed.starts_with(b"age-encryption.org/v1\n"));
        assert!(!sealed.windows(9).any(|w| w == b"t0k-value"));

        let e = format!(
            "{:#}",
            Archive::decrypt(&sealed, &pass("wrong")).unwrap_err()
        );
        assert!(e.contains("wrong passphrase"), "{e}");
        assert!(archive.encrypt(&pass(""), Some(2)).is_err());

        let back = Archive::decrypt(&sealed, &pass("correct horse")).unwrap();
        let q = provider(&tmp.path().join("b"));
        let written = back.restore(&q, false).unwrap();
        assert_eq!(written.len(), 3);
        assert_eq!(fs::read(q.path()).unwrap(), fs::read(p.path()).unwrap());
        assert_eq!(
            fs::read(q.files_dir().join("vpn/work.ovpn")).unwrap(),
            b"\x00\x01ovpn\xff"
        );
        for w in &written {
            check_private(w, "restored").unwrap();
        }

        // Existing files are kept unless forced; nothing is half-written.
        fs::remove_file(q.files_dir().join("id.pem")).unwrap();
        let e = format!("{:#}", back.restore(&q, false).unwrap_err());
        assert!(e.contains("--force") && e.contains("secrets.env"), "{e}");
        assert!(!q.files_dir().join("id.pem").exists());
        back.restore(&q, true).unwrap();
        assert!(q.files_dir().join("id.pem").exists());
    }

    #[test]
    fn merge_adds_keeps_and_forces() {
        let tmp = tempfile::tempdir().unwrap();
        let p = provider(tmp.path());
        write_private(p.path(), b"# keep me\nTOKEN=\"old\"\nSAME=v\n").unwrap();
        write_private(&p.files_dir().join("id.pem"), b"old-pem").unwrap();
        let r = |s: &str| SecretRef::parse(s).unwrap();
        let (tok, same, new, pem, vpn) = (
            r("envfile:TOKEN"),
            r("envfile:SAME"),
            r("envfile:NEW"),
            r("envfile-file:id.pem"),
            r("envfile-file:vpn/work.ovpn"),
        );
        let (v_tok, v_same, v_new, v_pem, v_vpn) = (
            pass("new-tok"),
            pass("v"),
            pass("multi\nline 'q' \"x\""),
            SecretValue::new(b"new-pem".to_vec()),
            SecretValue::new(b"\x00\xffovpn".to_vec()),
        );
        let items = [
            ("tok", &tok, &v_tok),
            ("same", &same, &v_same),
            ("new", &new, &v_new),
            ("pem", &pem, &v_pem),
            ("vpn", &vpn, &v_vpn),
        ];
        // A dry merge says the same and writes nothing.
        let before = fs::read(p.path()).unwrap();
        let dry = merge(&p, &items, false, true).unwrap();
        assert_eq!(fs::read(p.path()).unwrap(), before);
        assert!(!p.files_dir().join("vpn/work.ovpn").exists());
        let got = merge(&p, &items, false, false).unwrap();
        assert_eq!(dry, got);
        let shown: Vec<(&str, &str)> = got.iter().map(|(n, m)| (n.as_str(), m.as_str())).collect();
        assert_eq!(
            shown,
            [
                ("tok", "kept"),
                ("same", "unchanged"),
                ("new", "added"),
                ("pem", "kept"),
                ("vpn", "added")
            ]
        );
        let text = fs::read_to_string(p.path()).unwrap();
        assert!(
            text.starts_with("# keep me\n") && text.contains("TOKEN=\"old\""),
            "{text}"
        );
        let loaded: Vec<(String, String)> = dotenvy::from_path_iter(p.path())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(loaded.contains(&("NEW".into(), "multi\nline 'q' \"x\"".into())));
        assert_eq!(
            fs::read(p.files_dir().join("vpn/work.ovpn")).unwrap(),
            b"\x00\xffovpn"
        );
        check_private(&p.files_dir().join("vpn/work.ovpn"), "x").unwrap();
        check_private(p.path(), "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(p.files_dir().join("vpn"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }

        let got = merge(&p, &items, true, false).unwrap();
        assert_eq!(got[0].1, Merge::Updated);
        assert_eq!(got[3].1, Merge::Updated);
        assert_eq!(got[2].1, Merge::Unchanged);
        assert_eq!(fs::read(p.files_dir().join("id.pem")).unwrap(), b"new-pem");

        // Only envfile refs, relative paths.
        let lp = r("lastpass://Dev/X#password");
        assert!(merge(&p, &[("x", &lp, &v_tok)], false, true).is_err());
    }

    fn tar_of(entries: &[(&str, &[u8], tar::EntryType)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, bytes, ty) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o600);
            h.set_entry_type(*ty);
            // Written raw so hostile names reach the parser unchanged.
            let raw = &mut h.as_old_mut().name;
            raw[..name.len()].copy_from_slice(name.as_bytes());
            h.set_cksum();
            b.append(&h, *bytes).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn hostile_archives_are_refused() {
        use tar::EntryType::{Regular, Symlink};
        for (entries, needle) in [
            (
                vec![("../evil", &b"x"[..], Regular)],
                "unexpected archive entry",
            ),
            (vec![("files/../../x", &b"x"[..], Regular)], ".."),
            (vec![("files//x", &b"x"[..], Regular)], "relative"),
            (vec![("files/a//x", &b"x"[..], Regular)], "empty"),
            (
                vec![("files/link", &b""[..], Symlink)],
                "not a regular file",
            ),
            (
                vec![
                    ("secrets.env", &b"A=1"[..], Regular),
                    ("secrets.env", &b"A=2"[..], Regular),
                ],
                "twice",
            ),
        ] {
            let e = format!("{:#}", Archive::from_tar(&tar_of(&entries)).unwrap_err());
            assert!(e.contains(needle), "{needle}: {e}");
        }
        let ok = Archive::from_tar(&tar_of(&[("files/a/b.pem", b"x", Regular)])).unwrap();
        assert_eq!(ok.names().collect::<Vec<_>>(), ["files/a/b.pem"]);
    }
}
