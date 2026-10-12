//! The on-disk shape of `bundle.yml`, exactly as serde reads it. These types also generate the
//! JSON Schema. The loader turns them into the validated types in [`super`].

use std::borrow::Cow;
use std::collections::BTreeMap;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer};

use super::filesys;
use super::package::{PackageField, package_schema};
use super::shapes::{
    Check, Checked, FromScalar, ListOrMap, ModeSpec, OneOrMany, OrderedMap, ScalarOr,
};
use super::system;
use crate::facts::Fact;
use crate::template::is_template;
use crate::vars::VarSpec;

/// A bundle: manifest keys, then an ordered list of steps.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "dot bundle")]
pub struct BundleFile {
    /// Bundle name; defaults to the name of the directory holding `bundle.yml`.
    pub name: Option<String>,
    /// Free text: what the bundle is for.
    pub description: Option<String>,
    /// Bundles (or `cap:<capability>`) this one needs; installed first.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Ordering only: run after these if they are selected, never pulls them in.
    #[serde(default)]
    pub after: Vec<String>,
    /// Capabilities this bundle provides.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Conditional requirements.
    #[serde(default)]
    pub requires_if: Vec<RequiresIf>,
    /// The whole bundle is skipped unless these facts match.
    pub when: Option<When>,
    /// Optional fast "already satisfied" command: when it exits 0 the bundle's steps are
    /// skipped (unless `--force`).
    pub check: Option<String>,
    /// Always included (when its `when:` matches), e.g. `common`.
    #[serde(default)]
    pub implicit: bool,
    /// Variables for this bundle: values, or declaration mappings.
    #[serde(default)]
    pub vars: OrderedMap<VarSpec>,
    /// Option defaults for this bundle's steps, over the profile's and `dot.yml`'s.
    pub defaults: Option<DefaultsFile>,
    /// What the bundle does, in order: each step is one directive (`link`, `package`,
    /// `template`, `shell`, ...) with optional `when`, `label`, `if`, `only_if`, `unless` and
    /// `notify`.
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Steps run once, after the bundle's steps, only when a step that names them in `notify:`
    /// changed something: name -> step (e.g. `restart-sshd: { service: { name: ssh, restart:
    /// true } }`). In the order written.
    #[serde(default)]
    pub handlers: OrderedMap<Step>,
}

/// `defaults:` in `dot.yml`, a profile, or a bundle.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefaultsFile {
    /// Defaults for `link` steps.
    #[serde(default)]
    pub link: LinkDefaultsFile,
    /// Defaults for `shell` steps.
    #[serde(default)]
    pub shell: ShellDefaultsFile,
}

/// Option defaults for `link` steps; a step's own options win.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkDefaultsFile {
    /// Create missing parent directories of the link.
    pub create: Option<bool>,
    /// Replace a symlink that points somewhere else.
    pub relink: Option<bool>,
    /// What to do when a file or directory (not a link) is in the way.
    pub on_conflict: Option<OnConflict>,
}

/// Option defaults for `shell` steps; a step's own options win.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellDefaultsFile {
    /// Give the command the terminal (stdin) instead of running it with no input.
    pub interactive: Option<bool>,
    /// The program `shell` steps (and `unless:` checks) run through, unless a step names one.
    pub interpreter: Option<ShellInterpreter>,
    /// Script lines run before every `shell` step's `cmd`, `unless` and `upgrade` (e.g.
    /// `set +eu; . "$SDKMAN_DIR/bin/sdkman-init.sh"`), unless a step sets its own.
    pub prelude: Option<String>,
}

/// The program a `shell` step's command runs through. Unset: `sh` on Unix; `pwsh` on Windows
/// when installed, else `powershell`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ShellInterpreter {
    /// `sh -c`
    Sh,
    /// `bash -c`
    Bash,
    /// `zsh -c`
    Zsh,
    /// PowerShell 7; multi-line scripts are fine.
    Pwsh,
    /// Windows PowerShell 5.1 (Windows only).
    Powershell,
    /// `cmd.exe /C` (Windows only; one line).
    Cmd,
}

impl ShellInterpreter {
    pub fn as_str(self) -> &'static str {
        match self {
            ShellInterpreter::Sh => "sh",
            ShellInterpreter::Bash => "bash",
            ShellInterpreter::Zsh => "zsh",
            ShellInterpreter::Pwsh => "pwsh",
            ShellInterpreter::Powershell => "powershell",
            ShellInterpreter::Cmd => "cmd",
        }
    }
}

impl DefaultsFile {
    pub fn to_defaults(&self) -> super::Defaults {
        super::Defaults {
            link: super::LinkDefaults {
                create: self.link.create,
                relink: self.link.relink,
                on_conflict: self.link.on_conflict,
            },
            shell: super::ShellDefaults {
                interactive: self.shell.interactive,
                interpreter: self.shell.interpreter,
                prelude: self.shell.prelude.clone(),
            },
        }
    }
}

/// A requirement that applies only on matching machines.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequiresIf {
    /// The facts that must match.
    pub when: When,
    /// Bundles (or `cap:<capability>`) required when they do.
    pub requires: Vec<String>,
}

/// Map of fact name to one or more accepted values. All keys must match (AND); any listed value
/// matches (OR).
#[derive(Debug, Clone, PartialEq)]
pub struct When(pub BTreeMap<Fact, Vec<String>>);

impl Check for BTreeMap<Fact, OneOrMany> {
    fn check(&self) -> Result<(), String> {
        for (fact, values) in self {
            // A templated value is checked after it is rendered, at plan time.
            for v in values.0.iter().filter(|v| !is_template(v)) {
                fact.check_value(v)?;
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for When {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let Checked(map) = Checked::<BTreeMap<Fact, OneOrMany>>::deserialize(d)?;
        Ok(When(map.into_iter().map(|(k, v)| (k, v.0)).collect()))
    }
}

impl JsonSchema for When {
    fn schema_name() -> Cow<'static, str> {
        "When".into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        let mut props = serde_json::Map::new();
        for fact in Fact::ALL {
            let description = fact_description(fact);
            let mut schema = match fact.known_values() {
                Some(known) => {
                    // Each known value, `!value` to exclude it, and `none` for `env`.
                    let mut values: Vec<String> = known.iter().map(|v| v.to_string()).collect();
                    if fact == Fact::Env {
                        values.push(crate::facts::ENV_NONE.to_string());
                    }
                    let negated: Vec<String> = values.iter().map(|v| format!("!{v}")).collect();
                    values.extend(negated);
                    let value = json_schema!({
                        "anyOf": [
                            { "enum": values },
                            { "type": "string", "pattern": TEMPLATE_PATTERN }
                        ]
                    });
                    json_schema!({
                        "anyOf": [
                            value,
                            { "type": "array", "items": value, "minItems": 1 }
                        ]
                    })
                }
                None => g.subschema_for::<OneOrMany>(),
            };
            schema.insert("description".into(), description.into());
            props.insert(fact.as_str().to_string(), schema.into());
        }
        json_schema!({
            "description": "Fact name -> accepted value(s). All keys must match; any listed value matches.",
            "type": "object",
            "properties": props,
            "additionalProperties": false
        })
    }
}

/// What each fact means, for the JSON Schema (`dot facts` prints this machine's).
fn fact_description(fact: Fact) -> &'static str {
    match fact {
        Fact::Os => "Operating system: `linux`, `macos` or `windows`.",
        Fact::Arch => "CPU architecture: `x86_64` or `aarch64`.",
        Fact::Distro => {
            "Linux distribution (`ID` in /etc/os-release), e.g. `ubuntu`, `debian`, `fedora`."
        }
        Fact::Family => {
            "Distribution family (`ID_LIKE` in /etc/os-release), e.g. `debian`, `rhel`, `arch`, \
             `alpine`."
        }
        Fact::Version => {
            "OS version: exact (`24.04`) or comparisons that must all hold (`>=22.04, <25`), \
             compared numerically."
        }
        Fact::Codename => "Release codename (`VERSION_CODENAME` in /etc/os-release), e.g. `noble`.",
        Fact::Role => {
            "`desktop` or `server`: pinned locally (`DOT_ROLE`), else `desktop` when a graphical \
             session or display manager is found."
        }
        Fact::Env => {
            "Special environment: `wsl`, `container`, `vm`, `ssh`, or `none` for none of them. \
             `!value` (quoted) excludes one."
        }
        Fact::Pkg => "The package manager in use, e.g. `apt`, `brew`, `dnf`, `winget`, `scoop`.",
        Fact::Init => "Init system: `systemd`, `launchd` or `other`.",
    }
}

/// One step: common keys plus exactly one directive key.
pub type Step = Checked<StepFields>;

/// The keys of a step. Exactly one directive key (`link`, `create`, `shell`, `clean`) is allowed;
/// that is checked while parsing so the error carries the step's line. The JSON Schema is written
/// by hand (below) so that editors enforce the same rule.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepFields {
    /// Skip this step unless these facts match.
    pub when: Option<When>,
    /// Name shown in output instead of the generated one.
    pub label: Option<String>,
    /// Handlers (the bundle's `handlers:`) to run at the end of the bundle when this step
    /// changes something.
    #[serde(default)]
    pub notify: Option<OneOrMany>,
    /// Skip this step unless this expression over variables is true (Jinja, without braces):
    /// `godot_mono`, `client_id != ""`.
    #[serde(rename = "if")]
    pub condition: Option<String>,
    /// Apply this step only when this command exits 0 (e.g. no IDE is running), asked only
    /// when the step has something to do; otherwise it is skipped.
    pub only_if: Option<String>,
    /// Skip this step when this command exits 0 (e.g. `command -v docker`), asked only when
    /// the step has something to do. On a `shell` step: the command's own `unless`.
    pub unless: Option<String>,
    /// Symlinks: destination -> source (relative to the bundle directory).
    pub link: Option<OrderedMap<ScalarOr<LinkOpts>>>,
    /// Directories to create.
    pub create: Option<ListOrMap<CreateOpts>>,
    /// A command run through the platform shell.
    pub shell: Option<ScalarOr<ShellOpts>>,
    /// Directories to remove dangling repo symlinks from.
    pub clean: Option<ListOrMap<CleanOpts>>,
    /// Packages to install: a list of names (mapped through `dot.yml`'s `packages:` for this
    /// machine's package manager), or a mapping of package manager to names.
    pub package: Option<PackageField>,
    /// Executables from a GitHub repository's latest release.
    pub github_release: Option<super::network::GithubReleaseField>,
    /// A git checkout, cloned over https and kept up to date.
    pub git: Option<super::network::GitField>,
    /// A file from an https URL: saved, unpacked, or installed as a package.
    pub download: Option<super::network::DownloadField>,
    // File and system directives (see `filesys`).
    pub template: Option<filesys::TemplateField>,
    pub copy: Option<filesys::CopyField>,
    pub secret_file: Option<filesys::SecretFileField>,
    pub block: Option<filesys::BlockField>,
    pub path: Option<filesys::PathField>,
    pub env: Option<filesys::EnvMap>,
    pub macos_defaults: Option<filesys::MacosDefaultsField>,
    pub service: Option<filesys::ServiceField>,
    // System directives (see `system`).
    pub package_repo: Option<system::PackageRepoField>,
    pub user: Option<system::UserField>,
    pub absent: Option<system::AbsentField>,
}

/// What a `link` does when a real file or directory is where the link goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OnConflict {
    /// Move the existing file or directory to `<dest>.dot-backup` (numbered if taken).
    #[default]
    Backup,
    /// Delete the existing file or directory.
    Overwrite,
    /// Fail the step.
    Fail,
}

/// Options left out stay `None` here and in the checked model: they are filled from the inherited
/// defaults (repo, profile, bundle, then built in) at plan time.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkOpts {
    /// Source path, relative to the bundle directory.
    pub src: String,
    /// Create missing parent directories of the destination (default: true).
    pub create: Option<bool>,
    /// Replace a symlink that points somewhere else (default: true).
    pub relink: Option<bool>,
    /// What to do when a real file or directory is in the way (default: backup).
    pub on_conflict: Option<OnConflict>,
}

impl FromScalar for LinkOpts {
    fn from_scalar(src: String) -> Self {
        Self {
            src,
            create: None,
            relink: None,
            on_conflict: None,
        }
    }
}

/// `create:` options for one path.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateOpts {
    /// Mode for the directory (Unix only), e.g. "0700".
    pub mode: Option<ModeSpec>,
    /// A root-owned directory (`/opt/...`): made with `sudo install -d -o root` (default false).
    #[serde(default)]
    pub root: bool,
    /// An empty file instead of a directory, made only when missing: its content is never
    /// touched (`~/.local.zshrc`, sourced by a managed rc file). `dot remove` removes it only
    /// while it is still empty (default false).
    #[serde(default)]
    pub file: bool,
}

/// `shell:` as a mapping (a plain string is just `cmd`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellOpts {
    /// The command (a script: several lines are fine), with `{{ variables }}` rendered.
    pub cmd: String,
    /// Working directory; relative paths are from the bundle directory (the default).
    pub cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: OrderedMap<String>,
    /// Hand the terminal to the command (prompts, sudo) instead of capturing its output
    /// (default: false).
    pub interactive: Option<bool>,
    /// Skip the step when this command exits 0.
    pub unless: Option<String>,
    /// Under `dot apply --upgrade`, run this instead of skipping when `unless` succeeds (the
    /// tool is installed): e.g. `rustup update` for a `curl | sh` installer. Needs `unless`.
    pub upgrade: Option<String>,
    /// Sensitive variables to export to the command anyway (others are withheld).
    #[serde(default)]
    pub export_sensitive: Vec<String>,
    /// The command uses `sudo`: the run asks for the password once, up front, and keeps it
    /// fresh, instead of the command prompting in the middle of the run (default: false).
    #[serde(default)]
    pub sudo: bool,
    /// The program the command (and `unless`) runs through; default from `defaults:`, else the
    /// platform's (`sh`; `pwsh` or `powershell` on Windows).
    pub interpreter: Option<ShellInterpreter>,
    /// Script lines run before `cmd`, `unless` and `upgrade` (default from `defaults: { shell:
    /// { prelude } }`): sourcing an init script that defines a shell function (SDKMAN's `sdk`).
    pub prelude: Option<String>,
}

impl FromScalar for ShellOpts {
    fn from_scalar(cmd: String) -> Self {
        Self {
            cmd,
            cwd: None,
            env: OrderedMap::default(),
            interactive: None,
            unless: None,
            upgrade: None,
            export_sensitive: Vec::new(),
            sudo: false,
            interpreter: None,
            prelude: None,
        }
    }
}

/// `clean:` options for one directory.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanOpts {
    /// Also clean subdirectories (symlinked directories are never entered).
    #[serde(default)]
    pub recursive: bool,
}

impl StepFields {
    /// The directive keys that are set.
    pub fn directive_keys(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if self.link.is_some() {
            keys.push("link");
        }
        if self.create.is_some() {
            keys.push("create");
        }
        if self.shell.is_some() {
            keys.push("shell");
        }
        if self.clean.is_some() {
            keys.push("clean");
        }
        if self.package.is_some() {
            keys.push("package");
        }
        if self.github_release.is_some() {
            keys.push("github_release");
        }
        if self.git.is_some() {
            keys.push("git");
        }
        if self.download.is_some() {
            keys.push("download");
        }
        keys.extend(filesys::directive_keys(self));
        keys.extend(system::directive_keys(self));
        keys
    }
}

impl Check for StepFields {
    fn check(&self) -> Result<(), String> {
        if self.unless.is_some() && self.only_if.is_some() {
            return Err("a step takes `only_if` or `unless`, not both".to_string());
        }
        if self.unless.is_some() && self.shell.as_ref().is_some_and(|s| s.0.unless.is_some()) {
            return Err(
                "`unless` is given twice: on the step and in `shell:`; keep one".to_string(),
            );
        }
        match self.directive_keys().as_slice() {
            [_] => self.check_targets(),
            [] => Err(format!(
                "a step needs exactly one directive ({}), found none",
                DIRECTIVES.join(", ")
            )),
            many => Err(format!(
                "a step needs exactly one directive, found {}: {}; split it into separate steps",
                many.len(),
                many.join(", ")
            )),
        }
    }
}

impl StepFields {
    /// Destinations must not depend on the working directory.
    fn check_targets(&self) -> Result<(), String> {
        if let Some(r) = filesys::check_targets(self) {
            return r;
        }
        if let Some(r) = system::check_targets(self) {
            return r;
        }
        let (kind, paths): (&str, Vec<&str>) = if let Some(l) = &self.link {
            ("link", l.0.iter().map(|(k, _)| k.as_str()).collect())
        } else if let Some(c) = &self.create {
            ("create", c.0.iter().map(|(k, _)| k.as_str()).collect())
        } else if let Some(c) = &self.clean {
            ("clean", c.0.iter().map(|(k, _)| k.as_str()).collect())
        } else {
            return Ok(());
        };
        // Templated paths are checked after rendering, at plan time.
        match paths.iter().find(|p| !is_template(p) && !is_target_path(p)) {
            Some(p) => Err(format!(
                "{kind}: `{p}` must be an absolute path or start with `~/`"
            )),
            None => Ok(()),
        }
    }
}

/// Whether `p` is a destination that does not depend on the working directory, on any OS: what
/// [`TARGET_PATTERN`] accepts short of a template. `~`, `~/...`, a Unix absolute path, a UNC path
/// or a drive path, whichever OS parses it: a Linux step (`/usr/local/go`, under a `when:`) must
/// still parse on Windows, and a Windows one (`C:\\Tools`) on Linux. Whether the path is absolute
/// on this machine is checked when a step that runs here is planned ([`crate::paths::expand_target`]).
pub(super) fn is_target_path(p: &str) -> bool {
    let b = p.as_bytes();
    let drive =
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\');
    p == "~"
        || p.starts_with("~/")
        || p.starts_with("~\\")
        || p.starts_with('/')
        || p.starts_with(r"\\")
        || drive
        || std::path::Path::new(p).is_absolute()
}

/// The directive keys a step may use.
pub const DIRECTIVES: [&str; 19] = [
    "link",
    "create",
    "shell",
    "clean",
    "package",
    "github_release",
    "git",
    "download",
    "template",
    "copy",
    "secret_file",
    "block",
    "path",
    "env",
    "macos_defaults",
    "service",
    "package_repo",
    "user",
    "absent",
];

/// Matches a value that is rendered before it is validated.
pub const TEMPLATE_PATTERN: &str = r"\{\{|\{%";

/// Matches a destination path the parser accepts: `~`, `~/...`, an absolute Unix or Windows path,
/// or a template.
pub const TARGET_PATTERN: &str = r"^(~$|~[/\\]|/|\\\\|[A-Za-z]:[/\\])|\{\{|\{%";

impl JsonSchema for StepFields {
    fn schema_name() -> Cow<'static, str> {
        "Step".into()
    }

    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        let target = json_schema!({ "type": "string", "pattern": TARGET_PATTERN });
        let path_list_or_map = |opts: Schema| {
            json_schema!({
                "anyOf": [
                    { "type": "array", "items": target },
                    {
                        "type": "object",
                        "propertyNames": target,
                        "additionalProperties": opts
                    }
                ]
            })
        };
        let link = json_schema!({
            "description": "Symlinks: destination -> source (relative to the bundle directory).",
            "type": "object",
            "propertyNames": target,
            "additionalProperties": g.subschema_for::<ScalarOr<LinkOpts>>()
        });
        let mut create = path_list_or_map(g.subschema_for::<Option<CreateOpts>>());
        create.insert(
            "description".into(),
            "Directories (or, with `file: true`, empty files) to create.".into(),
        );
        let mut clean = path_list_or_map(g.subschema_for::<Option<CleanOpts>>());
        clean.insert(
            "description".into(),
            "Directories to remove dangling repo symlinks from.".into(),
        );
        let mut shell = g.subschema_for::<ScalarOr<ShellOpts>>();
        shell.insert(
            "description".into(),
            "A command run through the platform shell.".into(),
        );
        let mut when = g.subschema_for::<When>();
        when.insert(
            "description".into(),
            "Skip this step unless these facts match.".into(),
        );
        let one_of: Vec<serde_json::Value> = DIRECTIVES
            .iter()
            .map(|d| serde_json::json!({ "required": [d] }))
            .collect();
        let mut schema = json_schema!({
            "description": "One step: optional `when` / `label` and exactly one directive key.",
            "type": "object",
            "properties": {
                "when": when,
                "label": { "type": "string", "description": "Name shown in output instead of the generated one." },
                "if": { "type": "string", "description": "Skip this step unless this expression over variables is true (Jinja, without braces): `godot_mono`, `client_id != \"\"`." },
                "only_if": { "type": "string", "description": "Apply this step only when this command exits 0; asked only when the step has something to do, otherwise the step is skipped." },
                "unless": { "type": "string", "description": "Skip this step when this command exits 0; asked only when the step has something to do. On a `shell` step: the command's own `unless`." },
                "notify": {
                    "description": "Handlers (the bundle's `handlers:`) to run at the end of the bundle when this step changes something.",
                    "anyOf": [
                        { "type": "string" },
                        { "type": "array", "items": { "type": "string" }, "minItems": 1 }
                    ]
                },
                "link": link,
                "create": create,
                "shell": shell,
                "clean": clean,
                "package": package_schema(),
                "github_release": g.subschema_for::<super::network::GithubReleaseField>(),
                "git": super::network::git_schema(g),
                "download": super::network::download_schema(g)
            },
            "additionalProperties": false,
            "oneOf": one_of
        });
        if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
            for (key, s) in filesys::schema_properties(g)
                .into_iter()
                .chain(system::schema_properties(g))
            {
                props.insert(key.into(), s.into());
            }
        }
        schema
    }
}
