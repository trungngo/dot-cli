//! System directives: `package_repo` (a vendor's package repository, so its packages can be
//! installed with `package:`), `user` (the login shell and supplementary groups) and `absent`
//! (a path that must not exist). Their on-disk shapes (which also generate the JSON Schema), the
//! checks run while parsing, and the validated model the planners read. [`super::raw`] and
//! [`super`] only hook these in.
//!
//! ```yaml
//! - package_repo:
//!     name: docker
//!     apt:
//!       url: https://download.docker.com/linux/ubuntu
//!       key: https://download.docker.com/linux/ubuntu/gpg
//!       fingerprint: 9DC8 5822 9FC7 DD38 854A  E2D8 8D81 803C 0EBF CD88
//!       suites: "{{ codename }}"
//!       fallback_suite: noble          # when the vendor has nothing for this release yet
//!       components: [stable]
//!     dnf: { baseurl: "https://download.docker.com/linux/fedora/$releasever/$basearch/stable",
//!            gpgkey: https://download.docker.com/linux/fedora/gpg }
//! - package_repo: { name: hashicorp, brew: hashicorp/tap }
//! - user: { shell: zsh, groups: [docker, libvirt] }
//! - absent:
//!     ~/.old-tool:
//!     /etc/apt/sources.list.d/docker.list: { root: true }
//! ```

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::Deserialize;

use super::Directive;
use super::raw::{StepFields, TARGET_PATTERN, is_target_path};
use super::shapes::{Check, Checked, FromScalar, ListOrMap, OneOrMany, ScalarOr};
use crate::template::is_template;

// ---------------------------------------------------------------------------------------------
// On-disk shapes

/// `package_repo:` — a vendor package repository for this machine's package manager. Only the
/// entry for the manager this machine uses is applied, so one step can cover several platforms.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PackageRepoOpts {
    /// Names the repository's files (`/etc/apt/sources.list.d/<name>.sources`,
    /// `/etc/apt/keyrings/<name>.gpg`, `/etc/yum.repos.d/<name>.repo`) and its zypper alias:
    /// letters, digits, `.`, `_` and `-`.
    pub name: String,
    /// apt (Debian, Ubuntu): a deb822 `.sources` file with `Signed-By:` its own keyring.
    pub apt: Option<AptRepo>,
    /// dnf / yum (Fedora, RHEL): a `.repo` file in `/etc/yum.repos.d`.
    #[serde(alias = "yum")]
    pub dnf: Option<DnfRepo>,
    /// zypper (openSUSE): `zypper addrepo`, the signing key imported with `rpm --import`.
    pub zypper: Option<ZypperRepo>,
    /// Homebrew: a tap (`owner/repo`), trusted unless `trust: false`.
    pub brew: Option<ScalarOr<BrewTap>>,
    /// scoop: a bucket (`extras`, or a name with `url`).
    pub scoop: Option<ScalarOr<ScoopBucket>>,
    /// winget: a source (needs an elevated terminal).
    pub winget: Option<WingetSource>,
}

/// An apt repository.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AptRepo {
    /// Base URL (`URIs:`), e.g. `https://download.docker.com/linux/ubuntu`.
    pub url: String,
    /// URL of the signing key (ASCII-armored or binary); stored dearmored in `keyring`.
    pub key: Option<String>,
    /// The key's expected fingerprint (spaces allowed), or a list of them for a keyring that
    /// holds several primary keys (a vendor rotating its key): a downloaded key file holding a
    /// key that is not pinned is refused before anything is installed.
    pub fingerprint: Option<OneOrMany>,
    /// Suite(s) (`Suites:`), usually `"{{ codename }}"`, or a fixed one (`stable`).
    pub suites: OneOrMany,
    /// Used instead of the (single) suite when the repository does not publish it
    /// (`<url>/dists/<suite>/Release` is not found): a vendor that lags behind new releases.
    pub fallback_suite: Option<String>,
    /// Components (default: `[main]`); `[]` for a flat repository.
    pub components: Option<Vec<String>>,
    /// Architectures (default: this machine's, e.g. `amd64`); `[]` leaves them out.
    pub architectures: Option<Vec<String>>,
    /// Where the key goes (default: `/etc/apt/keyrings/<name>.gpg`).
    pub keyring: Option<String>,
    /// The sources file (default: `/etc/apt/sources.list.d/<name>.sources`).
    pub file: Option<String>,
    /// Only write the sources file when it does not exist: for packages that maintain their own
    /// entry once installed (VS Code). Default: false (kept exactly as configured).
    #[serde(default)]
    pub if_missing: bool,
    /// Other sources files for the same repository to remove when the source is written (e.g.
    /// `/etc/apt/sources.list.d/docker.list` from the vendor's instructions): apt refuses to
    /// read any source when two entries for one repository name different `Signed-By` keys.
    #[serde(default)]
    pub replaces: Vec<String>,
}

/// A dnf / yum repository.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DnfRepo {
    /// `baseurl=` (dnf expands `$releasever` and `$basearch` itself).
    pub baseurl: String,
    /// `gpgkey=`: the signing key's URL; packages are checked against it (`gpgcheck=1`).
    pub gpgkey: Option<String>,
    /// `name=` (default: the step's `name`).
    pub description: Option<String>,
    /// The `.repo` file (default: `/etc/yum.repos.d/<name>.repo`).
    pub file: Option<String>,
    /// Only write the file when it does not exist (default: false).
    #[serde(default)]
    pub if_missing: bool,
}

/// A zypper repository.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ZypperRepo {
    /// The repository's base URL.
    pub url: String,
    /// The signing key's URL, imported with `rpm --import` before the repository is added.
    pub gpgkey: Option<String>,
}

/// A Homebrew tap.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrewTap {
    /// `owner/repo`.
    pub tap: String,
    /// Clone URL, when it is not `https://github.com/<owner>/homebrew-<repo>`.
    pub url: Option<String>,
    /// `brew trust --tap` it, so Homebrew loads its formulae and casks (default: true; only
    /// where Homebrew has `brew trust`).
    pub trust: Option<bool>,
}

impl FromScalar for BrewTap {
    fn from_scalar(tap: String) -> Self {
        Self {
            tap,
            url: None,
            trust: None,
        }
    }
}

/// A scoop bucket.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScoopBucket {
    /// A known bucket (`extras`, `nerd-fonts`) or any name with `url`.
    pub bucket: String,
    /// Its git URL, for a bucket scoop does not know.
    pub url: Option<String>,
}

impl FromScalar for ScoopBucket {
    fn from_scalar(bucket: String) -> Self {
        Self { bucket, url: None }
    }
}

/// A winget source.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WingetSource {
    /// The source's name.
    pub source: String,
    /// The source's URL.
    pub url: String,
    /// `--type` (e.g. `Microsoft.Rest`); winget's default when left out.
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

/// A signing key's URL: `https://` (the key is what every package is trusted by, so it must
/// not travel in the clear), or also `file://` where the tool reads it itself (`allow_file`: dnf
/// and zypper keys shipped on the system). Templated values are checked when rendered.
pub fn check_key_url(what: &str, v: &str, allow_file: bool) -> Result<(), String> {
    if is_template(v) || v.starts_with("https://") || (allow_file && v.starts_with("file://")) {
        return Ok(());
    }
    Err(format!(
        "{what} `{v}` must be an https:// URL{}: a key fetched in the clear can be swapped in \
         transit",
        if allow_file { " (or file://)" } else { "" }
    ))
}

/// A pinned key's fingerprint: the whole of it (40 hexadecimal digits, or 64 for v5 / v6 keys),
/// spaces allowed. A key id does not pin a key.
pub fn check_fingerprint(v: &str) -> Result<(), String> {
    if is_template(v) {
        return Ok(());
    }
    let digits: String = v.chars().filter(|c| !c.is_whitespace()).collect();
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "package_repo: apt fingerprint `{v}` is not a hexadecimal fingerprint"
        ));
    }
    if !matches!(digits.len(), 40 | 64) {
        return Err(format!(
            "package_repo: apt fingerprint `{v}` must be the whole fingerprint (40 or 64 \
             hexadecimal digits), not a key id"
        ));
    }
    Ok(())
}

/// apt picks a file's format by its extension: a keyring is `.gpg` (binary) or `.asc`
/// (armored), a sources file `.sources` (deb822) or `.list` (one-line); anything else apt
/// ignores or misreads. Templated values are checked when rendered.
pub fn check_apt_extension(what: &str, path: &str, allowed: &[&str]) -> Result<(), String> {
    if is_template(path) || allowed.iter().any(|e| path.ends_with(e)) {
        return Ok(());
    }
    Err(format!(
        "package_repo: apt {what} `{path}` must end in {}: apt reads the format from the \
         extension",
        allowed
            .iter()
            .map(|e| format!("`{e}`"))
            .collect::<Vec<_>>()
            .join(" or ")
    ))
}

fn check_word(what: &str, v: &str) -> Result<(), String> {
    if is_template(v) {
        return Ok(());
    }
    if v.trim().is_empty() || v.chars().any(char::is_whitespace) {
        return Err(format!("{what} `{v}` must be one word"));
    }
    Ok(())
}

impl Check for PackageRepoOpts {
    fn check(&self) -> Result<(), String> {
        let n = &self.name;
        let ok = !n.is_empty()
            && !n.starts_with('.')
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !ok {
            return Err(format!(
                "package_repo: name `{n}` may only use letters, digits, `.`, `_` and `-`"
            ));
        }
        if self.apt.is_none()
            && self.dnf.is_none()
            && self.zypper.is_none()
            && self.brew.is_none()
            && self.scoop.is_none()
            && self.winget.is_none()
        {
            return Err(
                "package_repo: expected at least one of apt, dnf, zypper, brew, scoop, winget"
                    .into(),
            );
        }
        if let Some(a) = &self.apt {
            check_word("package_repo: apt url", &a.url)?;
            if a.fingerprint.is_some() && a.key.is_none() {
                return Err("package_repo: apt `fingerprint` needs `key`".into());
            }
            if let Some(f) = &a.fingerprint {
                for f in &f.0 {
                    check_fingerprint(f)?;
                }
            }
            if let Some(k) = &a.key {
                check_key_url("package_repo: apt key", k, false)?;
            }
            for s in &a.suites.0 {
                check_word("package_repo: apt suite", s)?;
            }
            if let Some(f) = &a.fallback_suite {
                check_word("package_repo: apt fallback_suite", f)?;
                if a.suites.0.len() != 1 {
                    return Err("package_repo: apt `fallback_suite` needs exactly one suite".into());
                }
            }
            for c in a.components.iter().flatten() {
                check_word("package_repo: apt component", c)?;
            }
            for c in a.architectures.iter().flatten() {
                check_word("package_repo: apt architecture", c)?;
            }
            for p in [&a.keyring, &a.file]
                .into_iter()
                .flatten()
                .chain(&a.replaces)
            {
                if !is_template(p) && !is_absolute(p) {
                    return Err(format!("package_repo: apt `{p}` must be an absolute path"));
                }
            }
            if let Some(k) = &a.keyring {
                check_apt_extension("keyring", k, &[".gpg", ".asc"])?;
            }
            if let Some(f) = &a.file {
                check_apt_extension("file", f, &[".sources", ".list"])?;
                if a.replaces.contains(f) {
                    return Err(format!(
                        "package_repo: apt `replaces` lists `{f}`, the sources file itself"
                    ));
                }
            }
        }
        if let Some(d) = &self.dnf {
            check_word("package_repo: dnf baseurl", &d.baseurl)?;
            if let Some(k) = &d.gpgkey {
                check_key_url("package_repo: dnf gpgkey", k, true)?;
            }
            if let Some(p) = &d.file
                && !is_template(p)
                && !is_absolute(p)
            {
                return Err(format!("package_repo: dnf `{p}` must be an absolute path"));
            }
        }
        if let Some(z) = &self.zypper {
            check_word("package_repo: zypper url", &z.url)?;
            if let Some(k) = &z.gpgkey {
                check_key_url("package_repo: zypper gpgkey", k, true)?;
            }
        }
        if let Some(ScalarOr(b)) = &self.brew {
            let parts: Vec<&str> = b.tap.split('/').collect();
            if !is_template(&b.tap)
                && (parts.len() != 2
                    || parts.iter().any(|p| p.is_empty())
                    || b.tap.chars().any(char::is_whitespace))
            {
                return Err(format!(
                    "package_repo: brew tap `{}` must be `owner/repo`",
                    b.tap
                ));
            }
        }
        if let Some(ScalarOr(s)) = &self.scoop {
            check_word("package_repo: scoop bucket", &s.bucket)?;
        }
        if let Some(w) = &self.winget {
            check_word("package_repo: winget source", &w.source)?;
            check_word("package_repo: winget url", &w.url)?;
        }
        Ok(())
    }
}

/// `user:` — the login shell and supplementary groups of a user (default: the one running
/// `dot`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UserOpts {
    /// The user (default: the one running dot).
    pub name: Option<String>,
    /// Login shell: a name found on `PATH` (`zsh`) or an absolute path. Added to `/etc/shells`
    /// when missing there.
    pub shell: Option<String>,
    /// Supplementary groups the user must be in (others are left alone). They take effect at the
    /// next login.
    #[serde(default)]
    pub groups: Vec<String>,
}

impl Check for UserOpts {
    fn check(&self) -> Result<(), String> {
        if self.shell.is_none() && self.groups.is_empty() {
            return Err("user: expected `shell` or `groups`".into());
        }
        if let Some(n) = &self.name {
            check_name("user: name", n)?;
        }
        if let Some(s) = &self.shell {
            check_word("user: shell", s)?;
        }
        for g in &self.groups {
            check_name("user: group", g)?;
        }
        Ok(())
    }
}

fn check_name(what: &str, v: &str) -> Result<(), String> {
    if is_template(v) {
        return Ok(());
    }
    let ok = !v.is_empty()
        && !v.starts_with('-')
        && v.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '$'));
    if ok {
        Ok(())
    } else {
        Err(format!("{what} `{v}` is not a valid name"))
    }
}

/// `absent:` options for one path.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AbsentOpts {
    /// Allow a path outside the home directory, removed through sudo when this user cannot
    /// (default: false).
    #[serde(default)]
    pub root: bool,
    /// `link`: remove it only while it is a symlink (or junction), never a real file or
    /// directory someone put there: for retiring a link an old setup made.
    pub only: Option<AbsentOnly>,
    /// With `only: link`: only a link pointing into this directory (`~/dotfiles`), as an old
    /// setup made it; a link pointing elsewhere is kept.
    pub pointing_into: Option<String>,
}

/// What an `absent:` entry removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AbsentOnly {
    /// Only a symlink (or junction).
    Link,
}

// ---------------------------------------------------------------------------------------------
// Validated model

/// A validated `package_repo:` step.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageRepoSpec {
    pub name: String,
    pub apt: Option<AptRepo>,
    pub dnf: Option<DnfRepo>,
    pub zypper: Option<ZypperRepo>,
    pub brew: Option<BrewTap>,
    pub scoop: Option<ScoopBucket>,
    pub winget: Option<WingetSource>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserSpec {
    pub name: Option<String>,
    pub shell: Option<String>,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AbsentEntry {
    /// As written (`~/...` or absolute).
    pub path: String,
    pub root: bool,
    /// Only a symlink is removed (`only: link`).
    pub link_only: bool,
    /// Only a link into this directory (as written).
    pub pointing_into: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Hooks for the parser, the model and the schema

/// The directive keys this module adds.
pub const DIRECTIVES: [&str; 3] = ["package_repo", "user", "absent"];

/// Which of this module's directive keys a step sets.
pub(super) fn directive_keys(s: &StepFields) -> Vec<&'static str> {
    let set = [
        s.package_repo.is_some(),
        s.user.is_some(),
        s.absent.is_some(),
    ];
    DIRECTIVES
        .iter()
        .zip(set)
        .filter_map(|(k, set)| set.then_some(*k))
        .collect()
}

/// `absent:` paths must not depend on the working directory. `None`: not an `absent` step.
pub(super) fn check_targets(s: &StepFields) -> Option<Result<(), String>> {
    let a = s.absent.as_ref()?;
    Some(
        match a
            .0
            .iter()
            .map(|(k, _)| k.as_str())
            .find(|p| !is_template(p) && !is_target_path(p))
        {
            Some(p) => Err(format!(
                "absent: `{p}` must be an absolute path or start with `~/`"
            )),
            None if a.0.is_empty() => Err("absent: expected at least one path".into()),
            None => Ok(()),
        },
    )
}

/// A Unix absolute path (apt and dnf files are Linux paths), on whichever OS the config is
/// parsed: a Linux step (under a `when:`) must still parse on Windows.
fn is_absolute(p: &str) -> bool {
    p.starts_with('/') || std::path::Path::new(p).is_absolute()
}

/// Takes this module's directive out of a checked step, if it has one.
pub(super) fn take_directive(s: &mut StepFields) -> Option<Directive> {
    if let Some(Checked(r)) = s.package_repo.take() {
        return Some(Directive::PackageRepo(Box::new(PackageRepoSpec {
            name: r.name,
            apt: r.apt,
            dnf: r.dnf,
            zypper: r.zypper,
            brew: r.brew.map(|b| b.0),
            scoop: r.scoop.map(|b| b.0),
            winget: r.winget,
        })));
    }
    if let Some(Checked(u)) = s.user.take() {
        return Some(Directive::User(UserSpec {
            name: u.name,
            shell: u.shell,
            groups: u.groups,
        }));
    }
    if let Some(a) = s.absent.take() {
        return Some(Directive::Absent(
            a.0.into_iter()
                .map(|(path, o)| AbsentEntry {
                    path,
                    root: o.root,
                    link_only: o.only == Some(AbsentOnly::Link) || o.pointing_into.is_some(),
                    pointing_into: o.pointing_into,
                })
                .collect(),
        ));
    }
    None
}

/// The generated label of one of this module's directives.
pub(super) fn default_label(d: &Directive) -> Option<String> {
    Some(match d {
        Directive::PackageRepo(r) => format!("package_repo {}", r.name),
        Directive::User(u) => {
            let mut parts = Vec::new();
            if let Some(s) = &u.shell {
                parts.push(format!("shell {s}"));
            }
            if !u.groups.is_empty() {
                parts.push(format!("groups {}", u.groups.join(", ")));
            }
            format!("user {}", parts.join("; "))
        }
        Directive::Absent(e) => {
            let items: Vec<&str> = e.iter().map(|e| e.path.as_str()).collect();
            match items.len() {
                1..=3 => format!("absent {}", items.join(", ")),
                n => format!("absent {}, ... ({n} total)", items[..2].join(", ")),
            }
        }
        _ => return None,
    })
}

/// Adds this module's directives to the step schema's `properties`.
pub(super) fn schema_properties(g: &mut SchemaGenerator) -> Vec<(&'static str, Schema)> {
    let target = json_schema!({ "type": "string", "pattern": TARGET_PATTERN });
    let mut repo = g.subschema_for::<Checked<PackageRepoOpts>>();
    repo.insert(
        "description".into(),
        "A vendor package repository for this machine's package manager (apt, dnf, zypper, brew tap, scoop bucket, winget source).".into(),
    );
    let mut user = g.subschema_for::<Checked<UserOpts>>();
    user.insert(
        "description".into(),
        "A user's login shell and supplementary groups.".into(),
    );
    let absent = json_schema!({
        "description": "Paths that must not exist (removed if they do). Outside the home directory only with `root: true`.",
        "anyOf": [
            { "type": "array", "items": target, "minItems": 1 },
            {
                "type": "object",
                "propertyNames": target,
                "additionalProperties": g.subschema_for::<Option<AbsentOpts>>(),
                "minProperties": 1
            }
        ]
    });
    vec![("package_repo", repo), ("user", user), ("absent", absent)]
}

/// The step fields' types, as [`StepFields`] declares them.
pub type PackageRepoField = Checked<PackageRepoOpts>;
pub type UserField = Checked<UserOpts>;
pub type AbsentField = ListOrMap<AbsentOpts>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Bundle, ConfigError, parse_bundle};

    fn parse(yaml: &str) -> Result<Bundle, ConfigError> {
        parse_bundle(
            yaml,
            &crate::paths::tests::abs("/nonexistent-dot-test/b/bundle.yml"),
        )
    }

    fn err(yaml: &str) -> String {
        match parse(yaml) {
            Ok(b) => panic!("expected an error, parsed {:#?}", b.steps),
            Err(e) => e.to_string(),
        }
    }

    const ALL: &str = r#"
steps:
  - package_repo:
      name: docker
      apt:
        url: https://download.docker.com/linux/ubuntu
        key: https://download.docker.com/linux/ubuntu/gpg
        fingerprint: 9DC8 5822 9FC7 DD38 854A  E2D8 8D81 803C 0EBF CD88
        suites: "{{ codename }}"
        fallback_suite: noble
        components: [stable]
      yum: { baseurl: "https://x/$basearch", gpgkey: https://x/gpg }
      brew: hashicorp/tap
      scoop: { bucket: mine, url: https://example.com/b.git }
      winget: { source: corp, url: https://w.example.com, type: Microsoft.Rest }
  - package_repo: { name: gh, apt: { url: https://cli.github.com/packages, suites: [stable] } }
  - user: { shell: zsh, groups: [docker, libvirt] }
  - user: { name: alice, groups: [wheel] }
  - absent: [~/.old]
  - absent:
      /etc/apt/sources.list.d/docker.list: { root: true }
      ~/.x:
"#;

    #[test]
    fn parses_every_shape() {
        let b = parse(ALL).unwrap();
        let Directive::PackageRepo(r) = &b.steps[0].directive else {
            panic!()
        };
        let apt = r.apt.as_ref().unwrap();
        assert_eq!(apt.suites.0, ["{{ codename }}"]);
        assert_eq!(apt.fallback_suite.as_deref(), Some("noble"));
        assert_eq!(
            r.dnf.as_ref().unwrap().gpgkey.as_deref(),
            Some("https://x/gpg")
        );
        assert_eq!(r.brew.as_ref().unwrap().tap, "hashicorp/tap");
        assert_eq!(r.scoop.as_ref().unwrap().bucket, "mine");
        assert_eq!(
            r.winget.as_ref().unwrap().kind.as_deref(),
            Some("Microsoft.Rest")
        );
        let Directive::User(u) = &b.steps[2].directive else {
            panic!()
        };
        assert_eq!(
            (u.shell.as_deref(), u.groups.len(), u.name.as_deref()),
            (Some("zsh"), 2, None)
        );
        let Directive::Absent(a) = &b.steps[5].directive else {
            panic!()
        };
        assert_eq!(
            a[0],
            AbsentEntry {
                path: "/etc/apt/sources.list.d/docker.list".into(),
                root: true,
                link_only: false,
                pointing_into: None,
            }
        );
        assert!(!a[1].root);
        let labels: Vec<String> = b.steps.iter().map(|s| s.display_label()).collect();
        assert_eq!(labels[0], "package_repo docker");
        assert_eq!(labels[2], "user shell zsh; groups docker, libvirt");
        assert_eq!(labels[4], "absent ~/.old");
        let kinds: Vec<&str> = b.steps.iter().map(|s| s.directive.kind()).collect();
        assert_eq!(
            kinds,
            [
                "package_repo",
                "package_repo",
                "user",
                "user",
                "absent",
                "absent"
            ]
        );
    }

    #[test]
    fn strict_errors_carry_file_and_line() {
        let cases = [
            (
                "  - package_repo: { name: 'a b', brew: x/y }\n",
                "name `a b`",
            ),
            ("  - package_repo: { name: a }\n", "at least one of apt"),
            (
                "  - package_repo: { name: a, apt: { url: u, suites: s, fingerprint: AB } }\n",
                "`fingerprint` needs `key`",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, key: 'https://k', suites: s, fingerprint: XYZ } }\n",
                "not a hexadecimal",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, key: 'https://k', suites: s, fingerprint: 8D81803C0EBFCD88 } }\n",
                "not a key id",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, key: 'https://k', suites: s, fingerprint: [9DC858229FC7DD38854AE2D88D81803C0EBFCD88, 8D81803C0EBFCD88] } }\n",
                "not a key id",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, key: 'http://k/gpg', suites: s } }\n",
                "must be an https:// URL",
            ),
            (
                "  - package_repo: { name: a, dnf: { baseurl: u, gpgkey: 'http://k/gpg' } }\n",
                "must be an https:// URL (or file://)",
            ),
            (
                "  - package_repo: { name: a, zypper: { url: u, gpgkey: 'ftp://k' } }\n",
                "zypper gpgkey",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, suites: s, keyring: /etc/apt/keyrings/a.key } }\n",
                "must end in `.gpg` or `.asc`",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, suites: s, file: /etc/apt/sources.list.d/a.txt } }\n",
                "must end in `.sources` or `.list`",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, suites: s, replaces: [docker.list] } }\n",
                "must be an absolute path",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, suites: [a, b], fallback_suite: c } }\n",
                "exactly one suite",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, suites: s, keyring: rel/k } }\n",
                "must be an absolute path",
            ),
            (
                "  - package_repo: { name: a, apt: { url: u, suite: s } }\n",
                "unknown field `suite`",
            ),
            (
                "  - package_repo: { name: a, brew: justone }\n",
                "`owner/repo`",
            ),
            ("  - user: {}\n", "expected `shell` or `groups`"),
            ("  - user: { groups: ['a b'] }\n", "not a valid name"),
            ("  - absent: [rel/x]\n", "must be an absolute path"),
            ("  - absent: []\n", "at least one path"),
            (
                "  - absent: { ~/x: { rot: true } }\n",
                "unknown field `rot`",
            ),
            ("  - absent: [~/x]\n    user: { shell: zsh }\n", "found 2"),
        ];
        for (step, want) in cases {
            let e = err(&format!("steps:\n{step}"));
            assert!(e.contains(want), "{step}: {e}");
            assert!(
                e.contains("bundle.yml:2:") || e.contains("bundle.yml:3:"),
                "{step}: {e}"
            );
        }
    }

    #[test]
    fn schema_covers_the_new_directives() {
        let schema = serde_json::to_value(crate::config::json_schema()).unwrap();
        let step = &schema["$defs"]["Step"];
        for d in DIRECTIVES {
            assert!(step["properties"].get(d).is_some(), "schema lacks {d}");
        }
        let text = schema.to_string();
        for word in [
            "fallback_suite",
            "fingerprint",
            "if_missing",
            "baseurl",
            "groups",
        ] {
            assert!(text.contains(word), "schema lacks {word}");
        }
    }
}
