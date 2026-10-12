//! What one machine hands the `dot` it runs on another (`dot apply --host`, `dot plan --host`,
//! `dot secrets push`): secrets resolved on the sending machine and the variables forwarded to
//! the run. It travels as one stream on the ssh session's stdin (or, when that stdin is the
//! terminal, through a FIFO fed by a second ssh session, see [`relay`]); never on a command
//! line, in the environment, in a URL or in a file.
//!
//! The stream (version 1) is a sequence of records. Each is one line of JSON metadata (names and
//! flags, never a value), then, for a record with a `len`, exactly that many raw bytes and a
//! newline:
//!
//! ```text
//! {"t":"dot-handoff","v":1,"via":"apply","host":"me@box","persist":false,"force":false,"envfile_path":null,"files_dir":null}
//! {"t":"secret","name":"api_token","file":false,"target":"envfile:API_TOKEN","len":12}
//! <12 bytes>
//! {"t":"var","name":"EMAIL","persist":true,"keep":false,"sensitive":false,"len":17}
//! <17 bytes>
//! {"t":"end"}
//! ```
//!
//! The receiving `dot` keeps the secrets in memory for its run (they are used before any
//! provider is asked, and registered with the redactor, as are variables flagged `sensitive`) and
//! sets the variables as `--var` would. Only with `persist` are they written: secrets into the
//! envfile (`target` names the key or file), `persist: local` variables into `local.env`,
//! owner-only, a different value already there replaced only with `force`. A variable marked
//! `keep` (a per-machine answer asked for on the sending machine) is kept in `local.env` even
//! without `persist`: it is this machine's own value. Nothing is written before the run is
//! confirmed ([`Received`] is its last prepare stage), and a secret this machine keeps instead
//! of the one sent (no `force`) is the one the run uses. (Variables are sent over a value the
//! receiver holds only when given with `--var`, which wins for the run, as it does locally.)

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use dot_core::workspace::Inputs;
use dot_core::{EventSink, Plan};
use dot_secrets::{EnvfileConfig, EnvfileProvider, Merge, SecretRef, SecretValue};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The stream's version; a receiver refuses any other.
pub const VERSION: u32 = 1;

/// How long either end of the FIFO waits for the other.
#[cfg(unix)]
const FIFO_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Upper bounds a receiver enforces: one value, and the whole stream.
const MAX_VALUE: usize = 16 << 20;
const MAX_TOTAL: usize = 64 << 20;

/// Which command sent the stream: it words what the receiver reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Via {
    /// `dot apply --host` / `dot plan --host`.
    #[default]
    Apply,
    /// `dot secrets push`.
    Push,
}

impl Via {
    /// The sender's flag that replaces a different value already kept.
    fn force_flag(self) -> &'static str {
        match self {
            Via::Apply => "--force-secrets",
            Via::Push => "--force",
        }
    }
}

/// One secret, resolved by the sender.
pub struct SentSecret {
    pub name: String,
    /// Used as a file (`kind: file`): any bytes.
    pub file: bool,
    /// Where `persist` keeps it: an `envfile:KEY` or `envfile-file:path` ref (`None`: its
    /// `secrets.yml` entry has no envfile ref, so it cannot be kept).
    pub target: Option<String>,
    pub value: SecretValue,
}

/// One variable for the run, as `--var NAME=value`.
pub struct SentVar {
    pub name: String,
    pub value: Zeroizing<String>,
    /// Declared `persist: local`: kept in `local.env` with `persist`.
    pub persist: bool,
    /// Kept in `local.env` even without `persist`: the receiving machine's own value, asked for
    /// on the sending one (a `per_machine: true`, `persist: local` variable).
    pub keep: bool,
    pub sensitive: bool,
}

/// Everything sent. `Debug` shows names only.
#[derive(Default)]
pub struct Handoff {
    pub via: Via,
    /// What the sender calls the receiving machine (`me@box`), for its reports.
    pub host: Option<String>,
    /// Keep what was sent (secrets in the envfile, `persist: local` variables in `local.env`).
    pub persist: bool,
    /// With `persist`: replace values that differ.
    pub force: bool,
    /// `providers.envfile` of `secrets.yml`: where `persist` writes.
    pub envfile_path: Option<String>,
    pub files_dir: Option<String>,
    pub secrets: Vec<SentSecret>,
    pub vars: Vec<SentVar>,
}

impl std::fmt::Debug for Handoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handoff")
            .field("persist", &self.persist)
            .field("force", &self.force)
            .field("secrets", &self.secret_names())
            .field("vars", &self.var_names())
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "kebab-case", deny_unknown_fields)]
enum Meta {
    DotHandoff {
        v: u32,
        via: Via,
        host: Option<String>,
        persist: bool,
        force: bool,
        envfile_path: Option<String>,
        files_dir: Option<String>,
    },
    Secret {
        name: String,
        file: bool,
        target: Option<String>,
        len: usize,
    },
    Var {
        name: String,
        persist: bool,
        keep: bool,
        sensitive: bool,
        len: usize,
    },
    End,
}

/// Secret names as `secrets.yml` allows them.
fn valid_secret_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Variable names as `--var` allows them.
fn valid_var_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A host label as ssh takes it: printable, no blanks.
fn valid_host(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && s.chars().all(|c| c.is_ascii_graphic())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl Handoff {
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty() && self.vars.is_empty()
    }

    pub fn secret_names(&self) -> Vec<&str> {
        self.secrets.iter().map(|s| s.name.as_str()).collect()
    }

    pub fn var_names(&self) -> Vec<&str> {
        self.vars.iter().map(|v| v.name.as_str()).collect()
    }

    /// "2 secrets (a, b) and 1 variable (EMAIL)": names and counts, never a value; an empty
    /// part is left out.
    pub fn describe(&self) -> String {
        let part = |what: &str, names: Vec<&str>| {
            (!names.is_empty()).then(|| {
                format!(
                    "{} {what}{} ({})",
                    names.len(),
                    plural(names.len()),
                    names.join(", ")
                )
            })
        };
        match (
            part("secret", self.secret_names()),
            part("variable", self.var_names()),
        ) {
            (Some(s), Some(v)) => format!("{s} and {v}"),
            (Some(one), None) | (None, Some(one)) => one,
            (None, None) => "nothing".to_string(),
        }
    }

    /// What the receiver calls itself in its reports: the sender's name for it.
    fn label(&self) -> &str {
        self.host.as_deref().unwrap_or("this machine")
    }

    /// The stream. Zeroed when dropped.
    pub fn encode(&self) -> Result<Zeroizing<Vec<u8>>> {
        // Sized up front: a growing buffer would leave copies of values behind in freed memory.
        let size: usize = 1024
            + self
                .secrets
                .iter()
                .map(|s| s.value.len() + s.name.len() + 512)
                .sum::<usize>()
            + self
                .vars
                .iter()
                .map(|v| v.value.len() + v.name.len() + 256)
                .sum::<usize>();
        let mut out = Zeroizing::new(Vec::with_capacity(size));
        let meta = |m: &Meta, out: &mut Vec<u8>| -> Result<()> {
            serde_json::to_writer(&mut *out, m)?;
            out.push(b'\n');
            Ok(())
        };
        meta(
            &Meta::DotHandoff {
                v: VERSION,
                via: self.via,
                host: self.host.clone(),
                persist: self.persist,
                force: self.force,
                envfile_path: self.envfile_path.clone(),
                files_dir: self.files_dir.clone(),
            },
            &mut out,
        )?;
        for s in &self.secrets {
            meta(
                &Meta::Secret {
                    name: s.name.clone(),
                    file: s.file,
                    target: s.target.clone(),
                    len: s.value.len(),
                },
                &mut out,
            )?;
            out.extend_from_slice(s.value.expose());
            out.push(b'\n');
        }
        for v in &self.vars {
            meta(
                &Meta::Var {
                    name: v.name.clone(),
                    persist: v.persist,
                    keep: v.keep,
                    sensitive: v.sensitive,
                    len: v.value.len(),
                },
                &mut out,
            )?;
            out.extend_from_slice(v.value.as_bytes());
            out.push(b'\n');
        }
        meta(&Meta::End, &mut out)?;
        Ok(out)
    }

    /// Reads a stream made by [`Handoff::encode`], checking every record.
    pub fn decode(r: impl Read) -> Result<Handoff> {
        let mut r = BufReader::new(r);
        let mut total = 0usize;
        let next = |r: &mut BufReader<_>| -> Result<Meta> {
            let mut line = Vec::new();
            r.by_ref()
                .take(4096)
                .read_until(b'\n', &mut line)
                .context("reading what was handed over")?;
            if line.last() != Some(&b'\n') {
                if line.len() < 4096 {
                    bail!("what was handed over ended early (the sending side stopped?)");
                }
                bail!("what was handed over is malformed (a record is too long)");
            }
            serde_json::from_slice(&line).map_err(|_| {
                anyhow::anyhow!(
                    "what was handed over is not a dot {VERSION} stream (is the dot here older or \
                     newer than the one that sent it?)"
                )
            })
        };
        let payload = |r: &mut BufReader<_>, len: usize, total: &mut usize| -> Result<_> {
            if len > MAX_VALUE {
                bail!("a value handed over is larger than {} MiB", MAX_VALUE >> 20);
            }
            *total += len;
            if *total > MAX_TOTAL {
                bail!(
                    "what was handed over is larger than {} MiB",
                    MAX_TOTAL >> 20
                );
            }
            let mut buf = Zeroizing::new(vec![0u8; len]);
            r.read_exact(&mut buf)
                .context("what was handed over ended early")?;
            let mut nl = [0u8; 1];
            r.read_exact(&mut nl)
                .context("what was handed over ended early")?;
            if nl[0] != b'\n' {
                bail!("what was handed over is malformed (a value's length is wrong)");
            }
            Ok(buf)
        };
        let mut h = match next(&mut r)? {
            Meta::DotHandoff {
                v,
                via,
                host,
                persist,
                force,
                envfile_path,
                files_dir,
            } => {
                if v != VERSION {
                    bail!(
                        "what was handed over is version {v} of the stream; this dot reads \
                         version {VERSION}: use the same dot on both machines"
                    );
                }
                if host.as_deref().is_some_and(|h| !valid_host(h)) {
                    bail!("what was handed over names the host oddly");
                }
                Handoff {
                    via,
                    host,
                    persist,
                    force,
                    envfile_path,
                    files_dir,
                    ..Handoff::default()
                }
            }
            _ => bail!("what was handed over does not start with its header"),
        };
        loop {
            match next(&mut r)? {
                Meta::End => break,
                Meta::DotHandoff { .. } => bail!("what was handed over has two headers"),
                Meta::Secret {
                    name,
                    file,
                    target,
                    len,
                } => {
                    if !valid_secret_name(&name) || h.secrets.iter().any(|s| s.name == name) {
                        bail!("what was handed over names secret `{name}` twice or oddly");
                    }
                    if let Some(t) = &target {
                        SecretRef::parse(t).map_err(|e| anyhow::anyhow!("secret `{name}`: {e}"))?;
                    }
                    let bytes = payload(&mut r, len, &mut total)?;
                    h.secrets.push(SentSecret {
                        name,
                        file,
                        target,
                        value: SecretValue::new(bytes.to_vec()),
                    });
                }
                Meta::Var {
                    name,
                    persist,
                    keep,
                    sensitive,
                    len,
                } => {
                    if !valid_var_name(&name) || h.vars.iter().any(|v| v.name == name) {
                        bail!("what was handed over names variable `{name}` twice or oddly");
                    }
                    let bytes = payload(&mut r, len, &mut total)?;
                    let value = String::from_utf8(bytes.to_vec()).map_err(|e| {
                        let mut b = e.into_bytes();
                        zeroize::Zeroize::zeroize(&mut b);
                        anyhow::anyhow!("variable `{name}` handed over is not UTF-8 text")
                    })?;
                    h.vars.push(SentVar {
                        name,
                        value: Zeroizing::new(value),
                        persist,
                        keep,
                        sensitive,
                    });
                }
            }
        }
        Ok(h)
    }

    /// The variables this machine keeps in its `local.env`.
    fn kept_vars(&self) -> impl Iterator<Item = &SentVar> {
        self.vars
            .iter()
            .filter(|v| (self.persist && v.persist) || v.keep)
    }

    /// The secrets this machine keeps in its envfile: with `persist`, those with a target.
    fn kept_secrets(&self) -> impl Iterator<Item = &SentSecret> {
        self.secrets
            .iter()
            .filter(|s| self.persist && s.target.is_some())
    }

    /// Whether anything is to be kept on this machine.
    fn keeps_anything(&self) -> bool {
        self.kept_secrets().next().is_some() || self.kept_vars().next().is_some()
    }
}

/// Reads the stream from `source`: `-` is stdin, anything else a FIFO made by [`relay`].
pub fn read_from(source: &str) -> Result<Handoff> {
    if source == "-" {
        return Handoff::decode(std::io::stdin().lock());
    }
    read_fifo(Path::new(source))
}

#[cfg(unix)]
fn read_fifo(path: &Path) -> Result<Handoff> {
    use std::os::unix::fs::FileTypeExt;
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot find {}", path.display()))?;
    if !meta.file_type().is_fifo() {
        bail!("{} is not a FIFO", path.display());
    }
    // Opening blocks until the relay opens the other end; it may never (it failed): wait for a
    // while only.
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::File::open(&p).map(Handoff::decode));
    });
    match rx.recv_timeout(FIFO_WAIT) {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => Err(e).with_context(|| format!("cannot open {}", path.display())),
        Err(_) => bail!(
            "nothing was handed over through {} in {} s",
            path.display(),
            FIFO_WAIT.as_secs()
        ),
    }
}

#[cfg(not(unix))]
fn read_fifo(path: &Path) -> Result<Handoff> {
    bail!(
        "{}: receiving through a FIFO needs Linux or macOS",
        path.display()
    )
}

/// Whether the other end of `fd` (a pipe or socket we write to) is gone.
#[cfg(unix)]
fn hung_up(fd: std::os::fd::RawFd) -> bool {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one valid pollfd, no wait.
    let n = unsafe { libc::poll(&mut p, 1, 0) };
    n > 0 && p.revents & (libc::POLLHUP | libc::POLLERR) != 0
}

/// `dot secrets relay` (run by `dot apply --host` when ssh's stdin is the terminal): makes a
/// FIFO in a new owner-only directory under `$TMPDIR` (else `/tmp`) and prints its path, reads
/// stdin (in memory), then writes it into the FIFO once the run there opens it, and removes both.
/// Bytes go through memory and the kernel's pipe only: nothing is written to disk. It gives up
/// when nobody opens the FIFO in time, or as soon as the session that started it is gone (its
/// stdout hangs up): the sending side stops it when the run ended without reading.
#[cfg(unix)]
pub fn relay() -> Result<std::process::ExitCode> {
    use std::io::Write as _;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = tempfile::Builder::new()
        .prefix("dot-handoff-")
        .tempdir()
        .context("creating a private directory")?;
    let fifo = dir.path().join("s");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes())
        .context("the FIFO's path holds a NUL")?;
    // SAFETY: `c` is a valid NUL-terminated path.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot make the FIFO {}", fifo.display()));
    }
    {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{}", fifo.display())?;
        out.flush()?;
    }
    // Everything first: the sender closes its end once it has written it all.
    let mut data = Zeroizing::new(Vec::with_capacity(64 * 1024));
    std::io::stdin()
        .lock()
        .take((MAX_TOTAL + (1 << 20)) as u64)
        .read_to_end(&mut data)
        .context("reading stdin")?;
    // Opening for writing without a reader fails (ENXIO) instead of blocking: poll, so a run
    // that never opens it does not keep this process waiting.
    let stdout = std::io::stdout().as_raw_fd();
    let deadline = std::time::Instant::now() + FIFO_WAIT;
    let file = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
        {
            Ok(f) => break f,
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
                if hung_up(stdout) {
                    bail!("the session that started this relay is gone");
                }
                if std::time::Instant::now() > deadline {
                    bail!(
                        "nobody read {} in {} s",
                        fifo.display(),
                        FIFO_WAIT.as_secs()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e).with_context(|| format!("cannot open {}", fifo.display())),
        }
    };
    // Both ends are open: the name is no longer needed (and nothing is left behind should this
    // process be stopped while writing).
    let _ = std::fs::remove_file(&fifo);
    let _ = dir.close();
    // Blocking writes from now on.
    // SAFETY: `file` owns a valid descriptor for the duration of these calls.
    unsafe {
        let fd = file.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }
    let mut file = file;
    file.write_all(&data)
        .context("handing over through the FIFO")?;
    drop(file);
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(not(unix))]
pub fn relay() -> Result<std::process::ExitCode> {
    bail!("`dot secrets relay` needs Linux or macOS")
}

/// What a run started by another machine (`--secrets-from`) was handed: the values it uses, and
/// what it keeps once it goes ahead. As a prepare stage (the last one, after the confirmation),
/// it writes what is to be kept.
#[derive(Default)]
pub struct Received {
    /// Secrets for this run, used before any provider is asked. A secret this machine keeps a
    /// different value of (`persist` without `force`) is not among them: its own value is used.
    pub secrets: Vec<(String, SecretValue)>,
    /// Values of the variables the sender flagged sensitive: redacted from this run's output.
    pub sensitive: Vec<Zeroizing<String>>,
    /// Kept once the run goes ahead.
    pending: Option<(Handoff, Inputs)>,
    /// What keeping it did: report lines (names only), shown once the run is over.
    pub report: Vec<String>,
}

impl Received {
    /// Keeps what is to be kept (once).
    pub fn keep(&mut self) -> Result<()> {
        if let Some((h, inputs)) = self.pending.take() {
            self.report = keep(&h, &inputs, false)?
                .lines
                .into_iter()
                .map(|l| format!("dot: {}: {l}", h.label()))
                .collect();
        }
        Ok(())
    }

    /// Prints what keeping it did.
    pub fn print_report(&self) {
        for line in &self.report {
            eprintln!("{line}");
        }
    }
}

impl dot_core::engine::Prepare for Received {
    fn prepare(&mut self, _plan: &Plan, _sink: &EventSink) -> Result<()> {
        self.keep()
    }
}

/// For a `dot apply` / `dot plan` started by another machine (`--secrets-from`): reads what it
/// handed over, adds the variables to `vars` (as `--var`s) and returns the rest (see
/// [`Received`]). Nothing is written here; with `dry_run` nothing ever is.
pub fn receive_for_run(
    source: &str,
    vars: &mut Vec<String>,
    dry_run: bool,
    inputs: impl FnOnce() -> Result<Inputs>,
) -> Result<Received> {
    received(read_from(source)?, vars, dry_run, inputs)
}

/// [`receive_for_run`] of a stream already read.
fn received(
    h: Handoff,
    vars: &mut Vec<String>,
    dry_run: bool,
    inputs: impl FnOnce() -> Result<Inputs>,
) -> Result<Received> {
    let mut out = Received::default();
    // Secrets this machine keeps its own (different) value of: the run uses that one.
    let mut own = Kept::default();
    if h.keeps_anything() {
        let inputs = inputs()?;
        let preview = keep(&h, &inputs, true)?;
        if dry_run {
            eprintln!(
                "dot: {}: dry run: nothing kept; a real run would: {}",
                h.label(),
                preview.lines.join("; ")
            );
        } else {
            own = preview;
            out.pending = Some((Handoff::default(), inputs));
        }
    }
    vars.extend(
        h.vars
            .iter()
            .map(|v| format!("{}={}", v.name, v.value.as_str())),
    );
    out.sensitive = h
        .vars
        .iter()
        .filter(|v| v.sensitive)
        .map(|v| v.value.clone())
        .collect();
    out.secrets = h
        .secrets
        .iter()
        .filter(|s| !own.secrets.contains(&s.name))
        .map(|s| (s.name.clone(), s.value.clone()))
        .collect();
    if let Some((slot, _)) = out.pending.as_mut() {
        *slot = h;
    }
    Ok(out)
}

/// What [`keep`] did (or would do).
#[derive(Default)]
struct Kept {
    /// Report lines: names only.
    lines: Vec<String>,
    /// Secrets left alone: a different value is here.
    secrets: BTreeSet<String>,
}

/// Keeps what `h` says to keep on this machine (`dry`: works out what that would do): its
/// secrets in the envfile (as `providers.envfile` places it), its kept variables in
/// `local.env`.
fn keep(h: &Handoff, inputs: &Inputs, dry: bool) -> Result<Kept> {
    let env = crate::secrets::provider_env(inputs);
    let config = EnvfileConfig {
        path: h.envfile_path.clone(),
        files_dir: h.files_dir.clone(),
    };
    let provider = EnvfileProvider::from_config(Some(&config), &env);
    let mut out = Kept::default();
    let mut parsed = Vec::new();
    for sec in h.kept_secrets() {
        let t = sec.target.as_deref().unwrap_or_default();
        parsed.push((
            sec,
            SecretRef::parse(t).map_err(|e| anyhow::anyhow!("secret `{}`: {e}", sec.name))?,
        ));
    }
    let items: Vec<(&str, &SecretRef, &SecretValue)> = parsed
        .iter()
        .map(|(sec, r)| (sec.name.as_str(), r, &sec.value))
        .collect();
    if !items.is_empty() {
        let merged = dot_secrets::merge(&provider, &items, h.force, dry)?;
        let mut places = Vec::new();
        if items.iter().any(|(_, r, _)| !r.is_file()) {
            places.push(dot_core::paths::display(provider.path(), &inputs.home));
        }
        if items.iter().any(|(_, r, _)| r.is_file()) {
            places.push(dot_core::paths::display(provider.files_dir(), &inputs.home));
        }
        out.lines.push(format!(
            "secrets kept in {}: {}",
            places.join(", "),
            report(&merged, h.via, h.via == Via::Apply)
        ));
        out.secrets.extend(
            merged
                .iter()
                .filter(|(_, m)| *m == Merge::Kept)
                .map(|(n, _)| n.clone()),
        );
    }
    let keep: Vec<&SentVar> = h.kept_vars().collect();
    if !keep.is_empty() {
        let entries: Vec<(&str, &str)> = keep
            .iter()
            .map(|v| (v.name.as_str(), v.value.as_str()))
            .collect();
        let local = inputs.local_env();
        let merged = dot_secrets::merge_env(
            &local,
            &entries,
            keep.iter().any(|v| v.sensitive),
            h.force,
            dry,
        )?;
        out.lines.push(format!(
            "variables kept in {}: {}",
            dot_core::paths::display(&local, &inputs.home),
            report(&merged, h.via, false)
        ));
    }
    Ok(out)
}

/// "added a, b; unchanged c; kept d (a different value is here; --force-secrets replaces it)".
/// `used`: the run uses a kept value.
fn report(merged: &[(String, Merge)], via: Via, used: bool) -> String {
    let mut parts = Vec::new();
    for m in [Merge::Added, Merge::Updated, Merge::Unchanged, Merge::Kept] {
        let names: Vec<&str> = merged
            .iter()
            .filter(|(_, r)| *r == m)
            .map(|(n, _)| n.as_str())
            .collect();
        if names.is_empty() {
            continue;
        }
        let mut part = format!("{} {}", m.as_str(), names.join(", "));
        if m == Merge::Kept {
            part.push_str(&format!(
                " (a different value is here{}; {} replaces it)",
                if used { ", and this run uses it" } else { "" },
                via.force_flag()
            ));
        }
        parts.push(part);
    }
    parts.join("; ")
}

/// `dot secrets receive`, run by `dot secrets push` on the other machine: keeps what is handed
/// over on stdin.
pub fn receive() -> Result<std::process::ExitCode> {
    let h = read_from("-")?;
    if !h.persist {
        bail!("nothing to do: what was handed over is not meant to be kept");
    }
    let inputs = crate::commands::inputs(&crate::RepoArgs::default())?;
    for line in keep(&h, &inputs, false)?.lines {
        eprintln!("dot: {}: {line}", h.label());
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Handoff {
        Handoff {
            via: Via::Apply,
            host: Some("me@box".into()),
            persist: true,
            force: false,
            envfile_path: Some("~/s/secrets.env".into()),
            files_dir: None,
            secrets: vec![
                SentSecret {
                    name: "tok".into(),
                    file: false,
                    target: Some("envfile:API_TOKEN".into()),
                    value: SecretValue::from("line1\nline2 {\"t\":\"end\"}\n".to_string()),
                },
                SentSecret {
                    name: "vpn.p12".into(),
                    file: true,
                    target: Some("envfile-file:vpn/work.p12".into()),
                    value: SecretValue::new(vec![0, 0xff, b'\n', 1]),
                },
                SentSecret {
                    name: "glab".into(),
                    file: false,
                    target: None,
                    value: SecretValue::from(String::new()),
                },
            ],
            vars: vec![SentVar {
                name: "EMAIL".into(),
                value: Zeroizing::new("me@example.com".into()),
                persist: true,
                keep: false,
                sensitive: false,
            }],
        }
    }

    #[test]
    fn the_stream_round_trips_any_bytes() {
        let bytes = sample().encode().unwrap();
        let back = Handoff::decode(bytes.as_slice()).unwrap();
        assert!(back.persist && !back.force);
        assert_eq!(
            (back.via, back.host.as_deref()),
            (Via::Apply, Some("me@box"))
        );
        assert_eq!(back.envfile_path.as_deref(), Some("~/s/secrets.env"));
        assert_eq!(back.secret_names(), ["tok", "vpn.p12", "glab"]);
        assert_eq!(
            back.secrets[0].value.expose(),
            b"line1\nline2 {\"t\":\"end\"}\n"
        );
        assert_eq!(back.secrets[1].value.expose(), [0, 0xff, b'\n', 1]);
        assert!(back.secrets[1].file);
        assert_eq!(back.secrets[2].target, None);
        assert_eq!(back.vars[0].value.as_str(), "me@example.com");
        assert_eq!(
            format!("{back:?}"),
            "Handoff { persist: true, force: false, secrets: [\"tok\", \"vpn.p12\", \"glab\"], \
             vars: [\"EMAIL\"] }"
        );
        assert_eq!(
            back.describe(),
            "3 secrets (tok, vpn.p12, glab) and 1 variable (EMAIL)"
        );
        // An empty part is left out.
        let mut only = sample();
        only.vars.clear();
        assert_eq!(only.describe(), "3 secrets (tok, vpn.p12, glab)");
        only.secrets.clear();
        assert_eq!(only.describe(), "nothing");
        let mut vars = sample();
        vars.secrets.clear();
        assert_eq!(vars.describe(), "1 variable (EMAIL)");
        // Every metadata line is JSON without a value.
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.starts_with(
                "{\"t\":\"dot-handoff\",\"v\":1,\"via\":\"apply\",\"host\":\"me@box\","
            ),
            "{text}"
        );
        assert!(text.ends_with("{\"t\":\"end\"}\n"), "{text}");
    }

    #[test]
    fn bad_streams_are_refused() {
        let good = sample().encode().unwrap();
        let cut = &good[..good.len() - 5];
        for (bytes, needle) in [
            (&b""[..], "ended early"),
            (cut, "ended early"),
            (&b"{\"t\":\"end\"}\n"[..], "header"),
            (&b"hello\n"[..], "not a dot"),
            (
                &b"{\"t\":\"dot-handoff\",\"v\":2,\"via\":\"apply\",\"host\":null,\"persist\":false,\"force\":false,\"envfile_path\":null,\"files_dir\":null}\n"[..],
                "version 2",
            ),
        ] {
            let e = format!("{:#}", Handoff::decode(bytes).unwrap_err());
            assert!(e.contains(needle), "{needle}: {e}");
        }
        // A length that does not match the value.
        let h = "{\"t\":\"dot-handoff\",\"v\":1,\"via\":\"apply\",\"host\":null,\"persist\":false,\"force\":false,\"envfile_path\":null,\"files_dir\":null}\n";
        let bad = format!(
            "{h}{{\"t\":\"secret\",\"name\":\"a\",\"file\":false,\"target\":null,\"len\":2}}\nabc\n{{\"t\":\"end\"}}\n"
        );
        let e = format!("{:#}", Handoff::decode(bad.as_bytes()).unwrap_err());
        assert!(e.contains("length is wrong"), "{e}");
        let odd = format!(
            "{h}{{\"t\":\"var\",\"name\":\"A B\",\"persist\":false,\"keep\":false,\"sensitive\":false,\"len\":1}}\nx\n{{\"t\":\"end\"}}\n"
        );
        let e = format!("{:#}", Handoff::decode(odd.as_bytes()).unwrap_err());
        assert!(e.contains("oddly"), "{e}");
    }

    #[test]
    fn persisting_writes_owner_only_and_reports_names() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let inputs = Inputs {
            home: home.clone(),
            cwd: home.clone(),
            ..Inputs::default()
        };
        let mut h = sample();
        h.envfile_path = None;
        // Paths as this OS spells them under `~`.
        let lines: Vec<String> = keep(&h, &inputs, false)
            .unwrap()
            .lines
            .iter()
            .map(|l| l.replace('\\', "/"))
            .collect();
        assert_eq!(
            lines,
            [
                "secrets kept in ~/.config/dot/secrets.env, ~/.config/dot/files: added tok, vpn.p12",
                "variables kept in ~/.config/dot/local.env: added EMAIL",
            ]
        );
        let env = home.join(".config/dot/secrets.env");
        let loaded: Vec<(String, String)> = dotenvy::from_path_iter(&env)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(loaded[0].1, "line1\nline2 {\"t\":\"end\"}\n");
        assert_eq!(
            std::fs::read(home.join(".config/dot/files/vpn/work.p12")).unwrap(),
            [0, 0xff, b'\n', 1]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&env, &home.join(".config/dot/files/vpn/work.p12")] {
                let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{}", p.display());
            }
        }
        // Again, with another value there: kept unless forced.
        std::fs::write(
            home.join(".config/dot/local.env"),
            "EMAIL=other@example.com\n",
        )
        .unwrap();
        let lines = keep(&h, &inputs, false).unwrap().lines;
        assert!(lines[0].ends_with("unchanged tok, vpn.p12"), "{lines:?}");
        assert!(
            lines[1]
                .ends_with("kept EMAIL (a different value is here; --force-secrets replaces it)"),
            "{lines:?}"
        );
        // Pushed: no run, and push's own flag.
        h.via = Via::Push;
        let lines = keep(&h, &inputs, false).unwrap().lines;
        assert!(
            lines[1].ends_with("kept EMAIL (a different value is here; --force replaces it)"),
            "{lines:?}"
        );
        h.force = true;
        let lines = keep(&h, &inputs, false).unwrap().lines;
        assert!(lines[1].ends_with("updated EMAIL"), "{lines:?}");
    }

    fn home() -> (tempfile::TempDir, Inputs) {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let inputs = Inputs {
            home: home.clone(),
            cwd: home,
            ..Inputs::default()
        };
        (tmp, inputs)
    }

    fn var(name: &str, value: &str, persist: bool, keep: bool, sensitive: bool) -> SentVar {
        SentVar {
            name: name.into(),
            value: Zeroizing::new(value.into()),
            persist,
            keep,
            sensitive,
        }
    }

    #[test]
    fn a_run_uses_what_this_machine_keeps_and_writes_only_once_it_goes_ahead() {
        let (_tmp, inputs) = home();
        let dir = inputs.home.join(".config/dot");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("local.env"), "EMAIL=mine@here\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        dot_secrets::persist_env(
            &dir.join("secrets.env"),
            &[("API_TOKEN", "own-token")],
            true,
        )
        .unwrap();
        let mut h = sample();
        h.envfile_path = None;
        h.vars = vec![
            var("EMAIL", "sent@there", true, false, false),
            var("PIN", "4242-sensitive", false, false, true),
        ];
        let mut vars = Vec::new();
        let mut r = received(h, &mut vars, false, || Ok(inputs.clone())).unwrap();
        // Its own value of `tok`: the sent one is not used. A variable sent over its own value
        // (a `--var` there) still is, for this run.
        let names: Vec<&str> = r.secrets.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["vpn.p12", "glab"]);
        assert_eq!(vars, ["EMAIL=sent@there", "PIN=4242-sensitive"]);
        assert_eq!(r.sensitive.len(), 1);
        assert_eq!(r.sensitive[0].as_str(), "4242-sensitive");
        // Nothing written until the run goes ahead.
        assert!(!dir.join("files/vpn/work.p12").exists());
        r.keep().unwrap();
        assert!(dir.join("files/vpn/work.p12").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("local.env")).unwrap(),
            "EMAIL=mine@here\n"
        );
        let report = r.report.join("\n").replace('\\', "/");
        assert!(
            report.contains("dot: me@box: secrets kept in ~/.config/dot/secrets.env, ~/.config/dot/files: added vpn.p12; kept tok"),
            "{report}"
        );
        // Once only.
        r.report.clear();
        r.keep().unwrap();
        assert!(r.report.is_empty());
    }

    #[test]
    fn a_per_machine_answer_is_kept_without_persist_and_a_dry_run_keeps_nothing() {
        let (_tmp, inputs) = home();
        let h = || Handoff {
            host: Some("me@box".into()),
            vars: vec![
                var("MY_MACHINE", "box-one", true, true, false),
                var("EMAIL", "me@example.com", true, false, false),
            ],
            ..Handoff::default()
        };
        let local = inputs.local_env();
        let mut vars = Vec::new();
        let mut r = received(h(), &mut vars, true, || Ok(inputs.clone())).unwrap();
        r.keep().unwrap();
        assert!(!local.exists(), "a dry run keeps nothing");
        assert_eq!(vars, ["MY_MACHINE=box-one", "EMAIL=me@example.com"]);

        let mut vars = Vec::new();
        let mut r = received(h(), &mut vars, false, || Ok(inputs.clone())).unwrap();
        assert!(!local.exists());
        r.keep().unwrap();
        // Only the per-machine answer: EMAIL is forwarded for this run only.
        assert_eq!(
            std::fs::read_to_string(&local).unwrap(),
            "MY_MACHINE=\"box-one\"\n"
        );
        assert_eq!(
            r.report.join("\n").replace('\\', "/"),
            "dot: me@box: variables kept in ~/.config/dot/local.env: added MY_MACHINE"
        );
        // Nothing to keep: inputs are not even looked up.
        let plain = Handoff {
            vars: vec![var("EMAIL", "x@y", true, false, false)],
            ..Handoff::default()
        };
        let mut vars = Vec::new();
        let r = received(plain, &mut vars, false, || bail!("not needed")).unwrap();
        assert!(r.pending.is_none());
    }
}
