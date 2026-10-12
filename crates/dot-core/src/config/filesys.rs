//! File and system directives: `template`, `copy`, `secret_file`, `block`, `path`, `env`,
//! `macos_defaults` and `service`. Their on-disk shapes (which also generate the JSON Schema),
//! the checks run while parsing, and the validated model the planners read. [`super::raw`] and
//! [`super`] only hook these in.

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, de::value::MapAccessDeserializer};

use super::raw::{StepFields, TARGET_PATTERN, is_target_path};
use super::shapes::{Check, Checked, FromScalar, ListOrMap, ModeSpec, OrderedMap, ScalarOr};
use super::{Bundle, ConfigError, Directive, Step};
use crate::runenv::PathPosition;
use crate::template::is_template;

// ---------------------------------------------------------------------------------------------
// On-disk shapes

/// `template:` options for one destination.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TemplateOpts {
    /// Template source, relative to the bundle directory. Rendered with the same variables as
    /// the rest of the bundle; `secret()` is allowed.
    pub src: String,
    /// File mode (Unix), e.g. "0644". Default: an existing file keeps its mode, a new one gets
    /// "0644"; "0600" when the template uses a secret. A template that uses a secret refuses a
    /// mode readable by group or others.
    pub mode: Option<ModeSpec>,
    /// A root-owned file (`/etc/...`): written through `sudo install -o root` (default false).
    #[serde(default)]
    pub root: bool,
}

impl FromScalar for TemplateOpts {
    fn from_scalar(src: String) -> Self {
        Self {
            src,
            mode: None,
            root: false,
        }
    }
}

/// `copy:` options for one destination.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CopyOpts {
    /// Source file or directory, relative to the bundle directory.
    pub src: String,
    /// File mode (Unix) for the copies. Default: the source's.
    pub mode: Option<ModeSpec>,
    /// Root-owned copies (`/etc/...`): written through `sudo install -o root` (default false).
    #[serde(default)]
    pub root: bool,
}

impl FromScalar for CopyOpts {
    fn from_scalar(src: String) -> Self {
        Self {
            src,
            mode: None,
            root: false,
        }
    }
}

/// `secret_file:` options for one destination.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretFileOpts {
    /// Logical secret name (see `secrets.yml`); its value is written as the file's content.
    pub secret: String,
    /// File mode (Unix); default "0600". Modes readable by group or others are refused unless
    /// `allow_loose_mode` is set.
    pub mode: Option<ModeSpec>,
    /// Accept a mode readable by group or others (default: false).
    #[serde(default)]
    pub allow_loose_mode: bool,
}

impl FromScalar for SecretFileOpts {
    fn from_scalar(secret: String) -> Self {
        Self {
            secret,
            mode: None,
            allow_loose_mode: false,
        }
    }
}

/// `block:` — a managed block of lines in a file `dot` does not own (`~/.bashrc`, ...).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlockOpts {
    /// The file (created if absent).
    pub file: String,
    /// The block's lines (a template; `secret()` only in a file readable by its owner alone).
    pub content: String,
    /// Tells several blocks of one bundle in the same file apart: letters, digits, `.`, `_`,
    /// `-`.
    pub marker: Option<String>,
    /// Comment prefix for the marker lines (default "#").
    pub comment: Option<String>,
    /// The file is root-owned (`/etc/hosts`): read with sudo when needed, written through
    /// `sudo install -o root` (default false).
    #[serde(default)]
    pub root: bool,
}

impl Check for BlockOpts {
    fn check(&self) -> Result<(), String> {
        if !is_template(&self.file) && !is_target_path(&self.file) {
            return Err(format!(
                "block: file `{}` must be an absolute path or start with `~/`",
                self.file
            ));
        }
        if let Some(m) = &self.marker {
            check_marker(m)?;
        }
        if let Some(c) = &self.comment
            && (c.trim().is_empty() || c.contains(['\n', '\r']))
        {
            return Err("block: comment must be a non-empty, single-line prefix".into());
        }
        Ok(())
    }
}

fn check_marker(m: &str) -> Result<(), String> {
    let ok = !m.is_empty()
        && m.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "block: marker `{m}` may only use letters, digits, `.`, `_` and `-`"
        ))
    }
}

/// `path:` options for one directory.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathOpts {
    /// `prepend` (default: in front of the existing PATH) or `append`.
    pub position: Option<PathPosition>,
    /// `user`: on Windows, also add the directory to the user `Path` in the registry
    /// (`HKCU\Environment`), so programs started outside a `dot env` shell see it; `dot
    /// remove` takes it out again. Nothing to persist on other systems.
    pub persist: Option<PathPersist>,
}

/// Where a `path:` entry is persisted besides `dot`'s own record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathPersist {
    /// The Windows user `Path` (`HKCU\Environment`).
    User,
}

impl JsonSchema for PathPosition {
    fn schema_name() -> Cow<'static, str> {
        "PathPosition".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "enum": ["prepend", "append"] })
    }
}

/// `env:` — variable name to value (a template; secrets are not allowed).
#[derive(Debug, Clone, PartialEq)]
pub struct EnvMap(pub OrderedMap<String>);

impl<'de> Deserialize<'de> for EnvMap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let map = OrderedMap::<String>::deserialize(d)?;
        for (name, _) in &map.0 {
            if !is_env_name(name) {
                return Err(de::Error::custom(format!(
                    "env: `{name}` is not a valid environment variable name"
                )));
            }
            if name.eq_ignore_ascii_case("PATH") {
                return Err(de::Error::custom(
                    "env: set PATH with the `path:` directive, which adds to it",
                ));
            }
        }
        if map.0.is_empty() {
            return Err(de::Error::custom("env: expected at least one variable"));
        }
        Ok(EnvMap(map))
    }
}

impl JsonSchema for EnvMap {
    fn schema_name() -> Cow<'static, str> {
        "EnvMap".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Environment variables for the later steps of the run (and `dot env --shell`).",
            "type": "object",
            "propertyNames": { "pattern": "^[A-Za-z_][A-Za-z0-9_]*$", "not": { "pattern": "^[Pp][Aa][Tt][Hh]$" } },
            "additionalProperties": { "type": "string" },
            "minProperties": 1
        })
    }
}

fn is_env_name(n: &str) -> bool {
    !n.is_empty()
        && !n.starts_with(|c: char| c.is_ascii_digit())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A mapping, or a list of mappings.
#[derive(Debug, Clone, PartialEq)]
pub struct OneOrList<T>(pub Vec<T>);

impl<'de, T: DeserializeOwned> Deserialize<'de> for OneOrList<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_<T>(PhantomData<T>);
        impl<'de, T: DeserializeOwned> Visitor<'de> for V_<T> {
            type Value = OneOrList<T>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a mapping or a list of mappings")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(|t| OneOrList(vec![t]))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(t) = seq.next_element::<T>()? {
                    out.push(t);
                }
                if out.is_empty() {
                    return Err(de::Error::custom("expected at least one entry"));
                }
                Ok(OneOrList(out))
            }
        }
        d.deserialize_any(V_(PhantomData))
    }
}

impl<T: JsonSchema> JsonSchema for OneOrList<T> {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        format!("One_or_list_of_{}", T::schema_name()).into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        let one = g.subschema_for::<T>();
        json_schema!({ "anyOf": [ one, { "type": "array", "items": one, "minItems": 1 } ] })
    }
}

/// The value type of a macOS preference, as `defaults write -<type>` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DefaultsType {
    String,
    Int,
    Float,
    Bool,
}

impl DefaultsType {
    pub fn as_str(self) -> &'static str {
        match self {
            DefaultsType::String => "string",
            DefaultsType::Int => "int",
            DefaultsType::Float => "float",
            DefaultsType::Bool => "bool",
        }
    }

    /// What `defaults read-type` calls it.
    pub fn read_type_name(self) -> &'static str {
        match self {
            DefaultsType::String => "string",
            DefaultsType::Int => "integer",
            DefaultsType::Float => "float",
            DefaultsType::Bool => "boolean",
        }
    }

    /// `value` in canonical form for this type (`true`/`false`, a decimal integer, a float), or
    /// why it does not fit.
    pub fn normalize(self, value: &str) -> Result<String, String> {
        let bad = || {
            Err(format!(
                "macos_defaults: `{value}` is not a valid {} value",
                self.as_str()
            ))
        };
        match self {
            DefaultsType::String => Ok(value.to_string()),
            DefaultsType::Bool => match value {
                "true" | "false" => Ok(value.to_string()),
                _ => bad(),
            },
            DefaultsType::Int => value
                .trim()
                .parse::<i64>()
                .map(|v| v.to_string())
                .or_else(|_| bad()),
            DefaultsType::Float => match value.trim().parse::<f64>() {
                Ok(v) if v.is_finite() => Ok(v.to_string()),
                _ => bad(),
            },
        }
    }
}

/// A scalar preference value: a string, number or boolean, kept as text.
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultsValue(pub String);

impl<'de> Deserialize<'de> for DefaultsValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_;
        impl Visitor<'_> for V_ {
            type Value = DefaultsValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string, number or boolean")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(DefaultsValue(v.to_string()))
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(DefaultsValue(v.to_string()))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(DefaultsValue(v.to_string()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(DefaultsValue(v.to_string()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
                Ok(DefaultsValue(v.to_string()))
            }
        }
        d.deserialize_any(V_)
    }
}

impl JsonSchema for DefaultsValue {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "DefaultsValue".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": ["string", "number", "integer", "boolean"] })
    }
}

/// One macOS preference (`defaults write <domain> <key> -<type> <value>`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MacosDefaultOpts {
    /// Preference domain, e.g. `com.apple.dock` or `NSGlobalDomain`.
    pub domain: String,
    /// The preference's key, e.g. `autohide`.
    pub key: String,
    /// The value's type: `string`, `int`, `float` or `bool` (`defaults write -<type>`).
    #[serde(rename = "type")]
    pub ty: DefaultsType,
    /// The value, of that type.
    pub value: DefaultsValue,
    /// Write the per-host preference (`defaults -currentHost`); default false.
    #[serde(default)]
    pub current_host: bool,
}

impl Check for MacosDefaultOpts {
    fn check(&self) -> Result<(), String> {
        if self.domain.trim().is_empty() || self.key.trim().is_empty() {
            return Err("macos_defaults: `domain` and `key` must not be empty".into());
        }
        if !is_template(&self.value.0) {
            self.ty.normalize(&self.value.0)?;
        }
        Ok(())
    }
}

/// Which service manager a `service:` step uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceManagerKind {
    /// systemd (system services) on Linux, launchd on macOS, the service control manager on
    /// Windows.
    #[default]
    Auto,
    Systemd,
    SystemdUser,
    Launchd,
    Windows,
}

impl ServiceManagerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceManagerKind::Auto => "auto",
            ServiceManagerKind::Systemd => "systemd",
            ServiceManagerKind::SystemdUser => "systemd-user",
            ServiceManagerKind::Launchd => "launchd",
            ServiceManagerKind::Windows => "windows",
        }
    }
}

/// `service:` — a service installed, enabled and running (or not).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceOpts {
    /// Unit name (systemd; `.service` is added when there is no suffix), job label (launchd) or
    /// service name (Windows).
    pub name: String,
    /// Default: auto.
    pub manager: Option<ServiceManagerKind>,
    /// Enabled at boot / login (default: true); false disables it.
    pub enable: Option<bool>,
    /// Running now (default: true); false stops it.
    pub start: Option<bool>,
    /// A templated unit file (systemd) or property list (launchd) to install, relative to the
    /// bundle directory.
    pub unit: Option<String>,
    /// systemd-user only: keep the user's services running without a login session
    /// (`loginctl enable-linger`); default false.
    pub linger: Option<bool>,
    /// Restart the service (when it is running) every time the step runs: for handlers
    /// (`handlers: { restart-sshd: { service: { name: ssh, restart: true } } }`), which run only
    /// when a step that notifies them changed something. Default false.
    #[serde(default)]
    pub restart: bool,
}

impl FromScalar for ServiceOpts {
    fn from_scalar(name: String) -> Self {
        Self {
            name,
            manager: None,
            enable: None,
            start: None,
            unit: None,
            linger: None,
            restart: false,
        }
    }
}

impl Check for ServiceOpts {
    fn check(&self) -> Result<(), String> {
        let name = self.name.trim();
        if name.is_empty() || name.contains(['/', '\\']) || name.chars().any(char::is_whitespace) {
            return Err(format!(
                "service: `{}` is not a valid service name",
                self.name
            ));
        }
        let manager = self.manager.unwrap_or_default();
        if self.linger == Some(true) && manager != ServiceManagerKind::SystemdUser {
            return Err("service: `linger` needs `manager: systemd-user`".into());
        }
        if self.unit.is_some() && manager == ServiceManagerKind::Windows {
            return Err("service: `unit` is not supported for Windows services".into());
        }
        if self.restart && self.start == Some(false) {
            return Err(
                "service: `restart: true` needs the service started (not `start: false`)".into(),
            );
        }
        Ok(())
    }
}

/// Checked in [`ServiceOpts::check`] whichever form was used.
#[derive(Debug, Clone)]
pub struct ServiceField(pub ServiceOpts);

impl<'de> Deserialize<'de> for ServiceField {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let ScalarOr(opts) = ScalarOr::<ServiceOpts>::deserialize(d)?;
        opts.check().map_err(de::Error::custom)?;
        Ok(ServiceField(opts))
    }
}

impl JsonSchema for ServiceField {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "Service".into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        g.subschema_for::<ScalarOr<ServiceOpts>>()
    }
}

// ---------------------------------------------------------------------------------------------
// Validated model

/// A `template:` or `copy:` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    /// Destination as written (`~/...` or absolute).
    pub dest: String,
    /// Source as written, relative to the bundle directory unless absolute.
    pub src: String,
    pub mode: Option<ModeSpec>,
    /// Root-owned: written through sudo.
    pub root: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SecretFileEntry {
    pub dest: String,
    pub secret: String,
    /// `None`: 0600.
    pub mode: Option<ModeSpec>,
    pub allow_loose_mode: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockSpec {
    pub file: String,
    pub content: String,
    pub marker: Option<String>,
    pub comment: String,
    /// Root-owned: read and written through sudo.
    pub root: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathDirEntry {
    pub dir: String,
    pub position: PathPosition,
    pub persist: Option<PathPersist>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MacosDefault {
    pub domain: String,
    pub key: String,
    pub ty: DefaultsType,
    /// As written (canonical unless templated).
    pub value: String,
    pub current_host: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceSpec {
    pub name: String,
    pub manager: ServiceManagerKind,
    pub enable: bool,
    pub start: bool,
    pub unit: Option<String>,
    pub linger: bool,
    /// Restart it (when running) whenever the step runs.
    pub restart: bool,
}

// ---------------------------------------------------------------------------------------------
// Hooks for the parser, the model and the schema

/// The directive keys this module adds.
pub const DIRECTIVES: [&str; 8] = [
    "template",
    "copy",
    "secret_file",
    "block",
    "path",
    "env",
    "macos_defaults",
    "service",
];

/// Which of this module's directive keys a step sets.
pub(super) fn directive_keys(s: &StepFields) -> Vec<&'static str> {
    let set = [
        s.template.is_some(),
        s.copy.is_some(),
        s.secret_file.is_some(),
        s.block.is_some(),
        s.path.is_some(),
        s.env.is_some(),
        s.macos_defaults.is_some(),
        s.service.is_some(),
    ];
    DIRECTIVES
        .iter()
        .zip(set)
        .filter_map(|(k, set)| set.then_some(*k))
        .collect()
}

/// Destination paths (the keys of `template`, `copy`, `secret_file`, `path`) must not depend on
/// the working directory. `None`: the step uses none of these directives.
pub(super) fn check_targets(s: &StepFields) -> Option<Result<(), String>> {
    let (kind, paths): (&str, Vec<&str>) = if let Some(m) = &s.template {
        ("template", m.0.iter().map(|(k, _)| k.as_str()).collect())
    } else if let Some(m) = &s.copy {
        ("copy", m.0.iter().map(|(k, _)| k.as_str()).collect())
    } else if let Some(m) = &s.secret_file {
        ("secret_file", m.0.iter().map(|(k, _)| k.as_str()).collect())
    } else if let Some(m) = &s.path {
        ("path", m.0.iter().map(|(k, _)| k.as_str()).collect())
    } else {
        return None;
    };
    Some(
        match paths.iter().find(|p| !is_template(p) && !is_target_path(p)) {
            Some(p) => Err(format!(
                "{kind}: `{p}` must be an absolute path or start with `~/`"
            )),
            None => Ok(()),
        },
    )
}

/// Takes this module's directive out of a checked step, if it has one.
pub(super) fn take_directive(s: &mut StepFields) -> Option<Directive> {
    if let Some(m) = s.template.take() {
        return Some(Directive::Template(
            m.0.into_iter()
                .map(|(dest, o)| FileEntry {
                    dest,
                    src: o.0.src,
                    mode: o.0.mode,
                    root: o.0.root,
                })
                .collect(),
        ));
    }
    if let Some(m) = s.copy.take() {
        return Some(Directive::Copy(
            m.0.into_iter()
                .map(|(dest, o)| FileEntry {
                    dest,
                    src: o.0.src,
                    mode: o.0.mode,
                    root: o.0.root,
                })
                .collect(),
        ));
    }
    if let Some(m) = s.secret_file.take() {
        return Some(Directive::SecretFile(
            m.0.into_iter()
                .map(|(dest, o)| SecretFileEntry {
                    dest,
                    secret: o.0.secret,
                    mode: o.0.mode,
                    allow_loose_mode: o.0.allow_loose_mode,
                })
                .collect(),
        ));
    }
    if let Some(Checked(b)) = s.block.take() {
        return Some(Directive::Block(BlockSpec {
            file: b.file,
            content: b.content,
            marker: b.marker,
            comment: b.comment.unwrap_or_else(|| "#".into()),
            root: b.root,
        }));
    }
    if let Some(p) = s.path.take() {
        return Some(Directive::Path(
            p.0.into_iter()
                .map(|(dir, o)| PathDirEntry {
                    dir,
                    position: o.position.unwrap_or_default(),
                    persist: o.persist,
                })
                .collect(),
        ));
    }
    if let Some(e) = s.env.take() {
        return Some(Directive::Env(e.0.0));
    }
    if let Some(d) = s.macos_defaults.take() {
        return Some(Directive::MacosDefaults(
            d.0.into_iter()
                .map(|Checked(o)| MacosDefault {
                    value: if is_template(&o.value.0) {
                        o.value.0
                    } else {
                        // Checked while parsing.
                        o.ty.normalize(&o.value.0).unwrap_or(o.value.0)
                    },
                    domain: o.domain,
                    key: o.key,
                    ty: o.ty,
                    current_host: o.current_host,
                })
                .collect(),
        ));
    }
    if let Some(ServiceField(o)) = s.service.take() {
        return Some(Directive::Service(ServiceSpec {
            name: o.name,
            manager: o.manager.unwrap_or_default(),
            enable: o.enable.unwrap_or(true),
            start: o.start.unwrap_or(true),
            unit: o.unit,
            linger: o.linger.unwrap_or(false),
            restart: o.restart,
        }));
    }
    None
}

/// The generated label of one of this module's directives.
pub(super) fn default_label(d: &Directive) -> Option<String> {
    fn list<'a>(kind: &str, items: impl Iterator<Item = &'a str>) -> String {
        let items: Vec<&str> = items.collect();
        match items.len() {
            0 => format!("{kind} (nothing)"),
            1..=3 => format!("{kind} {}", items.join(", ")),
            n => format!("{kind} {}, ... ({n} total)", items[..2].join(", ")),
        }
    }
    Some(match d {
        Directive::Template(e) => list("template", e.iter().map(|e| e.dest.as_str())),
        Directive::Copy(e) => list("copy", e.iter().map(|e| e.dest.as_str())),
        Directive::SecretFile(e) => list("secret_file", e.iter().map(|e| e.dest.as_str())),
        Directive::Block(b) => match &b.marker {
            Some(m) => format!("block {} ({m})", b.file),
            None => format!("block {}", b.file),
        },
        Directive::Path(e) => list("path", e.iter().map(|e| e.dir.as_str())),
        Directive::Env(e) => list("env", e.iter().map(|(k, _)| k.as_str())),
        Directive::MacosDefaults(e) => list("macos_defaults", e.iter().map(|d| d.key.as_str())),
        Directive::Service(s) => format!("service {}", s.name),
        _ => return None,
    })
}

/// Host-independent checks for `dot check`: template, copy and unit sources exist.
pub(super) fn lint_step(bundle: &Bundle, step: &Step, out: &mut Vec<ConfigError>) {
    let mut check = |what: &str, src: &str, dest: &str| {
        let path = bundle.dir.join(src);
        let checkable = !src.starts_with('~') && !is_template(src);
        if checkable && std::fs::symlink_metadata(&path).is_err() {
            out.push(ConfigError::at_step(
                bundle,
                step,
                format!(
                    "{what} source `{src}` for `{dest}` does not exist ({})",
                    path.display()
                ),
            ));
        }
    };
    match &step.directive {
        Directive::Template(e) => e.iter().for_each(|e| check("template", &e.src, &e.dest)),
        Directive::Copy(e) => e.iter().for_each(|e| check("copy", &e.src, &e.dest)),
        Directive::Service(s) => {
            if let Some(u) = &s.unit {
                check("service unit", u, &s.name);
            }
        }
        _ => {}
    }
}

/// Adds this module's directives to the step schema's `properties`.
pub(super) fn schema_properties(g: &mut SchemaGenerator) -> Vec<(&'static str, Schema)> {
    let target = json_schema!({ "type": "string", "pattern": TARGET_PATTERN });
    let target_map = |desc: &str, opts: Schema| {
        json_schema!({
            "description": desc,
            "type": "object",
            "propertyNames": target,
            "additionalProperties": opts
        })
    };
    let template = target_map(
        "Files rendered from templates: destination -> source (relative to the bundle directory).",
        g.subschema_for::<ScalarOr<TemplateOpts>>(),
    );
    let copy = target_map(
        "Files copied (not linked): destination -> source (relative to the bundle directory).",
        g.subschema_for::<ScalarOr<CopyOpts>>(),
    );
    let secret_file = target_map(
        "Files holding a secret: destination -> secret name.",
        g.subschema_for::<ScalarOr<SecretFileOpts>>(),
    );
    let mut block = g.subschema_for::<BlockOpts>();
    block.insert(
        "description".into(),
        "A managed block of lines in a file dot does not own.".into(),
    );
    let mut path = json_schema!({
        "anyOf": [
            { "type": "array", "items": target },
            {
                "type": "object",
                "propertyNames": target,
                "additionalProperties": g.subschema_for::<Option<PathOpts>>()
            }
        ]
    });
    path.insert(
        "description".into(),
        "Directories added to PATH for the later steps of the run (and `dot env --shell`).".into(),
    );
    let env = g.subschema_for::<EnvMap>();
    let mut macos = g.subschema_for::<OneOrList<MacosDefaultOpts>>();
    macos.insert(
        "description".into(),
        "macOS preferences, set with `defaults write` (macOS only).".into(),
    );
    let mut service = g.subschema_for::<ServiceField>();
    service.insert(
        "description".into(),
        "A service installed, enabled and running (systemd, launchd, Windows).".into(),
    );
    vec![
        ("template", template),
        ("copy", copy),
        ("secret_file", secret_file),
        ("block", block),
        ("path", path),
        ("env", env),
        ("macos_defaults", macos),
        ("service", service),
    ]
}

/// The step fields' types, as [`StepFields`] declares them.
pub type TemplateField = OrderedMap<ScalarOr<TemplateOpts>>;
pub type CopyField = OrderedMap<ScalarOr<CopyOpts>>;
pub type SecretFileField = OrderedMap<ScalarOr<SecretFileOpts>>;
pub type BlockField = Checked<BlockOpts>;
pub type PathField = ListOrMap<PathOpts>;
pub type MacosDefaultsField = OneOrList<Checked<MacosDefaultOpts>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Mode, parse_bundle};

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
  - template:
      ~/.config/glab/env: glab.env.j2
      ~/.gitconfig: { src: gitconfig.j2, mode: "0640" }
  - copy:
      ~/.ssh/config: ssh/config
  - secret_file:
      ~/.vpn/work.ovpn: vpn_profile
      ~/.netrc: { secret: netrc, mode: "0400" }
  - block:
      file: ~/.bashrc
      content: |
        eval "$(fnm env)"
      marker: fnm
      comment: "//"
  - path: [~/.local/bin, "{{ home }}/x"]
  - path:
      ~/.cargo/bin:
      /opt/late: { position: append, persist: user }
  - env: { EDITOR: nvim, PAGER: "less -R" }
  - macos_defaults: { domain: com.apple.dock, key: autohide, type: bool, value: true }
  - macos_defaults:
      - { domain: d, key: n, type: int, value: 7, current_host: true }
      - { domain: d, key: f, type: float, value: 1.50 }
  - service: sshd
  - service: { name: syncthing, manager: systemd-user, unit: s.service, linger: true, start: false }
"#;

    #[test]
    fn parses_every_shape() {
        let b = parse(ALL).unwrap();
        let d: Vec<&Directive> = b.steps.iter().map(|s| &s.directive).collect();
        let Directive::Template(t) = d[0] else {
            panic!()
        };
        assert_eq!(
            t[1],
            FileEntry {
                dest: "~/.gitconfig".into(),
                src: "gitconfig.j2".into(),
                mode: Some(ModeSpec::Literal(Mode(0o640))),
                root: false,
            }
        );
        let Directive::Copy(c) = d[1] else { panic!() };
        assert_eq!(
            (c[0].dest.as_str(), c[0].mode.is_none()),
            ("~/.ssh/config", true)
        );
        let Directive::SecretFile(s) = d[2] else {
            panic!()
        };
        assert_eq!(s[0].secret, "vpn_profile");
        assert_eq!(s[1].mode, Some(ModeSpec::Literal(Mode(0o400))));
        let Directive::Block(bl) = d[3] else { panic!() };
        assert_eq!(
            (bl.marker.as_deref(), bl.comment.as_str()),
            (Some("fnm"), "//")
        );
        let Directive::Path(p) = d[4] else { panic!() };
        assert_eq!(p[1].dir, "{{ home }}/x");
        let Directive::Path(p) = d[5] else { panic!() };
        assert_eq!(p[1].position, PathPosition::Append);
        assert_eq!(p[0].position, PathPosition::Prepend);
        assert_eq!(
            (p[0].persist, p[1].persist),
            (None, Some(PathPersist::User))
        );
        let Directive::Env(e) = d[6] else { panic!() };
        assert_eq!(e[1], ("PAGER".to_string(), "less -R".to_string()));
        let Directive::MacosDefaults(m) = d[7] else {
            panic!()
        };
        assert_eq!((m[0].value.as_str(), m[0].ty), ("true", DefaultsType::Bool));
        let Directive::MacosDefaults(m) = d[8] else {
            panic!()
        };
        assert_eq!((m[0].value.as_str(), m[0].current_host), ("7", true));
        assert_eq!(m[1].value, "1.5");
        let Directive::Service(sv) = d[9] else {
            panic!()
        };
        assert_eq!(
            (sv.name.as_str(), sv.manager, sv.enable, sv.start, sv.linger),
            ("sshd", ServiceManagerKind::Auto, true, true, false)
        );
        let Directive::Service(sv) = d[10] else {
            panic!()
        };
        assert_eq!(
            (sv.manager, sv.start, sv.linger, sv.unit.as_deref()),
            (
                ServiceManagerKind::SystemdUser,
                false,
                true,
                Some("s.service")
            )
        );
        let labels: Vec<String> = b.steps.iter().map(|s| s.display_label()).collect();
        assert_eq!(labels[0], "template ~/.config/glab/env, ~/.gitconfig");
        assert_eq!(labels[3], "block ~/.bashrc (fnm)");
        assert_eq!(labels[6], "env EDITOR, PAGER");
        assert_eq!(labels[9], "service sshd");
        let kinds: Vec<&str> = b.steps.iter().map(|s| s.directive.kind()).collect();
        assert_eq!(
            kinds,
            [
                "template",
                "copy",
                "secret_file",
                "block",
                "path",
                "path",
                "env",
                "macos_defaults",
                "macos_defaults",
                "service",
                "service"
            ]
        );
    }

    #[test]
    fn strict_errors_carry_file_and_line() {
        let cases = [
            (
                "  - template:\n      ~/x: { src: a, mod: \"0600\" }\n",
                3,
                "unknown field `mod`",
            ),
            (
                "  - template:\n      ~/x: { src: a, mode: 600 }\n",
                3,
                "quoted octal string",
            ),
            (
                "  - template:\n      rel/x: a\n",
                2,
                "must be an absolute path",
            ),
            (
                "  - copy:\n      ~/x: { from: a }\n",
                3,
                "unknown field `from`",
            ),
            (
                "  - secret_file:\n      ~/x: { secret: s, mode: \"0999\" }\n",
                3,
                "invalid file mode",
            ),
            (
                "  - block: { file: x, content: y }\n",
                2,
                "must be an absolute path",
            ),
            (
                "  - block: { file: ~/x, content: y, marker: 'a b' }\n",
                2,
                "marker `a b`",
            ),
            (
                "  - block: { file: ~/x, content: y, comment: '' }\n",
                2,
                "comment must be",
            ),
            ("  - block: { file: ~/x }\n", 2, "missing field `content`"),
            ("  - env: { A: [1] }\n", 2, "invalid type"),
            ("  - macos_defaults: []\n", 2, "at least one entry"),
            (
                "  - service: { name: x, manager: systemd, linger: true }\n",
                2,
                "`linger` needs",
            ),
            (
                "  - template: { ~/x: a }\n    copy: { ~/y: b }\n",
                2,
                "found 2: template, copy",
            ),
            (
                "  - path: [~/bin]\n    env: { A: b }\n",
                2,
                "found 2: path, env",
            ),
        ];
        for (step, line, want) in cases {
            let e = err(&format!("steps:\n{step}"));
            assert!(e.contains(want), "{step}: {e}");
            assert!(e.contains(&format!("bundle.yml:{line}:")), "{step}: {e}");
        }
    }

    #[test]
    fn schema_covers_the_new_directives() {
        let schema = serde_json::to_value(crate::config::json_schema()).unwrap();
        let step = &schema["$defs"]["Step"];
        for d in DIRECTIVES {
            assert!(step["properties"].get(d).is_some(), "schema lacks {d}");
        }
        for d in ["template", "copy", "secret_file"] {
            assert_eq!(
                step["properties"][d]["propertyNames"]["pattern"], TARGET_PATTERN,
                "{d}"
            );
        }
        let path = &step["properties"]["path"]["anyOf"];
        assert_eq!(path[0]["items"]["pattern"], TARGET_PATTERN);
        let text = schema.to_string();
        for word in [
            "systemd-user",
            "launchd",
            "prepend",
            "current_host",
            "allow_loose_mode",
        ] {
            assert!(text.contains(word), "schema lacks {word}");
        }
    }
}
