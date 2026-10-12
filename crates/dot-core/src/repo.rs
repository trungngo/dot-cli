//! The repository: `dot.yml` at its root (profiles, bundle selection, repo-wide vars and
//! defaults, the package name map) and the bundles under `bundles_dir`.
//!
//! Parsing is as strict as for bundles: unknown keys and bad values are errors with file and
//! line.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::shapes::OrderedMap;
use crate::config::{Bundle, ConfigError, Defaults, DefaultsFile, When};
use crate::facts::Facts;
use crate::graph::{self, CAP_PREFIX};
use crate::vars::{Layer, Source, VarSpec, Vars};

/// The name of the repository's config file.
pub const REPO_FILE: &str = "dot.yml";

/// Package managers a package name map may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PackageManagerName {
    Brew,
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Apk,
    Winget,
    Scoop,
}

impl PackageManagerName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Brew => "brew",
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Pacman => "pacman",
            Self::Zypper => "zypper",
            Self::Apk => "apk",
            Self::Winget => "winget",
            Self::Scoop => "scoop",
        }
    }
}

/// `dot.yml`, at the root of a dotfiles repository: profiles, the bundle selection,
/// repository-wide variables and defaults, and package names that differ per manager.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "dot repository (dot.yml)")]
pub struct RepoFile {
    /// Directory holding the bundles, relative to the repository root (default: `bundles`).
    pub bundles_dir: Option<String>,
    /// Bundles applied when neither the command line nor the profile names any (default: all).
    pub bundles: Option<Vec<String>>,
    /// Repository-wide variables: values, or declaration mappings.
    #[serde(default)]
    pub vars: OrderedMap<VarSpec>,
    /// Option defaults for every bundle.
    pub defaults: Option<DefaultsFile>,
    /// Package names that differ per manager: `fd: { apt: fd-find, brew: fd }`.
    #[serde(default)]
    pub packages: BTreeMap<String, BTreeMap<PackageManagerName, String>>,
    /// Default secret provider for profiles that do not set one.
    pub secrets_provider: Option<String>,
    /// Capability -> bundle providing it, for every profile.
    #[serde(default)]
    pub providers: BTreeMap<String, String>,
    /// Named fact sets with their bundle selection, vars and defaults; auto-detected or pinned.
    #[serde(default)]
    pub profiles: OrderedMap<ProfileFile>,
}

/// A kind of machine: which facts it matches, which bundles it applies, and its variables and
/// defaults.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileFile {
    /// What the profile is for.
    pub description: Option<String>,
    /// Facts this profile is for. A profile without `match` is only used when named with
    /// `--profile` or pinned with `DOT_PROFILE`; `match: {}` matches every machine.
    #[serde(rename = "match")]
    pub matches: Option<When>,
    /// Bundles to apply (default: `dot.yml`'s `bundles`).
    pub bundles: Option<Vec<String>>,
    /// Variables for this profile (over `dot.yml`'s): values, or declaration mappings.
    #[serde(default)]
    pub vars: OrderedMap<VarSpec>,
    /// Option defaults for this profile's bundles (over `dot.yml`'s).
    pub defaults: Option<DefaultsFile>,
    /// Secret provider for this profile, e.g. `lastpass` or `envfile`.
    pub secrets_provider: Option<String>,
    /// Capability -> bundle providing it (over `dot.yml`'s `providers`).
    #[serde(default)]
    pub providers: BTreeMap<String, String>,
}

/// A validated profile.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub description: Option<String>,
    pub matches: Option<When>,
    pub bundles: Option<Vec<String>>,
    pub vars: Layer,
    pub defaults: Defaults,
    pub secrets_provider: Option<String>,
    pub providers: BTreeMap<String, String>,
    /// Line of the profile's key in `dot.yml`, when found.
    pub line: Option<usize>,
}

/// A validated `dot.yml`.
#[derive(Debug, Clone)]
pub struct RepoConfig {
    /// Absolute path of `dot.yml`.
    pub file: PathBuf,
    pub root: PathBuf,
    pub bundles_dir: PathBuf,
    pub bundles: Option<Vec<String>>,
    pub vars: Layer,
    pub defaults: Defaults,
    /// Package name -> manager -> name for that manager (used by `package`).
    pub packages: BTreeMap<String, BTreeMap<String, String>>,
    pub secrets_provider: Option<String>,
    pub providers: BTreeMap<String, String>,
    pub profiles: Vec<Profile>,
    text: String,
}

impl RepoConfig {
    pub fn profile(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.name == name)
    }

    /// A config error located at the first line of `dot.yml` that starts with `key:` (nested
    /// keys may be given as `parent.key`).
    pub fn error_at(&self, key: &str, message: impl Into<String>) -> ConfigError {
        let mut e = ConfigError::new(&self.file, message);
        e.line = key_line(&self.text, key);
        e
    }
}

/// The line of a (possibly nested, `a.b`) mapping key, found by indentation. Good enough to
/// point at the right place in a hand-written file; `None` if not found.
pub fn key_line(text: &str, path: &str) -> Option<usize> {
    let mut indent: Option<usize> = None;
    let mut from = 0;
    let lines: Vec<&str> = text.lines().collect();
    for part in path.split('.') {
        let mut found = None;
        for (i, line) in lines.iter().enumerate().skip(from) {
            let trimmed = line.trim_start();
            let this_indent = line.len() - trimmed.len();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(parent) = indent
                && i > from
                && this_indent <= parent
            {
                break;
            }
            let is_key = [
                format!("{part}:"),
                format!("\"{part}\":"),
                format!("'{part}':"),
            ]
            .iter()
            .any(|k| trimmed.starts_with(k.as_str()));
            if is_key && indent.is_none_or(|p| this_indent > p) {
                found = Some((i, this_indent));
                break;
            }
        }
        let (i, ind) = found?;
        indent = Some(ind);
        from = i;
    }
    Some(from + 1)
}

/// Parses `dot.yml` text read from `file` (absolute). An empty file is an empty config.
pub fn parse_repo(text: &str, file: &Path) -> Result<RepoConfig, ConfigError> {
    let raw: RepoFile = if is_blank_yaml(text) {
        RepoFile::default()
    } else {
        serde_yaml_ng::from_str(text).map_err(|e| ConfigError::from_yaml(file, e))?
    };
    convert(raw, text, file)
}

fn is_blank_yaml(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .all(|l| l.is_empty() || l.starts_with('#') || l == "---")
}

fn convert(raw: RepoFile, text: &str, file: &Path) -> Result<RepoConfig, ConfigError> {
    let root = file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let bundles_dir = match raw.bundles_dir.as_deref() {
        Some(d) if d.trim().is_empty() => {
            let mut e = ConfigError::new(file, "`bundles_dir` is empty");
            e.line = key_line(text, "bundles_dir");
            return Err(e);
        }
        Some(d) => root.join(d),
        None => root.join("bundles"),
    };
    let profiles = raw
        .profiles
        .0
        .iter()
        .map(|(name, p)| Profile {
            name: name.clone(),
            description: p.description.clone(),
            matches: p.matches.clone(),
            bundles: p.bundles.clone(),
            vars: Layer::from_specs(Source::profile(name), p.vars.0.iter().map(|(k, v)| (k, v))),
            defaults: p
                .defaults
                .as_ref()
                .map(DefaultsFile::to_defaults)
                .unwrap_or_default(),
            secrets_provider: p.secrets_provider.clone(),
            providers: p.providers.clone(),
            line: key_line(text, &format!("profiles.{name}")),
        })
        .collect();
    Ok(RepoConfig {
        file: file.to_path_buf(),
        root,
        bundles_dir,
        bundles: raw.bundles,
        vars: Layer::from_specs(Source::repo(), raw.vars.0.iter().map(|(k, v)| (k, v))),
        defaults: raw
            .defaults
            .as_ref()
            .map(DefaultsFile::to_defaults)
            .unwrap_or_default(),
        packages: raw
            .packages
            .into_iter()
            .map(|(k, m)| {
                (
                    k,
                    m.into_iter()
                        .map(|(pm, n)| (pm.as_str().to_string(), n))
                        .collect(),
                )
            })
            .collect(),
        secrets_provider: raw.secrets_provider,
        providers: raw.providers,
        profiles,
        text: text.to_string(),
    })
}

/// Reads `dot.yml` under `root`, reporting every problem it can find: the parser's error plus
/// each other profile that fails on its own (located at the profile's key).
pub fn load_repo_config(root: &Path) -> Result<RepoConfig, Vec<ConfigError>> {
    let file = std::path::absolute(root.join(REPO_FILE)).unwrap_or_else(|_| root.join(REPO_FILE));
    let text = std::fs::read_to_string(&file)
        .map_err(|e| vec![ConfigError::new(&file, format!("cannot read: {e}"))])?;
    let first = match parse_repo(&text, &file) {
        Ok(c) => return Ok(c),
        Err(e) => e,
    };
    let mut errors = vec![first.clone()];
    let Ok(serde_yaml_ng::Value::Mapping(mut map)) =
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text)
    else {
        return Err(errors);
    };
    if let Some(serde_yaml_ng::Value::Mapping(profiles)) = map.remove("profiles") {
        for (k, v) in profiles {
            let Some(name) = k.as_str() else { continue };
            let line = key_line(&text, &format!("profiles.{name}"));
            if let Err(e) = serde_yaml_ng::from_value::<ProfileFile>(v) {
                // The parser's own error already covers the profile it stopped in.
                let same = first.line.is_some_and(|l| {
                    line.is_some_and(|pl| l >= pl) && next_profile_line(&text, line) > l
                });
                if !same {
                    let mut err = ConfigError::new(&file, format!("profiles.{name}: {e}"));
                    err.line = line;
                    errors.push(err);
                }
            }
        }
    }
    Err(errors)
}

/// The line after the profile starting at `line` ends (a crude upper bound: the next line with
/// the same or less indentation).
fn next_profile_line(text: &str, line: Option<usize>) -> usize {
    let Some(start) = line else { return 0 };
    let lines: Vec<&str> = text.lines().collect();
    let indent = |l: &str| l.len() - l.trim_start().len();
    let base = lines.get(start - 1).map(|l| indent(l)).unwrap_or(0);
    lines
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, l)| {
            !l.trim().is_empty() && !l.trim_start().starts_with('#') && indent(l) <= base
        })
        .map(|(i, _)| i + 1)
        .unwrap_or(usize::MAX)
}

/// The JSON Schema for `dot.yml`.
pub fn json_schema() -> schemars::Schema {
    schemars::schema_for!(RepoFile)
}

/// A loaded repository: `dot.yml` and every bundle, by name.
#[derive(Debug, Clone)]
pub struct Repo {
    pub config: RepoConfig,
    pub bundles: BTreeMap<String, Bundle>,
}

impl Repo {
    /// Loads `dot.yml` and every `bundle.yml` under `bundles_dir`. Reports every file's errors.
    pub fn load(root: &Path) -> Result<Repo, Vec<ConfigError>> {
        let mut errors = Vec::new();
        let config = match load_repo_config(root) {
            Ok(c) => Some(c),
            Err(e) => {
                errors.extend(e);
                None
            }
        };
        let dir = config
            .as_ref()
            .map(|c| c.bundles_dir.clone())
            .unwrap_or_else(|| root.join("bundles"));
        let mut bundles: BTreeMap<String, Bundle> = BTreeMap::new();
        let files = if dir.is_dir() {
            crate::config::find_bundle_files(&dir)
                .map_err(|e| vec![ConfigError::new(&dir, format!("cannot list bundles: {e}"))])?
        } else {
            Vec::new()
        };
        for f in files {
            match crate::config::load_bundle_all_errors(&f) {
                Ok(b) => {
                    if let Some(other) = bundles.get(&b.name) {
                        errors.push(ConfigError::new(
                            &b.file,
                            format!(
                                "bundle name `{}` is already used by {}",
                                b.name,
                                other.file.display()
                            ),
                        ));
                    } else {
                        bundles.insert(b.name.clone(), b);
                    }
                }
                Err(e) => errors.extend(e),
            }
        }
        match config {
            Some(config) if errors.is_empty() => Ok(Repo { config, bundles }),
            _ => Err(errors),
        }
    }

    /// Host-independent validation for `dot check`: every reference in `dot.yml` and in the
    /// bundles resolves, no cycles, link sources exist.
    pub fn check(&self) -> Vec<ConfigError> {
        let c = &self.config;
        let mut out = Vec::new();
        let unknown = |name: &str| !self.bundles.contains_key(name);
        for b in self.bundles.values() {
            if b.name.contains(':') || b.name.trim().is_empty() {
                out.push(ConfigError::new(
                    &b.file,
                    format!("invalid bundle name `{}`: it must not contain `:`", b.name),
                ));
            }
            out.extend(crate::config::lint(b));
        }
        for n in c.bundles.iter().flatten().filter(|n| unknown(n)) {
            out.push(c.error_at("bundles", format!("bundles: unknown bundle `{n}`")));
        }
        let check_providers =
            |key: &str, providers: &BTreeMap<String, String>, out: &mut Vec<ConfigError>| {
                for (cap, bundle) in providers {
                    let msg = match self.bundles.get(bundle) {
                        None => format!("providers: `{cap}`: unknown bundle `{bundle}`"),
                        Some(b) if !b.manifest.provides.contains(cap) => {
                            format!("providers: bundle `{bundle}` does not provide `{cap}`")
                        }
                        Some(_) => continue,
                    };
                    out.push(c.error_at(key, msg));
                }
            };
        check_providers("providers", &c.providers, &mut out);
        for p in &c.profiles {
            let at = |msg: String| {
                let mut e = ConfigError::new(&c.file, format!("profiles.{}: {msg}", p.name));
                e.line = p.line;
                e
            };
            for n in p.bundles.iter().flatten().filter(|n| unknown(n)) {
                out.push(at(format!("bundles: unknown bundle `{n}`")));
            }
            let mut sub = Vec::new();
            check_providers(
                &format!("profiles.{}.providers", p.name),
                &p.providers,
                &mut sub,
            );
            out.extend(sub.into_iter().map(|mut e| {
                e.message = format!("profiles.{}: {}", p.name, e.message);
                e
            }));
        }
        for p in graph::check_all(&self.bundles) {
            let file = p
                .bundle
                .as_ref()
                .and_then(|b| self.bundles.get(b))
                .map(|b| b.file.clone())
                .unwrap_or_else(|| c.file.clone());
            let mut err = ConfigError::new(&file, p.error.to_string());
            if let Some(key) = p.key
                && let Ok(text) = std::fs::read_to_string(&file)
            {
                err.line = key_line(&text, key);
            }
            out.push(err);
        }
        out
    }

    /// Chooses the profile: `requested` (`--profile`), else `pinned` (`DOT_PROFILE` in local
    /// config), else the most specific profile whose `match:` fits `facts`. `Ok(None)` when
    /// the repo has no profiles.
    pub fn select_profile(
        &self,
        facts: &Facts,
        vars: &Vars,
        requested: Option<&str>,
        pinned: Option<(&str, &str)>,
    ) -> Result<Option<ProfileChoice<'_>>, String> {
        let profiles = &self.config.profiles;
        let names = || {
            profiles
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let named = |name: &str, how: Cow<'static, str>| {
            self.config
                .profile(name)
                .map(|p| ProfileChoice {
                    profile: p,
                    how: how.into_owned(),
                })
                .ok_or_else(|| match profiles.is_empty() {
                    true => format!("unknown profile `{name}`: dot.yml defines no profiles"),
                    false => format!("unknown profile `{name}` (profiles: {})", names()),
                })
        };
        if let Some(name) = requested {
            return named(name, "--profile".into()).map(Some);
        }
        if let Some((name, source)) = pinned {
            return named(name, format!("pinned in {source}").into()).map(Some);
        }
        if profiles.is_empty() {
            return Ok(None);
        }
        let mut matching: Vec<(&Profile, usize)> = Vec::new();
        let mut reasons = Vec::new();
        for p in profiles {
            let Some(m) = &p.matches else {
                reasons.push(format!("{}: no `match`", p.name));
                continue;
            };
            let rendered = m
                .render(vars)
                .map_err(|e| format!("profiles.{}: match: {e}", p.name))?;
            match rendered.eval(facts) {
                Ok(()) => matching.push((p, m.0.len())),
                Err(r) => reasons.push(format!("{}: {r}", p.name)),
            }
        }
        let Some(best) = matching.iter().map(|(_, n)| *n).max() else {
            return Err(format!(
                "no profile matches this machine ({}); choose one with --profile or pin it with \
                 DOT_PROFILE in ~/.config/dot/local.env",
                reasons.join("; ")
            ));
        };
        let top: Vec<&Profile> = matching
            .iter()
            .filter(|(_, n)| *n == best)
            .map(|(p, _)| *p)
            .collect();
        if top.len() > 1 {
            return Err(format!(
                "several profiles match this machine equally well ({}); choose one with \
                 --profile or pin it with DOT_PROFILE in ~/.config/dot/local.env",
                top.iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let p = top[0];
        let how = match &p.matches {
            Some(m) if !m.0.is_empty() => format!(
                "matched {}",
                m.0.iter()
                    .map(|(f, v)| format!("{f}={}", v.join("|")))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            _ => "matched (catch-all)".to_string(),
        };
        Ok(Some(ProfileChoice { profile: p, how }))
    }

    /// Capability providers for a profile: the profile's over `dot.yml`'s.
    pub fn providers(&self, profile: Option<&Profile>) -> BTreeMap<String, String> {
        let mut out = self.config.providers.clone();
        if let Some(p) = profile {
            out.extend(p.providers.clone());
        }
        out
    }

    /// Whether `name` looks like a capability reference.
    pub fn is_cap(name: &str) -> bool {
        name.starts_with(CAP_PREFIX)
    }
}

/// The selected profile and how it was chosen (`--profile`, `pinned in ...`, `matched ...`).
#[derive(Debug, Clone)]
pub struct ProfileChoice<'a> {
    pub profile: &'a Profile,
    pub how: String,
}

/// Where the repository is and how it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub root: PathBuf,
    pub how: String,
}

/// Where the installers clone a dotfiles repository, and the last place [`find_repo`] looks.
/// Not `~/.dotfiles`: that is where other tools (dotbot, a hand-made setup) keep theirs, so a
/// machine can run such a setup and `dot` side by side, and `dot` never picks theirs up.
pub const DEFAULT_REPO_DIR: &str = "~/.dot";

/// Finds the repository: `flag` (`--repo`) > `env_repo` (`DOT_REPO`) > the nearest ancestor of
/// `cwd` holding `dot.yml` > `local_repo` (`DOT_REPO` in `~/.config/dot/local.env`) >
/// [`DEFAULT_REPO_DIR`] when it holds `dot.yml`.
pub fn find_repo(
    flag: Option<&Path>,
    env_repo: Option<&str>,
    cwd: &Path,
    local_repo: Option<&str>,
    home: &Path,
) -> Result<Located, String> {
    let check = |p: PathBuf, how: &str| -> Result<Located, String> {
        let p = if p.is_absolute() { p } else { cwd.join(p) };
        // As users see it, found or not: the ordinary spelling of a verbatim path.
        let shown = crate::paths::plain(crate::paths::normalize(&p));
        if p.join(REPO_FILE).is_file() {
            Ok(Located {
                root: shown,
                how: how.to_string(),
            })
        } else {
            Err(format!("{how}: {} has no {REPO_FILE}", shown.display()))
        }
    };
    let expand = |s: &str| crate::paths::expand_tilde(s, home);
    if let Some(p) = flag {
        return check(expand(&p.to_string_lossy()), "--repo");
    }
    if let Some(p) = env_repo.filter(|s| !s.is_empty()) {
        return check(expand(p), "DOT_REPO");
    }
    if let Some(dir) = cwd.ancestors().find(|d| d.join(REPO_FILE).is_file()) {
        return Ok(Located {
            root: dir.to_path_buf(),
            how: "current directory".into(),
        });
    }
    if let Some(p) = local_repo.filter(|s| !s.is_empty()) {
        return check(expand(p), "DOT_REPO in local.env");
    }
    let default = expand(DEFAULT_REPO_DIR);
    if default.join(REPO_FILE).is_file() {
        return check(default, DEFAULT_REPO_DIR);
    }
    Err(format!(
        "no dotfiles repository found: pass --repo, set DOT_REPO, run inside a repository \
         (a directory with {REPO_FILE}), set DOT_REPO in ~/.config/dot/local.env, or put it in \
         {DEFAULT_REPO_DIR}; to start one, copy \
         https://github.com/trungngo/dot-cli/tree/main/examples/basic"
    ))
}

#[cfg(test)]
mod tests;
