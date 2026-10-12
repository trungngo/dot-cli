//! Bundle files: parsing, validation, and the validated model the engine plans from.
//!
//! A `bundle.yml` holds manifest keys (`name`, `requires`, `when`, ...) and an ordered `steps:`
//! list. Each step has the common keys `when` / `label` and exactly one directive key. Parsing is
//! strict: unknown keys, a step with zero or two directives, unknown facts and malformed modes are
//! errors that name the file and line.

pub mod filesys;
pub mod network;
pub mod package;
pub mod raw;
pub mod shapes;
pub mod system;

use std::fmt;
use std::path::{Path, PathBuf};

pub use filesys::{
    BlockSpec, DefaultsType, FileEntry, MacosDefault, PathDirEntry, PathPersist, SecretFileEntry,
    ServiceManagerKind, ServiceSpec,
};
pub use network::{
    AssetSpec, BinEntry, DownloadFrom, DownloadSpec, DownloadTo, Extract, GitSpec,
    GithubReleaseSpec, LatestOpts, ReleaseRef, ReleaseSource,
};
pub use package::{PackageItem, PackageSpec, PackageStep, PkgManager, WingetScope};
pub use raw::{DefaultsFile, OnConflict, ShellInterpreter, When};
pub use shapes::{Mode, ModeSpec};
pub use system::{
    AbsentEntry, AbsentOnly, AptRepo, BrewTap, DnfRepo, PackageRepoSpec, ScoopBucket, UserSpec,
    WingetSource, ZypperRepo, check_fingerprint, check_key_url,
};

use crate::vars::{Layer, Source};

/// A problem in a config file. `line`/`column` are 1-based when known.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ConfigError {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file.display())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
            if let Some(col) = self.column {
                write!(f, ":{col}")?;
            }
        }
        write!(f, ": {}", self.message)
    }
}

impl ConfigError {
    pub fn new(file: &Path, message: impl Into<String>) -> Self {
        Self {
            file: file.to_path_buf(),
            line: None,
            column: None,
            message: message.into(),
        }
    }

    /// An error about a step, located at the step's position when it is known.
    pub fn at_step(bundle: &Bundle, step: &Step, message: impl AsRef<str>) -> Self {
        let at = match bundle.handler_name(step) {
            Some(name) => format!("handlers.{name}"),
            None => format!("steps[{}]", step.index),
        };
        Self {
            file: bundle.file.clone(),
            line: step.pos.map(|p| p.line),
            column: step.pos.map(|p| p.column),
            message: format!("{at}: {}", message.as_ref()),
        }
    }

    pub(crate) fn from_yaml(file: &Path, err: serde_yaml_ng::Error) -> Self {
        let mut message = err.to_string();
        let (line, column) = match err.location() {
            Some(loc) => {
                // The library appends " at line L column C"; we print the location up front.
                let suffix = format!(" at line {} column {}", loc.line(), loc.column());
                if let Some(stripped) = message.strip_suffix(&suffix) {
                    message = stripped.to_string();
                }
                (Some(loc.line()), Some(loc.column()))
            }
            None => (None, None),
        };
        Self {
            file: file.to_path_buf(),
            line,
            column,
            message,
        }
    }
}

/// A parsed, validated bundle.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub name: String,
    pub description: Option<String>,
    /// Absolute path of the `bundle.yml`.
    pub file: PathBuf,
    /// Directory holding the bundle file: base for relative sources and the default shell cwd.
    pub dir: PathBuf,
    /// Repository root: the nearest ancestor holding `dot.yml`, else `dir`. `clean` only removes
    /// dangling links that point inside it.
    pub root: PathBuf,
    pub manifest: Manifest,
    pub steps: Vec<Step>,
    /// Steps run at the end of the bundle, each once, when a step naming it in `notify:`
    /// changed something. Their [`Step::index`] continues after `steps`.
    pub handlers: Vec<Handler>,
}

/// A bundle's `handlers:` entry.
#[derive(Debug, Clone)]
pub struct Handler {
    pub name: String,
    pub step: Step,
}

impl Bundle {
    /// The steps, then the handlers' steps: what the engine plans, in order.
    pub fn all_steps(&self) -> impl Iterator<Item = &Step> {
        self.steps
            .iter()
            .chain(self.handlers.iter().map(|h| &h.step))
    }

    /// The name of the handler `step` is, if it is one.
    pub fn handler_name(&self, step: &Step) -> Option<&str> {
        let i = step.index.checked_sub(self.steps.len())?;
        self.handlers.get(i).map(|h| h.name.as_str())
    }

    /// How a step is shown: its label, and for a handler `handler <name>: <label>`.
    pub fn step_label(&self, step: &Step) -> String {
        match self.handler_name(step) {
            Some(name) => format!("handler {name}: {}", step.display_label()),
            None => step.display_label(),
        }
    }
}

/// Bundle-level keys: dependencies (see [`crate::graph`]), conditions, variables and defaults.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub requires: Vec<String>,
    pub after: Vec<String>,
    pub provides: Vec<String>,
    pub requires_if: Vec<(When, Vec<String>)>,
    pub when: Option<When>,
    pub check: Option<String>,
    /// Always included when its `when:` matches.
    pub implicit: bool,
    /// The bundle's `vars:` layer.
    pub vars: Layer,
    /// The bundle's own option defaults (innermost layer).
    pub defaults: Defaults,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            requires: Vec::new(),
            after: Vec::new(),
            provides: Vec::new(),
            requires_if: Vec::new(),
            when: None,
            check: None,
            implicit: false,
            vars: Layer::new(crate::vars::Source::bundle("")),
            defaults: Defaults::default(),
        }
    }
}

/// A 1-based position in a config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone)]
pub struct Step {
    /// 0-based position in `steps:`.
    pub index: usize,
    /// Where the step starts in the file, so errors found after parsing (template rendering,
    /// plan-time validation) can still name file and line.
    pub pos: Option<Pos>,
    pub when: Option<When>,
    pub label: Option<String>,
    /// Handlers of the bundle to run when this step changes something.
    pub notify: Vec<String>,
    /// `if:`: an expression over variables; the step is skipped unless it is true.
    pub condition: Option<String>,
    /// `only_if:` / `unless:` (not on `shell`, whose own `unless` it is): commands asked when
    /// the step has something to do.
    pub guard: Option<Guard>,
    pub directive: Directive,
}

/// A step's command guard, asked only when the step has something to do (or cannot be
/// planned): it is skipped unless `only_if` exits 0, or when `unless` exits 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    OnlyIf(String),
    Unless(String),
}

impl Step {
    /// The user's label, or one generated from the directive.
    pub fn display_label(&self) -> String {
        self.label
            .clone()
            .unwrap_or_else(|| self.directive.default_label())
    }
}

#[derive(Debug, Clone)]
pub enum Directive {
    Link(Vec<LinkEntry>),
    Create(Vec<CreateEntry>),
    Shell(ShellSpec),
    Clean(Vec<CleanEntry>),
    Package(PackageStep),
    GithubRelease(GithubReleaseSpec),
    Git(GitSpec),
    Download(DownloadSpec),
    Template(Vec<FileEntry>),
    Copy(Vec<FileEntry>),
    SecretFile(Vec<SecretFileEntry>),
    Block(BlockSpec),
    Path(Vec<PathDirEntry>),
    /// Variable name -> value (a template).
    Env(Vec<(String, String)>),
    MacosDefaults(Vec<MacosDefault>),
    Service(ServiceSpec),
    PackageRepo(Box<PackageRepoSpec>),
    User(UserSpec),
    Absent(Vec<AbsentEntry>),
}

impl Directive {
    pub fn kind(&self) -> &'static str {
        match self {
            Directive::Link(_) => "link",
            Directive::Create(_) => "create",
            Directive::Shell(_) => "shell",
            Directive::Clean(_) => "clean",
            Directive::Package(_) => "package",
            Directive::GithubRelease(_) => "github_release",
            Directive::Git(_) => "git",
            Directive::Download(_) => "download",
            Directive::Template(_) => "template",
            Directive::Copy(_) => "copy",
            Directive::SecretFile(_) => "secret_file",
            Directive::Block(_) => "block",
            Directive::Path(_) => "path",
            Directive::Env(_) => "env",
            Directive::MacosDefaults(_) => "macos_defaults",
            Directive::Service(_) => "service",
            Directive::PackageRepo(_) => "package_repo",
            Directive::User(_) => "user",
            Directive::Absent(_) => "absent",
        }
    }

    pub fn default_label(&self) -> String {
        if let Some(label) = filesys::default_label(self).or_else(|| system::default_label(self)) {
            return label;
        }
        fn list<'a>(kind: &str, items: impl Iterator<Item = &'a str>) -> String {
            let items: Vec<&str> = items.collect();
            match items.len() {
                0 => format!("{kind} (nothing)"),
                1..=3 => format!("{kind} {}", items.join(", ")),
                n => format!("{kind} {}, ... ({n} total)", items[..2].join(", ")),
            }
        }
        match self {
            Directive::Link(e) => list("link", e.iter().map(|e| e.dest.as_str())),
            Directive::Create(e) => list("create", e.iter().map(|e| e.path.as_str())),
            Directive::Clean(e) => list("clean", e.iter().map(|e| e.path.as_str())),
            Directive::Package(PackageStep {
                spec: PackageSpec::Names(n),
                ..
            }) => list("package", n.iter().map(String::as_str)),
            Directive::Package(PackageStep {
                spec: PackageSpec::ByManager(m),
                ..
            }) => list(
                "package",
                m.iter()
                    .flat_map(|(_, items)| items.iter().map(|i| i.name.as_str())),
            ),
            Directive::GithubRelease(g) => format!("github_release {}", g.repo),
            Directive::Git(g) => format!("git {}", g.dest),
            Directive::Download(d) => match &d.to {
                DownloadTo::Path(p) => format!("download {p}"),
                DownloadTo::Package(p) => format!("download package {p}"),
            },
            Directive::Shell(s) => {
                let first = s.cmd.lines().next().unwrap_or("").trim();
                let mut short: String = first.chars().take(60).collect();
                if short.len() < first.len() || s.cmd.trim().lines().count() > 1 {
                    short.push_str(" ...");
                }
                format!("shell: {short}")
            }
            _ => unreachable!("labelled by filesys::default_label"),
        }
    }
}

/// Option values a config may leave unset, inherited from outer layers. Every field is `None`
/// unless that layer sets it; [`Defaults::or`] stacks layers and the `resolve` methods of the
/// entries apply the built-in values last.
///
/// Filled from `dot.yml`, the profile and the bundle's own `defaults:`, innermost first
/// ([`crate::Context::defaults`] for bundle files applied with `-c`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Defaults {
    pub link: LinkDefaults,
    pub shell: ShellDefaults,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LinkDefaults {
    pub create: Option<bool>,
    pub relink: Option<bool>,
    pub on_conflict: Option<OnConflict>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellDefaults {
    pub interactive: Option<bool>,
    pub interpreter: Option<ShellInterpreter>,
    /// Script lines run before each `shell` step's `cmd`, `unless` and `upgrade` (sourcing a
    /// tool's init script, relaxed shell options).
    pub prelude: Option<String>,
}

impl Defaults {
    /// `self`, with every unset field taken from `outer`.
    pub fn or(&self, outer: &Defaults) -> Defaults {
        Defaults {
            link: LinkDefaults {
                create: self.link.create.or(outer.link.create),
                relink: self.link.relink.or(outer.link.relink),
                on_conflict: self.link.on_conflict.or(outer.link.on_conflict),
            },
            shell: ShellDefaults {
                interactive: self.shell.interactive.or(outer.shell.interactive),
                interpreter: self.shell.interpreter.or(outer.shell.interpreter),
                prelude: self
                    .shell
                    .prelude
                    .clone()
                    .or_else(|| outer.shell.prelude.clone()),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkEntry {
    /// Destination as written (`~/...` or absolute).
    pub dest: String,
    /// Source as written, relative to the bundle directory unless absolute.
    pub src: String,
    /// `None`: inherit (built-in default `true`).
    pub create: Option<bool>,
    /// `None`: inherit (built-in default `true`).
    pub relink: Option<bool>,
    /// `None`: inherit (built-in default `backup`).
    pub on_conflict: Option<OnConflict>,
}

/// A link entry's options after inheritance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkOptions {
    pub create: bool,
    pub relink: bool,
    pub on_conflict: OnConflict,
}

impl LinkEntry {
    pub fn resolve(&self, defaults: &Defaults) -> LinkOptions {
        let d = &defaults.link;
        LinkOptions {
            create: self.create.or(d.create).unwrap_or(true),
            relink: self.relink.or(d.relink).unwrap_or(true),
            on_conflict: self
                .on_conflict
                .or(d.on_conflict)
                .unwrap_or(OnConflict::Backup),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateEntry {
    pub path: String,
    pub mode: Option<ModeSpec>,
    /// Root-owned: made through sudo.
    pub root: bool,
    /// An empty file, made when missing, instead of a directory.
    pub file: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShellSpec {
    pub cmd: String,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// `None`: inherit (built-in default `false`).
    pub interactive: Option<bool>,
    pub unless: Option<String>,
    /// Run instead of skipping when `unless` succeeds, under `--upgrade`.
    pub upgrade: Option<String>,
    /// Sensitive variables exported to the command anyway.
    pub export_sensitive: Vec<String>,
    /// The command uses `sudo`, so the run primes it up front.
    pub sudo: bool,
    /// `None`: inherit (built-in default: the platform's).
    pub interpreter: Option<ShellInterpreter>,
    /// `None`: inherit (built-in default: none).
    pub prelude: Option<String>,
}

impl ShellSpec {
    /// The step's prelude, else the inherited one.
    pub fn prelude<'a>(&'a self, defaults: &'a Defaults) -> Option<&'a str> {
        self.prelude
            .as_deref()
            .or(defaults.shell.prelude.as_deref())
            .filter(|p| !p.trim().is_empty())
    }

    pub fn interactive(&self, defaults: &Defaults) -> bool {
        self.interactive
            .or(defaults.shell.interactive)
            .unwrap_or(false)
    }

    /// The step's interpreter, else the inherited default; `None`: the platform's.
    pub fn interpreter(&self, defaults: &Defaults) -> Option<ShellInterpreter> {
        self.interpreter.or(defaults.shell.interpreter)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CleanEntry {
    pub path: String,
    pub recursive: bool,
}

/// Reads and validates one bundle file.
pub fn load_bundle(path: &Path) -> Result<Bundle, ConfigError> {
    let file = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = std::fs::read_to_string(&file)
        .map_err(|e| ConfigError::new(&file, format!("cannot read: {e}")))?;
    parse_bundle(&text, &file)
}

/// Parses bundle text as if it were read from `file` (which should be absolute).
pub fn parse_bundle(text: &str, file: &Path) -> Result<Bundle, ConfigError> {
    let raw: raw::BundleFile =
        serde_yaml_ng::from_str(text).map_err(|e| ConfigError::from_yaml(file, e))?;
    let dir = file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let root = find_root(&dir);
    let name = match raw.name {
        Some(n) if n.trim().is_empty() => return Err(ConfigError::new(file, "`name` is empty")),
        Some(n) => n,
        None => dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "bundle".to_string()),
    };
    let positions = step_positions(text, raw.steps.len());
    let n = raw.steps.len();
    let steps: Vec<Step> = raw
        .steps
        .into_iter()
        .zip(positions)
        .enumerate()
        .map(|(index, (s, pos))| convert_step(index, pos, s.0))
        .collect();
    let handlers = convert_handlers(file, n, raw.handlers)?;
    check_notify(file, &steps, &handlers)?;
    Ok(Bundle {
        description: raw.description,
        file: file.to_path_buf(),
        dir,
        root,
        manifest: Manifest {
            requires: raw.requires,
            after: raw.after,
            provides: raw.provides,
            requires_if: raw
                .requires_if
                .into_iter()
                .map(|r| (r.when, r.requires))
                .collect(),
            when: raw.when,
            check: raw.check,
            implicit: raw.implicit,
            vars: Layer::from_specs(
                Source::bundle(&name),
                raw.vars.0.iter().map(|(k, v)| (k, v)),
            ),
            defaults: raw
                .defaults
                .as_ref()
                .map(DefaultsFile::to_defaults)
                .unwrap_or_default(),
        },
        name,
        steps,
        handlers,
    })
}

/// The `handlers:` of a bundle with `n` steps.
fn convert_handlers(
    file: &Path,
    n: usize,
    raw: shapes::OrderedMap<raw::Step>,
) -> Result<Vec<Handler>, ConfigError> {
    let mut handlers = Vec::new();
    for (i, (name, s)) in raw.0.into_iter().enumerate() {
        let ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !ok {
            return Err(ConfigError::new(
                file,
                format!("handlers: `{name}` may only use letters, digits, `.`, `_` and `-`"),
            ));
        }
        if s.0.notify.is_some() {
            return Err(ConfigError::new(
                file,
                format!("handlers.{name}: a handler cannot `notify` others"),
            ));
        }
        handlers.push(Handler {
            step: convert_step(n + i, None, s.0),
            name,
        });
    }
    Ok(handlers)
}

/// Every name in a step's `notify:` is one of the bundle's handlers.
fn check_notify(file: &Path, steps: &[Step], handlers: &[Handler]) -> Result<(), ConfigError> {
    for step in steps {
        let Some(missing) = step
            .notify
            .iter()
            .find(|want| !handlers.iter().any(|h| &h.name == *want))
        else {
            continue;
        };
        let known: Vec<&str> = handlers.iter().map(|h| h.name.as_str()).collect();
        return Err(ConfigError {
            file: file.to_path_buf(),
            line: step.pos.map(|p| p.line),
            column: step.pos.map(|p| p.column),
            message: format!(
                "steps[{}]: notify: no handler `{missing}` in this bundle ({})",
                step.index,
                if known.is_empty() {
                    "it has no `handlers:`".to_string()
                } else {
                    format!("its handlers: {}", known.join(", "))
                }
            ),
        });
    }
    Ok(())
}

/// Like [`load_bundle`], but on failure reports every problem it can find, not just the first:
/// the parser's error (exact line) plus each other step that fails on its own (located at the
/// step's start) and top-level keys that fail on their own. For `dot check`.
pub fn load_bundle_all_errors(path: &Path) -> Result<Bundle, Vec<ConfigError>> {
    let file = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = std::fs::read_to_string(&file)
        .map_err(|e| vec![ConfigError::new(&file, format!("cannot read: {e}"))])?;
    let first = match parse_bundle(&text, &file) {
        Ok(b) => return Ok(b),
        Err(e) => e,
    };
    let mut errors = vec![first.clone()];
    let Ok(serde_yaml_ng::Value::Mapping(mut map)) =
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text)
    else {
        return Err(errors);
    };
    let steps = match map.remove("steps") {
        Some(serde_yaml_ng::Value::Sequence(s)) => s,
        _ => Vec::new(),
    };
    let positions = step_positions(&text, steps.len());
    let first_step_line = positions.first().copied().flatten().map(|p| p.line);
    // The step holding the parser's error: the last one starting at or before its line.
    let failing_step = first.line.and_then(|l| {
        positions
            .iter()
            .rposition(|p| p.is_some_and(|p| p.line <= l))
    });
    let manifest_failed = match (first.line, first_step_line) {
        (Some(l), Some(s)) => l < s,
        _ => true,
    };
    if !manifest_failed
        && let Err(e) =
            serde_yaml_ng::from_value::<raw::BundleFile>(serde_yaml_ng::Value::Mapping(map))
    {
        errors.push(ConfigError::new(&file, e.to_string()));
    }
    for (i, (step, pos)) in steps.into_iter().zip(positions).enumerate() {
        if Some(i) == failing_step {
            continue;
        }
        if let Err(e) = serde_yaml_ng::from_value::<raw::Step>(step) {
            errors.push(ConfigError {
                file: file.clone(),
                line: pos.map(|p| p.line),
                column: pos.map(|p| p.column),
                message: format!("steps[{i}]: {e}"),
            });
        }
    }
    Err(errors)
}

/// The start of each of the first `n` steps. serde_yaml_ng has no spanned values, but it does
/// locate errors at the node being deserialized; so for step `i` the text is re-read with a probe
/// that skips `i` steps and fails on the next one, and the error's location is recorded. Bundle
/// files are small, so the quadratic cost does not matter; very long lists go unlocated.
fn step_positions(text: &str, n: usize) -> Vec<Option<Pos>> {
    use serde::de::{
        self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor,
    };

    const MAX_LOCATED: usize = 1000;
    const PROBE: &str = "\0dot-position-probe";

    struct Top(usize);
    impl<'de> DeserializeSeed<'de> for Top {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for Top {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a mapping")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<(), A::Error> {
            while let Some(k) = m.next_key::<String>()? {
                if k == "steps" {
                    m.next_value_seed(Steps(self.0))?;
                } else {
                    m.next_value::<IgnoredAny>()?;
                }
            }
            Ok(())
        }
    }
    struct Steps(usize);
    impl<'de> DeserializeSeed<'de> for Steps {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_seq(self)
        }
    }
    impl<'de> Visitor<'de> for Steps {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a list")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<(), A::Error> {
            for _ in 0..self.0 {
                s.next_element::<IgnoredAny>()?;
            }
            s.next_element_seed(Probe)?;
            Ok(())
        }
    }
    struct Probe;
    impl<'de> DeserializeSeed<'de> for Probe {
        type Value = ();
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for Probe {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a step")
        }
        // Steps are mappings (anything else already failed to parse); the error is located at
        // the start of the mapping.
        fn visit_map<A: MapAccess<'de>>(self, _m: A) -> Result<(), A::Error> {
            Err(de::Error::custom(PROBE))
        }
    }

    (0..n)
        .map(|i| {
            if i >= MAX_LOCATED {
                return None;
            }
            let err = Top(i)
                .deserialize(serde_yaml_ng::Deserializer::from_str(text))
                .err()?;
            if !err.to_string().contains(PROBE) {
                return None;
            }
            err.location().map(|l| Pos {
                line: l.line(),
                column: l.column(),
            })
        })
        .collect()
}

fn convert_step(index: usize, pos: Option<Pos>, mut s: raw::StepFields) -> Step {
    // `Check for StepFields` has already guaranteed exactly one directive.
    let directive = if let Some(d) = filesys::take_directive(&mut s) {
        d
    } else if let Some(d) = system::take_directive(&mut s) {
        d
    } else if let Some(link) = s.link {
        Directive::Link(
            link.0
                .into_iter()
                .map(|(dest, opts)| LinkEntry {
                    dest,
                    src: opts.0.src,
                    create: opts.0.create,
                    relink: opts.0.relink,
                    on_conflict: opts.0.on_conflict,
                })
                .collect(),
        )
    } else if let Some(create) = s.create {
        Directive::Create(
            create
                .0
                .into_iter()
                .map(|(path, o)| CreateEntry {
                    path,
                    mode: o.mode,
                    root: o.root,
                    file: o.file,
                })
                .collect(),
        )
    } else if let Some(shell) = s.shell {
        let o = shell.0;
        Directive::Shell(ShellSpec {
            cmd: o.cmd,
            cwd: o.cwd,
            env: o.env.0,
            interactive: o.interactive,
            // A step-level `unless` on a shell step is the command's own (checked while
            // parsing that only one is given).
            unless: o.unless.or(s.unless.take()),
            upgrade: o.upgrade,
            export_sensitive: o.export_sensitive,
            sudo: o.sudo,
            interpreter: o.interpreter,
            prelude: o.prelude,
        })
    } else if let Some(package) = s.package {
        Directive::Package(package.0)
    } else if let Some(clean) = s.clean {
        Directive::Clean(
            clean
                .0
                .into_iter()
                .map(|(path, o)| CleanEntry {
                    path,
                    recursive: o.recursive,
                })
                .collect(),
        )
    } else if let Some(g) = s.github_release {
        Directive::GithubRelease(g.0.into())
    } else if let Some(g) = s.git {
        Directive::Git(g.0.into())
    } else if let Some(d) = s.download {
        Directive::Download(d.0.into())
    } else {
        unreachable!("step validated to have exactly one directive")
    };
    let guard = match (s.only_if, s.unless) {
        (Some(c), _) => Some(Guard::OnlyIf(c)),
        (None, Some(c)) => Some(Guard::Unless(c)),
        (None, None) => None,
    };
    Step {
        index,
        pos,
        when: s.when,
        label: s.label,
        notify: s.notify.map(|n| n.0).unwrap_or_default(),
        condition: s.condition,
        guard,
        directive,
    }
}

/// The nearest ancestor of `dir` (inclusive) holding a `dot.yml`, else `dir` itself.
pub fn find_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|d| d.join("dot.yml").is_file())
        .unwrap_or(dir)
        .to_path_buf()
}

/// File names recognised as bundle files by `dot check <dir>`.
pub const BUNDLE_FILE_NAMES: [&str; 2] = ["bundle.yml", "bundle.yaml"];

/// Every bundle file under `dir`, recursively, sorted. Hidden directories (`.git`, ...) and
/// symlinked directories are not entered.
pub fn find_bundle_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    stack.push(entry.path());
                }
            } else if ft.is_file() && BUNDLE_FILE_NAMES.contains(&name.as_ref()) {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Host-independent checks beyond parsing, for `dot check`: every link source exists.
pub fn lint(bundle: &Bundle) -> Vec<ConfigError> {
    let mut out = Vec::new();
    for step in bundle.all_steps() {
        filesys::lint_step(bundle, step, &mut out);
        if let Directive::Link(entries) = &step.directive {
            for e in entries {
                let src = bundle.dir.join(&e.src);
                let checkable = !e.src.starts_with('~') && !crate::template::is_template(&e.src);
                if checkable && std::fs::symlink_metadata(&src).is_err() {
                    out.push(ConfigError::at_step(
                        bundle,
                        step,
                        format!(
                            "link source `{}` for `{}` does not exist ({})",
                            e.src,
                            e.dest,
                            src.display()
                        ),
                    ));
                }
            }
        }
    }
    out
}

/// Where a bundle uses a secret, found without planning: for `dot check`, and to say where an
/// undeclared name is used.
#[derive(Debug, Clone, PartialEq)]
pub struct SecretSite {
    pub name: String,
    /// The location; `message` says how it is used.
    pub at: ConfigError,
}

/// Every secret a bundle names, read statically: `secret("name")` calls in `bundle.yml` and in
/// `template:` sources (outside comment lines), `secret_file:` names and `github_release`
/// `token:`s. In file order; a name may appear more than once.
pub fn secret_uses(bundle: &Bundle) -> Vec<SecretSite> {
    let mut out = Vec::new();
    scan_secret_calls(&bundle.file, &mut out);
    for step in bundle.all_steps() {
        let mut named = |name: &str, how: String| {
            out.push(SecretSite {
                name: name.to_string(),
                at: ConfigError::at_step(bundle, step, how),
            })
        };
        match &step.directive {
            Directive::SecretFile(entries) => {
                for e in entries
                    .iter()
                    .filter(|e| !crate::template::is_template(&e.secret))
                {
                    named(&e.secret, format!("secret_file {}", e.dest));
                }
            }
            Directive::GithubRelease(g) => {
                if let Some(t) = &g.token {
                    named(t, format!("github_release {} token", g.repo));
                }
            }
            Directive::Template(entries) => {
                for e in entries {
                    if !e.src.starts_with('~') && !crate::template::is_template(&e.src) {
                        scan_secret_calls(&bundle.dir.join(&e.src), &mut out);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// `secret("name")` / `secret('name')` calls in `file`, skipping lines that are YAML or Jinja
/// comments. An unreadable file has none (other checks report it).
fn scan_secret_calls(file: &Path, out: &mut Vec<SecretSite>) {
    static CALL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"\bsecret\(\s*(?:"([^"\\]+)"|'([^'\\]+)')\s*\)"#).expect("valid regex")
    });
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with('#') || t.starts_with("{#") {
            continue;
        }
        for c in CALL.captures_iter(line) {
            let Some(name) = c.get(1).or_else(|| c.get(2)) else {
                continue;
            };
            let whole = c.get(0).expect("group 0 always matches");
            out.push(SecretSite {
                name: name.as_str().to_string(),
                at: ConfigError {
                    file: file.to_path_buf(),
                    line: Some(i + 1),
                    column: Some(line[..whole.start()].chars().count() + 1),
                    message: whole.as_str().to_string(),
                },
            });
        }
    }
}

/// The JSON Schema for `bundle.yml`, generated from the config structs.
pub fn json_schema() -> schemars::Schema {
    schemars::schema_for!(raw::BundleFile)
}

#[cfg(test)]
mod tests;
