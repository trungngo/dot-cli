//! `dot secrets push`, `export` and `import`: moving secrets between machines without ever going
//! through the repository.
//!
//! - `push [user@]host [NAME...]` resolves the secrets here, through this machine's providers
//!   (envfile, LastPass, ...: so a secret that lives only in a password manager can be pushed
//!   too), and hands them to `dot secrets receive` there on the ssh session's stdin (installing
//!   `dot` there first when it is missing, as `dot apply --host` does, or from `--dot-binary`,
//!   and checking that the `dot` there is this tool, recent enough, before anything is
//!   resolved). The remote keeps each one
//!   where its `secrets.yml` entry's envfile ref says (`ref:` or `fallback:`), in its own
//!   `~/.config/dot/secrets.env` and `files/`, owner-only; a different value already there is
//!   replaced only with `--force`. Never on a command line, in the environment or in a file.
//! - `export` writes an age archive encrypted to a passphrase (see
//!   [`dot_secrets::transfer`]), to stdout or `-o FILE`; `import` restores one owner-only and
//!   refuses to overwrite existing files without `--force`. The passphrase is read without echo
//!   on the terminal, or from `--passphrase-file`.
//!
//! The envfile's location comes from the repository's `secrets.yml` when there is one (so a
//! custom `providers.envfile.path` is honoured), else the default `~/.config/dot/secrets.env`.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::Args;
use dot_secrets::transfer::{Archive, Inventory};
use dot_secrets::{EnvfileProvider, SecretValue, SecretsConfig};

use crate::RepoArgs;
use crate::commands::inputs;
use crate::secrets::{provider_env, secrets_file};

#[derive(Args)]
pub struct PushArgs {
    /// Where to: `[user@]host` as ssh understands it (aliases from ~/.ssh/config work).
    #[arg(value_name = "[USER@]HOST")]
    pub host: String,
    /// The secrets to push (default: every one secrets.yml declares that resolves here).
    #[arg(value_name = "NAME")]
    pub names: Vec<String>,
    /// Replace values the remote already holds that differ.
    #[arg(long)]
    pub force: bool,
    /// Say what would be pushed and where the remote would keep it; resolve and send nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Install (or upgrade) dot there from this file, unless the dot there is this same build
    /// (same SHA-256) or newer, copied over with scp to ~/.local/bin/dot instead of downloaded:
    /// nothing is fetched from GitHub. It must be a dot of this version built for the remote's
    /// OS and CPU (for a Linux server, a static musl build: scripts/build-linux.sh). Default:
    /// DOT_HOST_BINARY.
    #[arg(long, value_name = "PATH", value_parser = crate::path_arg())]
    pub dot_binary: Option<PathBuf>,
    #[command(flatten)]
    pub repo: RepoArgs,
}

#[derive(Args)]
pub struct ExportArgs {
    /// Encrypt with age to a passphrase (the only format, and the default).
    #[arg(long, default_value_t = true)]
    pub age: bool,
    /// Write the archive here instead of to stdout.
    #[arg(short, long, value_name = "FILE", value_parser = crate::path_arg())]
    pub output: Option<PathBuf>,
    /// Read the passphrase from the first line of FILE instead of asking.
    #[arg(long, value_name = "FILE", value_parser = crate::path_arg())]
    pub passphrase_file: Option<PathBuf>,
    #[command(flatten)]
    pub repo: RepoArgs,
}

#[derive(Args)]
pub struct ImportArgs {
    /// The archive (default: stdin).
    #[arg(value_name = "FILE", value_parser = crate::path_arg())]
    pub file: Option<PathBuf>,
    /// Overwrite secrets that already exist.
    #[arg(long)]
    pub force: bool,
    /// Read the passphrase from the first line of FILE instead of asking.
    #[arg(long, value_name = "FILE", value_parser = crate::path_arg())]
    pub passphrase_file: Option<PathBuf>,
    #[command(flatten)]
    pub repo: RepoArgs,
}

/// The envfile provider as this machine's configuration places it, and the home directory.
pub(crate) fn envfile(repo: &RepoArgs) -> Result<(EnvfileProvider, PathBuf)> {
    let inputs = inputs(repo)?;
    let penv = provider_env(&inputs);
    let local = inputs.local_control().map_err(anyhow::Error::msg)?;
    let located = dot_core::repo::find_repo(
        inputs.repo.as_deref(),
        inputs.env.get("DOT_REPO").map(String::as_str),
        &inputs.cwd,
        local.get("DOT_REPO").map(String::as_str),
        &inputs.home,
    );
    let config = match located {
        Ok(l) if secrets_file(&l.root).exists() => {
            Some(SecretsConfig::load(&secrets_file(&l.root)).map_err(anyhow::Error::msg)?)
        }
        // An explicit --repo that cannot be found is an error; otherwise the defaults apply.
        Err(e) if repo.repo.is_some() => bail!("{e}"),
        _ => None,
    };
    let envfile_config = config.as_ref().and_then(|c| c.providers.envfile.as_ref());
    Ok((
        EnvfileProvider::from_config(envfile_config, &penv),
        inputs.home,
    ))
}

// ---------------------------------------------------------------------------------------------
// push

pub fn push(a: PushArgs) -> Result<ExitCode> {
    crate::remote::check_host(&a.host)?;
    let root = crate::remote::local_root(&a.repo)?;
    let file = secrets_file(&root);
    if !file.exists() {
        bail!(
            "there is no {} in {}: nothing says which secrets to push (to copy the envfile as it \
             is, use `dot secrets export` / `import`)",
            crate::secrets::SECRETS_FILE,
            root.display()
        );
    }
    let repo = dot_core::repo::Repo::load(&root).map_err(|errs| {
        let lines: Vec<String> = errs.iter().map(ToString::to_string).collect();
        anyhow::anyhow!("{}", lines.join("\n"))
    })?;
    // Bare refs are read there with the remote profile's provider (`--profile`), else dot.yml's.
    let provider = match &a.repo.profile {
        Some(p) => {
            let profile = repo
                .config
                .profile(p)
                .with_context(|| format!("there is no profile `{p}`"))?;
            profile
                .secrets_provider
                .clone()
                .or_else(|| repo.config.secrets_provider.clone())
        }
        None => repo.config.secrets_provider.clone(),
    };
    let there = SecretsConfig::load(&file)
        .map_err(anyhow::Error::msg)?
        .with_provider_override(provider.as_deref());
    let named = !a.names.is_empty();
    let names: Vec<String> = if named {
        let undeclared: Vec<&str> = a
            .names
            .iter()
            .filter(|n| !there.secrets.contains_key(*n))
            .map(String::as_str)
            .collect();
        if !undeclared.is_empty() {
            bail!(
                "not declared in {}: {}",
                crate::secrets::SECRETS_FILE,
                undeclared.join(", ")
            );
        }
        a.names.clone()
    } else {
        there.secrets.keys().cloned().collect()
    };
    // Only what the remote can keep: a secret whose entry has an envfile ref.
    let (keepable, unkeepable): (Vec<String>, Vec<String>) = names
        .into_iter()
        .partition(|n| crate::remote::send::envfile_target(&there, n).is_some());
    if !unkeepable.is_empty() {
        let msg = format!(
            "{} {} no envfile ref (`ref:` or `fallback:`) to be kept under there",
            unkeepable.join(", "),
            if unkeepable.len() == 1 { "has" } else { "have" }
        );
        if named {
            bail!(
                "{msg}; add one (e.g. `fallback: \"envfile:NAME\"`) to {}",
                crate::secrets::SECRETS_FILE
            );
        }
        eprintln!("dot: {}: not pushed: {msg}", a.host);
    }
    if keepable.is_empty() {
        bail!("nothing to push");
    }
    // A --dot-binary (or DOT_HOST_BINARY) that cannot be one is refused, dry run or not.
    let given = crate::remote::binary::HostBinary::given(a.dot_binary.clone())?;
    if a.dry_run {
        for n in &keepable {
            let t = crate::remote::send::envfile_target(&there, n).expect("keepable");
            println!("{n} -> {t}");
        }
        println!(
            "dry run: {} secret{} would be resolved here and kept on {}; nothing was resolved or \
             sent",
            keepable.len(),
            if keepable.len() == 1 { "" } else { "s" },
            a.host
        );
        return Ok(ExitCode::SUCCESS);
    }
    let mut here_args = a.repo.clone();
    here_args.profile = None;
    here_args.vars.clear();
    let here = inputs(&here_args)?;
    // The dot there first: installed when missing, and known to be this tool, recent enough,
    // before anything is unlocked or resolved here.
    let mut remote = crate::remote::Remote::connect(&a.host)?;
    remote.dot_binary = given;
    let probe = remote.probe()?;
    let dot = remote.ensure_dot(&probe)?;
    remote.host_info(&dot)?;
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let (secrets, notes) = crate::remote::send::resolve_here(
        &here,
        &file,
        &keepable,
        !named,
        interactive,
        &there,
        &a.host,
        crate::remote::send::Purpose::Push,
    )?;
    for n in notes {
        eprintln!("dot: {}: {n}", a.host);
    }
    if secrets.is_empty() {
        bail!("nothing to push: none of them resolved here");
    }
    let envfile = there.providers.envfile.clone().unwrap_or_default();
    let h = crate::handoff::Handoff {
        via: crate::handoff::Via::Push,
        host: Some(a.host.clone()),
        persist: true,
        force: a.force,
        envfile_path: envfile.path,
        files_dir: envfile.files_dir,
        secrets,
        vars: Vec::new(),
    };
    eprintln!("dot: {}: pushing {} on ssh's stdin", a.host, h.describe());
    let code = remote.receive(&dot, &h.encode()?)?;
    remote.close();
    Ok(match code {
        Some(0) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

// ---------------------------------------------------------------------------------------------
// export / import

/// The passphrase: the first line of `file`, else asked without echo on the terminal (twice
/// when `confirm`).
fn passphrase(file: Option<&Path>, confirm: bool) -> Result<SecretValue> {
    if let Some(f) = file {
        let text = zeroize::Zeroizing::new(
            std::fs::read_to_string(f)
                .with_context(|| format!("cannot read passphrase file {}", f.display()))?,
        );
        let line = text.lines().next().unwrap_or("");
        if line.is_empty() {
            bail!("passphrase file {} is empty", f.display());
        }
        return Ok(SecretValue::from(line.to_string()));
    }
    let term = console::Term::stderr();
    if !term.is_term() {
        bail!("no terminal to ask for a passphrase on; use --passphrase-file");
    }
    term.write_str("Passphrase: ")?;
    let first = zeroize::Zeroizing::new(term.read_secure_line()?);
    if first.is_empty() {
        bail!("the passphrase is empty");
    }
    if confirm {
        term.write_str("Passphrase (again): ")?;
        let again = zeroize::Zeroizing::new(term.read_secure_line()?);
        if *again != *first {
            bail!("the passphrases do not match");
        }
    }
    Ok(SecretValue::from(first.to_string()))
}

/// Writes `bytes` to `path`, owner-only on unix.
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("cannot write {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("cannot write {}", path.display()))
}

pub fn export(a: ExportArgs) -> Result<ExitCode> {
    if !a.age {
        bail!("choose a format: --age");
    }
    if a.output.is_none() && std::io::stdout().is_terminal() {
        bail!(
            "refusing to write an encrypted archive to a terminal; use -o FILE or redirect stdout"
        );
    }
    let (provider, _) = envfile(&a.repo)?;
    let inv = Inventory::of(&provider)?;
    if inv.is_empty() {
        bail!(
            "nothing to export: {} does not exist and there are no file secrets in {}",
            provider.path().display(),
            provider.files_dir().display()
        );
    }
    let archive = Archive::read(&inv)?;
    let pass = passphrase(a.passphrase_file.as_deref(), true)?;
    let sealed = archive.encrypt(&pass, None)?;
    match &a.output {
        Some(path) => {
            write_owner_only(path, &sealed)?;
            eprintln!(
                "exported {} to {}: {}",
                inv.paths().count(),
                path.display(),
                archive.names().collect::<Vec<_>>().join(", ")
            );
        }
        None => {
            let mut out = std::io::stdout().lock();
            out.write_all(&sealed)?;
            out.flush()?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn import(a: ImportArgs) -> Result<ExitCode> {
    let mut sealed = Vec::new();
    match a.file.as_deref() {
        Some(p) if p != Path::new("-") => {
            sealed = std::fs::read(p).with_context(|| format!("cannot read {}", p.display()))?;
        }
        _ => {
            if std::io::stdin().is_terminal() {
                bail!("give the archive as FILE or on stdin");
            }
            std::io::stdin().read_to_end(&mut sealed)?;
        }
    }
    let (provider, home) = envfile(&a.repo)?;
    let pass = passphrase(a.passphrase_file.as_deref(), false)?;
    let archive = Archive::decrypt(&sealed, &pass)?;
    if archive.is_empty() {
        bail!("the archive holds no secrets");
    }
    let written = archive.restore(&provider, a.force)?;
    for p in &written {
        println!("restored {}", dot_core::paths::display(p, &home));
    }
    Ok(ExitCode::SUCCESS)
}
