//! The sending side of `dot apply --host` and `dot plan --host` (and `dot secrets push`): what
//! this machine works out and resolves for a run on another one.
//!
//! - **The remote's run, planned here.** From what `dot host-info` reported (its facts, its
//!   pinned profile, the names its `local.env` sets), the repository is opened here as the
//!   remote would open it: its profile, its bundles, its variables. Nothing of this machine's own
//!   `local.env` takes part (another config directory is used), so the plan is the remote's.
//! - **Secrets.** By default, the names the remote's bundles use (read statically, as `dot
//!   check` does: `secret("...")` in bundle files and templates, `secret_file:`, `github_release`
//!   tokens); `--all-secrets`: every name `secrets.yml` declares. They are resolved here, through
//!   this machine's providers (its profile's `secrets_provider`), each unlocked once, so the
//!   remote needs no password manager. A required one that cannot be resolved stops the run
//!   before anything is sent, listing every such name; an `optional: true` one is skipped with a
//!   note; with `--all-secrets`, whatever does not resolve is skipped.
//! - **Variables.** `persist: local` variables with a value in this machine's `local.env` that
//!   the remote's `local.env` does not set (it keeps its own unless `--force-secrets`), and every
//!   `--var`, are forwarded (as `--var`s of the run there), except `per_machine: true` ones: the
//!   remote's own value is used, and when it has none it is asked for here ("value of MY_MACHINE
//!   for box"; a `persist: local` answer is kept there, as a local run keeps it), taken from
//!   `--var`, or the run stops, naming them.
//! - **The profile.** The remote's run is given the profile planned here (`--profile`), so the
//!   two cannot disagree. When that profile is the one the remote reported (its `DOT_PROFILE`,
//!   with no `--profile` here) and secrets or sensitive variables would go, the remote's own
//!   report is what chose them: this side asks before resolving anything, and without a
//!   terminal (or with `--yes`) refuses unless `--profile` names it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use dot_core::vars::{Layer, Persist, Rank, Source, VarDecl, looks_sensitive, parse_assignment};
use dot_core::workspace::{Inputs, Workspace};
use dot_core::{Bundle, Directive};
use dot_directives::github_release::TOKEN_SECRET;
use dot_secrets::{Interactive, Registry, Resolver, SecretRef, SecretValue, SecretsConfig};
use zeroize::Zeroizing;

use super::HostInfo;
use crate::RepoArgs;
use crate::handoff::{Handoff, SentSecret, SentVar, Via};
use crate::prompt::{LinePrompter, Prompter, StdinLines, TermAsker, VarAsker};

/// Which secrets go to the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// Those the remote's bundles use.
    Needed,
    /// Every one `secrets.yml` declares that resolves here.
    All,
    None,
}

pub struct Sender {
    pub host: String,
    /// The local repository.
    pub root: PathBuf,
    /// This command line's `--profile` (for the remote) and `--var`s.
    pub repo: RepoArgs,
    pub bundles: Vec<String>,
    pub no_deps: bool,
    pub secrets: Which,
    /// `--persist-secrets` / `--force-secrets`.
    pub persist: bool,
    pub force: bool,
    /// Questions can be asked here (a terminal, no `--yes`).
    pub interactive: bool,
    /// Per-machine variables without a value there are asked for (else the run stops): `dot
    /// apply`. `dot plan` plans without them.
    pub ask_per_machine: bool,
}

/// What [`Sender::prepare`] worked out.
#[derive(Default)]
pub struct Prepared {
    /// What to hand over (`None`: nothing).
    pub handoff: Option<Handoff>,
    /// The profile the run there was planned with: it is given that one.
    pub profile: Option<String>,
}

/// Why secrets are resolved here: words the error when one is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// For a run there (`dot apply --host`).
    Run,
    /// To keep there (`dot secrets push`).
    Push,
}

/// `s` with anything but printable ASCII replaced: text the remote reported, shown here.
pub(crate) fn printable(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// The remote's run, planned here.
struct Planned {
    ws: Workspace,
    res: dot_core::graph::Resolution,
    bundles: Vec<Bundle>,
    /// Every declaration the run sees (`dot.yml`, the profile, the bundles).
    decls: Vec<(String, VarDecl)>,
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

impl Sender {
    fn say(&self, msg: impl AsRef<str>) {
        eprintln!("dot: {}: {}", self.host, msg.as_ref());
    }

    fn cli_vars(&self) -> Result<Vec<(String, String)>> {
        self.repo
            .vars
            .iter()
            .map(|s| parse_assignment(s).map_err(anyhow::Error::msg))
            .collect::<Result<Vec<_>>>()
            .context("--var")
    }

    /// This machine's inputs, as every local command sees them.
    fn here(&self) -> Result<Inputs> {
        let mut repo = self.repo.clone();
        repo.profile = None;
        repo.vars.clear();
        crate::commands::inputs(&repo)
    }

    /// Opens the repository as the remote would, from what it reported.
    fn plan_there(
        &self,
        info: &HostInfo,
        facts: dot_core::Facts,
        cli: &[(String, String)],
    ) -> Result<Planned> {
        // Not this machine's config directory: its local.env (values, pinned profile) belongs
        // to this machine, not to the remote.
        let elsewhere = tempfile::tempdir().context("creating a temporary directory")?;
        let mut env = BTreeMap::new();
        env.insert(
            "XDG_CONFIG_HOME".to_string(),
            elsewhere.path().display().to_string(),
        );
        let inputs = Inputs {
            home: PathBuf::from(&info.home),
            cwd: self.root.clone(),
            env,
            repo: Some(self.root.clone()),
            profile: self.repo.profile.clone().or_else(|| info.profile.clone()),
            vars: cli.to_vec(),
            env_files: Vec::new(),
        };
        let mut ws = Workspace::open(&inputs, |_| facts)
            .map_err(|e| anyhow!("planning the run on {} here: {e}", self.host))?;
        // What its local.env sets counts as set there.
        if !info.local_env.is_empty() {
            let mut layer = Layer::new(Source::new(
                Rank::Local,
                format!("local.env on {}", self.host),
            ));
            for k in &info.local_env {
                layer.set(k.clone(), serde_json::Value::String("(set there)".into()));
            }
            ws.vars.push_layer(layer);
        }
        let res = ws.resolve(&self.bundles, self.no_deps).map_err(|errs| {
            let lines: Vec<String> = errs.iter().map(ToString::to_string).collect();
            anyhow!(
                "planning the run on {} here: {}",
                self.host,
                lines.join("\n")
            )
        })?;
        let bundles = ws.bundles(&res);
        let mut decls: Vec<(String, VarDecl)> = Vec::new();
        let mut add = |v: &dot_core::Vars| {
            for (n, d) in v.declared() {
                if !decls.iter().any(|(m, _)| m == n) {
                    decls.push((n.to_string(), d.clone()));
                }
            }
        };
        add(&ws.vars);
        for b in &bundles {
            add(&ws.vars.with_bundle(b));
        }
        Ok(Planned {
            ws,
            res,
            bundles,
            decls,
        })
    }

    /// `persist: local` values from this machine's `local.env` (not per-machine ones, nor those
    /// the remote's `local.env` sets, unless `--force-secrets`), then every `--var`.
    fn forwarded(
        &self,
        p: &Planned,
        info: &HostInfo,
        cli: &[(String, String)],
    ) -> Result<Vec<SentVar>> {
        let here = self.here()?;
        let mut mine: BTreeMap<String, Zeroizing<String>> = BTreeMap::new();
        let local_env = here.local_env();
        if local_env.exists() {
            for item in dotenvy::from_path_iter(&local_env)
                .with_context(|| format!("cannot read {}", local_env.display()))?
            {
                let (k, v) = item.map_err(|_| {
                    anyhow!("{} is not valid dotenv (not shown)", local_env.display())
                })?;
                mine.insert(k, Zeroizing::new(v));
            }
        }
        let decl = |n: &str| p.decls.iter().find(|(m, _)| m == n).map(|(_, d)| d);
        let sensitive = |n: &str| {
            decl(n)
                .and_then(|d| d.sensitive)
                .unwrap_or_else(|| looks_sensitive(n))
        };
        let keeps = |n: &str| decl(n).is_some_and(|d| d.persist == Some(Persist::Local));
        let mut out = Vec::new();
        for (name, d) in &p.decls {
            if d.persist != Some(Persist::Local)
                || d.per_machine == Some(true)
                || cli.iter().any(|(k, _)| k == name)
                || (info.local_env.contains(name) && !self.force)
            {
                continue;
            }
            if let Some(v) = mine.get(name).filter(|v| !v.is_empty()) {
                out.push(SentVar {
                    name: name.clone(),
                    value: v.clone(),
                    persist: true,
                    keep: false,
                    sensitive: sensitive(name),
                });
            }
        }
        for (k, v) in cli {
            out.retain(|s: &SentVar| &s.name != k);
            out.push(SentVar {
                name: k.clone(),
                value: Zeroizing::new(v.clone()),
                persist: keeps(k),
                keep: false,
                sensitive: sensitive(k),
            });
        }
        Ok(out)
    }

    /// Per-machine variables the run there needs (asked for, or required) and has no value
    /// for, in declaration order.
    fn per_machine_missing(&self, p: &Planned, cli: &[(String, String)]) -> Vec<String> {
        let mut wanted: BTreeSet<String> =
            p.ws.var_prompts(&p.res)
                .into_iter()
                .map(|q| q.name)
                .collect();
        wanted.extend(p.ws.vars.missing_required());
        for b in &p.bundles {
            wanted.extend(p.ws.vars.with_bundle(b).missing_required());
        }
        p.decls
            .iter()
            .filter(|(n, d)| {
                d.per_machine == Some(true)
                    && wanted.contains(n)
                    && !cli.iter().any(|(k, _)| k == n)
            })
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Asks for each per-machine variable here.
    fn ask(&self, p: &Planned, names: &[String]) -> Result<Vec<SentVar>> {
        let mut asker = TermAsker(console::Term::stderr());
        let mut out = Vec::new();
        for name in names {
            let d = p
                .decls
                .iter()
                .find(|(m, _)| m == name)
                .map(|(_, d)| d.clone())
                .unwrap_or_default();
            let hidden = d.sensitive.unwrap_or_else(|| looks_sensitive(name));
            let about = d.prompt.as_deref().or(d.description.as_deref());
            let question = format!(
                "Value of {name} for {}{}{}: ",
                self.host,
                about
                    .map(|a| format!(" ({})", a.trim_end_matches([':', ' ', '?'])))
                    .unwrap_or_default(),
                if hidden { " (hidden)" } else { "" }
            );
            let mut attempt = 0;
            let value = loop {
                attempt += 1;
                let answer = asker.ask(&question, hidden)?;
                let answer = answer.trim_end_matches(['\r', '\n']).to_string();
                let problems = if answer.is_empty() {
                    vec![format!("`{name}` needs a value")]
                } else {
                    p.ws.answer_problems(&p.res, name, &answer)
                };
                if problems.is_empty() {
                    break answer;
                }
                for line in &problems {
                    asker.say(line);
                }
                if attempt >= 3 {
                    bail!("no usable value for variable `{name}`");
                }
            };
            // A `persist: local` answer is kept there, as a local run keeps it: the remote's
            // own value, asked for once.
            let keep = d.persist == Some(Persist::Local);
            out.push(SentVar {
                name: name.clone(),
                value: Zeroizing::new(value),
                persist: keep,
                keep,
                sensitive: hidden,
            });
        }
        Ok(out)
    }

    /// The secret names the remote's bundles use.
    fn used(&self, p: &Planned, config: Option<&SecretsConfig>) -> Vec<String> {
        let mut names: BTreeSet<String> = p
            .bundles
            .iter()
            .flat_map(dot_core::config::secret_uses)
            .map(|s| s.name)
            .collect();
        // `github_release` steps without `token:` use the declared GitHub token.
        let tokenless = p
            .bundles
            .iter()
            .flat_map(Bundle::all_steps)
            .any(|s| matches!(&s.directive, Directive::GithubRelease(g) if g.token.is_none()));
        if tokenless && config.is_some_and(|c| c.secrets.contains_key(TOKEN_SECRET)) {
            names.insert(TOKEN_SECRET.to_string());
        }
        names.into_iter().collect()
    }

    /// See the module documentation. `dry`: only says what it would send.
    pub fn prepare(&self, info: &HostInfo, dry: bool) -> Result<Prepared> {
        let cli = self.cli_vars()?;
        let (facts, unknown) = info.facts()?;
        if !unknown.is_empty() {
            self.say(format!(
                "ignoring fact{} this dot does not know: {} (dot {} there is newer than this one, \
                 {}; `dot self-update` updates this one)",
                plural(unknown.len()),
                unknown.join(", "),
                printable(&info.version),
                env!("CARGO_PKG_VERSION")
            ));
        }
        let p = self.plan_there(info, facts, &cli)?;
        let profile = p.ws.profile_name().map(str::to_string);
        let mut vars = self.forwarded(&p, info, &cli)?;
        let missing = if self.ask_per_machine {
            self.per_machine_missing(&p, &cli)
        } else {
            Vec::new()
        };

        let file = crate::secrets::secrets_file(&self.root);
        let there = if file.exists() {
            Some(
                SecretsConfig::load(&file)
                    .map_err(anyhow::Error::msg)?
                    .with_provider_override(p.ws.secrets_provider.as_deref()),
            )
        } else {
            None
        };
        let names: Vec<String> = match self.secrets {
            Which::None => Vec::new(),
            Which::All => match &there {
                Some(c) => c.secrets.keys().cloned().collect(),
                None => {
                    self.say(format!(
                        "no {} in the repository: no secrets to send",
                        crate::secrets::SECRETS_FILE
                    ));
                    Vec::new()
                }
            },
            Which::Needed => {
                let used = self.used(&p, there.as_ref());
                match &there {
                    _ if used.is_empty() => Vec::new(),
                    None => bail!(
                        "the bundles for {} use secret{} {}, but there is no {}",
                        self.host,
                        plural(used.len()),
                        used.join(", "),
                        crate::secrets::SECRETS_FILE
                    ),
                    Some(c) => {
                        let undeclared: Vec<&String> = used
                            .iter()
                            .filter(|n| !c.secrets.contains_key(*n))
                            .collect();
                        if !undeclared.is_empty() {
                            let shown: Vec<&str> = undeclared.iter().map(|s| s.as_str()).collect();
                            bail!(
                                "the bundles for {} use secret{} {} that {} does not declare",
                                self.host,
                                plural(shown.len()),
                                shown.join(", "),
                                crate::secrets::SECRETS_FILE
                            );
                        }
                        used
                    }
                }
            }
        };

        // The remote's own report (its pinned profile) chose what is sent: confirmed here.
        let reported = self.repo.profile.is_none() && info.profile.is_some();
        let private: Vec<&str> = vars
            .iter()
            .filter(|v| v.sensitive && !cli.iter().any(|(k, _)| *k == v.name))
            .map(|v| v.name.as_str())
            .collect();
        let confirm = reported && (!names.is_empty() || !private.is_empty());
        let chosen = || {
            let mut what = Vec::new();
            if !names.is_empty() {
                what.push(format!(
                    "{} secret{} ({})",
                    names.len(),
                    plural(names.len()),
                    names.join(", ")
                ));
            }
            if !private.is_empty() {
                what.push(format!(
                    "{} sensitive variable{} ({})",
                    private.len(),
                    plural(private.len()),
                    private.join(", ")
                ));
            }
            format!(
                "profile `{}` is the one {} reports (pinned there; its facts: {}), and it decides \
                 what is sent: {}",
                profile.as_deref().unwrap_or_default(),
                self.host,
                printable(&info.facts),
                what.join(" and ")
            )
        };
        let refusal = || {
            format!(
                "{}; without a terminal to confirm it on, give --profile {} to send them{}",
                chosen(),
                profile.as_deref().unwrap_or_default(),
                if names.is_empty() {
                    ""
                } else {
                    " (or --no-secrets)"
                }
            )
        };

        if dry {
            if confirm {
                self.say(chosen());
                self.say(if self.interactive {
                    "a real run asks here before sending them".to_string()
                } else {
                    format!(
                        "a real run would stop: no terminal to confirm it on (give --profile {})",
                        profile.as_deref().unwrap_or_default()
                    )
                });
            }
            if !names.is_empty() {
                self.say(format!(
                    "would resolve here and send {} secret{}: {} (nothing is resolved now)",
                    names.len(),
                    plural(names.len()),
                    names.join(", ")
                ));
            }
            if !vars.is_empty() {
                let shown: Vec<&str> = vars.iter().map(|v| v.name.as_str()).collect();
                self.say(format!(
                    "would forward {} variable{}: {}",
                    shown.len(),
                    plural(shown.len()),
                    shown.join(", ")
                ));
            }
            if !missing.is_empty() {
                let flags: Vec<String> = missing
                    .iter()
                    .map(|n| format!("--var {n}=<value>"))
                    .collect();
                self.say(format!(
                    "per-machine variable{} without a value there: {} ({})",
                    plural(missing.len()),
                    missing.join(", "),
                    if self.interactive {
                        format!("a real run asks for it here, or give {}", flags.join(" "))
                    } else {
                        format!(
                            "a real run would stop: no terminal to ask on; give {}",
                            flags.join(" ")
                        )
                    }
                ));
            }
            return Ok(Prepared {
                handoff: None,
                profile,
            });
        }

        if confirm {
            if !self.interactive {
                bail!("{}", refusal());
            }
            self.say(chosen());
            let mut prompter = LinePrompter::new(StdinLines, std::io::stderr());
            if !prompter.confirm(&format!("Send them to {}?", self.host), false)? {
                bail!("cancelled: nothing was resolved, synced or sent");
            }
        }

        if !missing.is_empty() {
            if !self.interactive {
                let flags: Vec<String> = missing
                    .iter()
                    .map(|n| format!("--var {n}=<value>"))
                    .collect();
                bail!(
                    "{} {} per-machine (`per_machine: true`) and {} no value on {}: give {} \
                     (this machine's own value is never copied there)",
                    missing.join(", "),
                    if missing.len() == 1 { "is" } else { "are" },
                    if missing.len() == 1 { "has" } else { "have" },
                    self.host,
                    flags.join(" "),
                );
            }
            vars.extend(self.ask(&p, &missing)?);
        }

        let mut h = Handoff {
            via: Via::Apply,
            host: Some(self.host.clone()),
            persist: self.persist,
            force: self.force,
            vars,
            ..Handoff::default()
        };
        if let Some(there) = &there {
            if let Some(e) = &there.providers.envfile {
                h.envfile_path = e.path.clone();
                h.files_dir = e.files_dir.clone();
            }
            if !names.is_empty() {
                let (secrets, notes) = resolve_here(
                    &self.here()?,
                    &file,
                    &names,
                    self.secrets == Which::All,
                    self.interactive,
                    there,
                    &self.host,
                    Purpose::Run,
                )?;
                for n in notes {
                    self.say(n);
                }
                h.secrets = secrets;
            }
        }
        if self.persist {
            let unkept: Vec<&str> = h
                .secrets
                .iter()
                .filter(|s| s.target.is_none())
                .map(|s| s.name.as_str())
                .collect();
            if !unkept.is_empty() {
                self.say(format!(
                    "{}: no envfile ref in {}, so kept for that run only",
                    unkept.join(", "),
                    crate::secrets::SECRETS_FILE
                ));
            }
        }
        if h.is_empty() {
            // When something was wanted but did not resolve, the notes above said so.
            if self.secrets != Which::None && names.is_empty() {
                self.say("nothing to send: the run there needs no secret or variable from here");
            }
            return Ok(Prepared {
                handoff: None,
                profile,
            });
        }
        self.say(format!(
            "sending {} on ssh's stdin ({})",
            h.describe(),
            if self.persist {
                "to keep there: --persist-secrets"
            } else {
                "for that run only"
            }
        ));
        Ok(Prepared {
            handoff: Some(h),
            profile,
        })
    }
}

/// The terminal, for a provider's unlock prompt (`lpass login`) and for a missing secret.
struct Term {
    interactive: bool,
}

impl Interactive for Term {
    fn is_interactive(&self) -> bool {
        self.interactive
    }

    fn read_secret(&mut self, prompt: &str) -> Result<SecretValue> {
        if !self.interactive {
            bail!("cannot ask for {prompt}: running non-interactively");
        }
        let term = console::Term::stderr();
        term.write_str(prompt)?;
        Ok(SecretValue::from(term.read_secure_line()?))
    }
}

/// The first envfile ref of `name` under `config` (its `ref`, else its `fallback`): where the
/// receiving machine keeps it.
pub(crate) fn envfile_target(config: &SecretsConfig, name: &str) -> Option<SecretRef> {
    let entry = config.entry(name)?.ok()?;
    std::iter::once(entry.reference)
        .chain(entry.fallback)
        .find(|r| r.provider() == "envfile")
}

/// Resolves `names` here, through this machine's providers (its profile's `secrets_provider`
/// for bare refs); `there` is the receiving machine's view of `secrets.yml` (its kinds,
/// `optional:` and where it keeps each). `lenient`: a value that is not there is skipped
/// (`--all-secrets`, `dot secrets push` with no names); otherwise a required one stops
/// everything, listing every such name, as does any failure that is not a missing value (a
/// provider that fails, a secrets file others can read). Returns the values and notes (names
/// only).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_here(
    here: &Inputs,
    file: &Path,
    names: &[String],
    lenient: bool,
    interactive: bool,
    there: &SecretsConfig,
    host: &str,
    purpose: Purpose,
) -> Result<(Vec<SentSecret>, Vec<String>)> {
    let provider = Workspace::open(here, crate::commands::detect)
        .ok()
        .and_then(|w| w.secrets_provider);
    let config = SecretsConfig::load(file)
        .map_err(anyhow::Error::msg)?
        .with_provider_override(provider.as_deref());
    let mut resolver = Resolver::new(
        Registry::builtin(&crate::secrets::provider_env(here)),
        config,
    );
    let mut tty = Term { interactive };
    let partial = resolver.resolve_partial(names, &mut tty);
    let mut notes = Vec::new();
    let mut skipped = Vec::new();
    let mut hard = Vec::new();
    for f in partial.failed.iter().chain(&partial.deferred) {
        let optional = there.secrets.get(&f.name).is_some_and(|s| s.optional);
        // A value that is simply not there may be skipped; a provider that fails, a bad ref or
        // a secrets file others can read is fixed rather than worked around.
        if lenient && f.is_missing() {
            skipped.push(format!("{} ({})", f.name, f.reason()));
        } else if optional && f.is_missing() {
            notes.push(format!(
                "optional secret {} is not available here ({}): not sent",
                f.name,
                f.reason()
            ));
        } else {
            hard.push(format!("  {}: {}", f.name, f.reason()));
        }
    }
    if !hard.is_empty() {
        match purpose {
            Purpose::Run => bail!(
                "{} secret{} the run on {host} needs could not be resolved here, so nothing was \
                 sent:\n{}\n(--no-secrets lets the remote resolve its own)",
                hard.len(),
                plural(hard.len()),
                hard.join("\n")
            ),
            Purpose::Push => bail!(
                "{} secret{} could not be resolved here, so nothing was pushed to {host}:\n{}",
                hard.len(),
                plural(hard.len()),
                hard.join("\n")
            ),
        }
    }
    if !skipped.is_empty() {
        notes.push(format!(
            "not sent (not resolved here): {}",
            skipped.join(", ")
        ));
    }
    let mut out = Vec::new();
    for (name, value) in partial.resolved.iter() {
        let file = there
            .secrets
            .get(name)
            .is_some_and(|s| s.kind == dot_secrets::SecretKind::File);
        out.push(SentSecret {
            name: name.to_string(),
            file,
            target: envfile_target(there, name).map(|r| r.to_string()),
            value: value.clone(),
        });
    }
    Ok((out, notes))
}
