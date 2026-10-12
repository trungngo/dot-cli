//! Layered variables. Each layer is one source (facts, `dot.yml`, the profile, a bundle, a
//! `.env` file, the process environment, the command line); a higher layer wins. Every value
//! remembers where it came from and whether it is sensitive.
//!
//! Precedence, lowest first (see [`Rank`]):
//!
//! 1. facts (`os`, `distro`, `role`, `home`, the known folders `config_dir`, `appdata`, ...
//!    of [`crate::folders`], `windows_home` in WSL); `.env.example` adds declarations only
//!    (see [`example_layer`])
//! 2. `vars:` in `dot.yml`
//! 3. `vars:` in the profile
//! 4. `vars:` in `bundle.yml`
//! 5. `.env`
//! 6. `.env.<profile>`
//! 7. `.env.local`, `~/.config/dot/local.env`
//! 8. process environment: overrides a variable of the same name defined (or declared) below;
//!    it never adds new names, so `dot vars` does not list the whole environment
//! 9. `--var KEY=value`, `--env-file path`
//!
//! A `vars:` entry is either a value (string, bool, number, or a list of them) or a declaration
//! mapping (`type`, `required`, `pattern`, `default`, `sensitive`, `prompt`, `persist`,
//! `per_machine`, `description`). Declarations from all layers merge, the higher layer's keys
//! winning.
//!
//! Declared types are applied before templates see a value, so `{{ dir }}` of a `type: path`
//! variable is already `~`-expanded and a `type: bool` set to `no` is false. A variable built
//! from a sensitive one (by a template or by `.env` `${VAR}` interpolation) is sensitive too,
//! unless it is declared `sensitive: false`.
//!
//! A templated variable that cannot be rendered (an undefined name, a cycle) is left undefined
//! and reported by [`Vars::problems`]; a template that uses it fails with the reason. It does
//! not stop runs that never use it: a `dot.yml` variable may refer to one only some bundles
//! define.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, de::value::MapAccessDeserializer};
use serde_json::Value;

use crate::config::ConfigError;
use crate::facts::{Facts, Os};

/// Layer precedence, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rank {
    Facts,
    /// `.env.example`: declarations only, below every other declaration.
    Example,
    Repo,
    Profile,
    Bundle,
    DotEnv,
    DotEnvProfile,
    Local,
    Environment,
    Cli,
}

/// Where a value came from, e.g. `dot.yml`, `profile mac`, `.env.local`, `environment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub rank: Rank,
    pub name: String,
}

impl Source {
    pub fn new(rank: Rank, name: impl Into<String>) -> Self {
        Self {
            rank,
            name: name.into(),
        }
    }
    pub fn fact() -> Self {
        Self::new(Rank::Facts, "fact")
    }
    pub fn repo() -> Self {
        Self::new(Rank::Repo, "dot.yml")
    }
    pub fn profile(name: &str) -> Self {
        Self::new(Rank::Profile, format!("profile {name}"))
    }
    pub fn bundle(name: &str) -> Self {
        Self::new(Rank::Bundle, format!("bundle {name}"))
    }
    pub fn environment() -> Self {
        Self::new(Rank::Environment, "environment")
    }
    pub fn cli() -> Self {
        Self::new(Rank::Cli, "--var")
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

/// The type a variable's value is checked against (and converted to, in templates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum VarType {
    /// Any string (the default).
    String,
    /// `true` or `false`.
    Bool,
    /// A whole number.
    Int,
    /// A YAML list, or a comma-separated string (from `.env`).
    List,
    /// A string with `~` expanded to the home directory.
    Path,
}

impl VarType {
    fn as_str(self) -> &'static str {
        match self {
            VarType::String => "string",
            VarType::Bool => "bool",
            VarType::Int => "int",
            VarType::List => "list",
            VarType::Path => "path",
        }
    }
}

/// Where a prompted answer is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Persist {
    /// Written to `~/.config/dot/local.env` and asked once.
    Local,
}

/// A variable declaration: constraints, not a value (its `default` becomes the value of the
/// layer that declares it).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VarDecl {
    /// The value's type (default: string).
    #[serde(rename = "type")]
    pub ty: Option<VarType>,
    /// Missing (or empty) is an error before anything runs.
    pub required: Option<bool>,
    /// A regular expression the whole value must match.
    pub pattern: Option<String>,
    /// The value when no higher layer sets one.
    pub default: Option<VarLiteral>,
    /// Redacted in output and not exported to shell steps unless a step asks. Defaults to true
    /// for names that look like credentials (`*token*`, `*password*`, `*secret*`, ...).
    pub sensitive: Option<bool>,
    /// Question asked (before anything runs) when the variable has no value; a run that cannot
    /// ask fails instead, listing every such variable.
    pub prompt: Option<String>,
    /// Where a prompted answer is kept: `local` writes it to `~/.config/dot/local.env`, so it is
    /// asked once. Without it the answer lasts one run.
    pub persist: Option<Persist>,
    /// The value belongs to one machine (its name, its own disk): `dot apply --host` never
    /// copies this machine's value to another one, and asks for (or takes `--var`) the remote's
    /// own instead. Default false.
    pub per_machine: Option<bool>,
    /// Free text: what the variable is for.
    pub description: Option<String>,
}

impl VarDecl {
    /// `self` (the higher layer) over `lower`.
    fn over(&self, lower: &VarDecl) -> VarDecl {
        VarDecl {
            ty: self.ty.or(lower.ty),
            required: self.required.or(lower.required),
            pattern: self.pattern.clone().or_else(|| lower.pattern.clone()),
            default: None,
            sensitive: self.sensitive.or(lower.sensitive),
            prompt: self.prompt.clone().or_else(|| lower.prompt.clone()),
            persist: self.persist.or(lower.persist),
            per_machine: self.per_machine.or(lower.per_machine),
            description: self
                .description
                .clone()
                .or_else(|| lower.description.clone()),
        }
    }
}

/// A literal variable value from YAML: a scalar or a list of scalars.
#[derive(Debug, Clone, PartialEq)]
pub struct VarLiteral(pub Value);

impl<'de> Deserialize<'de> for VarLiteral {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(LiteralVisitor { allow_seq: true })
            .map(VarLiteral)
    }
}

impl JsonSchema for VarLiteral {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "VarLiteral".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [
                { "type": ["string", "boolean", "number"] },
                { "type": "array", "items": { "type": ["string", "boolean", "number"] } }
            ]
        })
    }
}

struct LiteralVisitor {
    allow_seq: bool,
}

impl<'de> Visitor<'de> for LiteralVisitor {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.allow_seq {
            f.write_str("a string, bool, number, or a list of them")
        } else {
            f.write_str("a string, bool or number")
        }
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_string()))
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Err(E::custom(
            "a variable needs a value; write \"\" for an empty string",
        ))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        if !self.allow_seq {
            return Err(de::Error::custom("lists cannot be nested"));
        }
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(ScalarSeed)? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
}

struct ScalarSeed;
impl<'de> de::DeserializeSeed<'de> for ScalarSeed {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(LiteralVisitor { allow_seq: false })
    }
}

/// One `vars:` entry: a value, or a declaration mapping.
#[derive(Debug, Clone, PartialEq)]
pub enum VarSpec {
    Value(Value),
    Decl(VarDecl),
}

impl<'de> Deserialize<'de> for VarSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_;
        impl<'de> Visitor<'de> for V_ {
            type Value = VarSpec;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a value (string, bool, number or list) or a declaration mapping")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<VarSpec, A::Error> {
                let decl = VarDecl::deserialize(MapAccessDeserializer::new(map))?;
                if let Some(p) = &decl.pattern {
                    compile_pattern(p).map_err(de::Error::custom)?;
                }
                Ok(VarSpec::Decl(decl))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<VarSpec, E> {
                LiteralVisitor { allow_seq: true }
                    .visit_str(v)
                    .map(VarSpec::Value)
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<VarSpec, E> {
                Ok(VarSpec::Value(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<VarSpec, E> {
                Ok(VarSpec::Value(Value::from(v)))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<VarSpec, E> {
                Ok(VarSpec::Value(Value::from(v)))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<VarSpec, E> {
                Ok(VarSpec::Value(Value::from(v)))
            }
            fn visit_unit<E: de::Error>(self) -> Result<VarSpec, E> {
                LiteralVisitor { allow_seq: true }
                    .visit_unit()
                    .map(VarSpec::Value)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<VarSpec, A::Error> {
                LiteralVisitor { allow_seq: true }
                    .visit_seq(seq)
                    .map(VarSpec::Value)
            }
        }
        d.deserialize_any(V_)
    }
}

impl JsonSchema for VarSpec {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        "VarSpec".into()
    }
    fn json_schema(g: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "anyOf": [ g.subschema_for::<VarLiteral>(), g.subschema_for::<VarDecl>() ]
        })
    }
}

fn compile_pattern(p: &str) -> Result<regex::Regex, String> {
    regex::Regex::new(&format!("^(?:{p})$")).map_err(|e| format!("invalid pattern `{p}`: {e}"))
}

/// One source's values and declarations.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub source: Source,
    pub values: Vec<(String, Value)>,
    pub decls: Vec<(String, VarDecl)>,
    /// For `.env` values built by `${VAR}` interpolation: the names each one used, so a value
    /// built from a sensitive one is sensitive too.
    pub refs: BTreeMap<String, BTreeSet<String>>,
}

impl Layer {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            values: Vec::new(),
            decls: Vec::new(),
            refs: BTreeMap::new(),
        }
    }

    /// A layer from a `vars:` section. A declaration's `default` becomes this layer's value.
    pub fn from_specs<'a>(
        source: Source,
        specs: impl IntoIterator<Item = (&'a String, &'a VarSpec)>,
    ) -> Self {
        let mut layer = Layer::new(source);
        for (name, spec) in specs {
            match spec {
                VarSpec::Value(v) => layer.values.push((name.clone(), v.clone())),
                VarSpec::Decl(d) => {
                    if let Some(def) = &d.default {
                        layer.values.push((name.clone(), def.0.clone()));
                    }
                    layer.decls.push((name.clone(), d.clone()));
                }
            }
        }
        layer
    }

    pub fn set(&mut self, name: impl Into<String>, value: Value) {
        let name = name.into();
        self.values.retain(|(k, _)| *k != name);
        self.values.push((name, value));
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.decls.is_empty()
    }
}

/// A variable after layering.
#[derive(Debug, Clone, PartialEq)]
pub struct VarValue {
    pub value: Value,
    pub source: Source,
    pub sensitive: bool,
}

impl VarValue {
    /// The value as a string: lists are space-separated, null is empty.
    pub fn as_string(&self) -> String {
        value_string(&self.value)
    }
}

/// The value as a string: lists are space-separated, null is empty.
pub fn value_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(items) => items.iter().map(value_string).collect::<Vec<_>>().join(" "),
        other => other.to_string(),
    }
}

/// Names that look like credentials are sensitive unless declared otherwise.
pub fn looks_sensitive(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "api_key",
        "private_key",
        "credential",
    ]
    .iter()
    .any(|w| n.contains(w))
}

/// Variables visible to templates: every layer, resolved.
#[derive(Debug, Clone, Default)]
pub struct Vars {
    layers: Vec<Layer>,
    /// Snapshot of the process environment (layer 8).
    environment: Option<Arc<BTreeMap<String, String>>>,
    home: Option<PathBuf>,
    resolved: BTreeMap<String, VarValue>,
    decls: BTreeMap<String, VarDecl>,
    /// The declared names in the order they were first declared (layer by layer, each layer in
    /// file order): prompts ask in this order.
    decl_order: Vec<String>,
    problems: Vec<String>,
    /// The subset of `problems` about values that do not fit their declaration.
    value_problems: Vec<String>,
    /// The resolved values as template values, built once per change. Variables that could not
    /// be rendered are left out.
    pub(crate) template_ctx: Arc<BTreeMap<String, minijinja::Value>>,
    /// Variables left undefined because their template could not be rendered, and why.
    pub(crate) unrendered: Arc<BTreeMap<String, String>>,
    /// Variables `dot apply` will ask for (declared with a `prompt:`, no value yet), set by
    /// [`Vars::defer_prompts`] for `dot plan`: they render as [`asked_placeholder`] instead of
    /// being undefined.
    asked: Arc<BTreeSet<String>>,
    /// `asked`, plus the variables whose value was rendered from one of them.
    asked_all: Arc<BTreeSet<String>>,
}

/// The known folders for `facts.os` as variable values (`none` where not applicable).
fn known_folder_values(
    facts: &Facts,
    home: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<(&'static str, Value)> {
    crate::folders::known_folders_on(
        facts.os,
        Os::host(),
        facts.arch,
        &home.display().to_string(),
        env,
    )
    .into_iter()
    .map(|(n, v)| (n, v.map(Value::from).unwrap_or(Value::Null)))
    .collect()
}

/// What a variable `dot apply` will ask for renders to while planning.
pub fn asked_placeholder(name: &str) -> String {
    format!("<asked:{name}>")
}

impl PartialEq for Vars {
    fn eq(&self, other: &Self) -> bool {
        self.resolved == other.resolved && self.decls == other.decls
    }
}

impl Vars {
    /// The fact layer: every fact (missing ones are `none`), `home`, the known folders (as
    /// derived from `home` alone; [`Vars::set_known_folders`] applies the environment) and
    /// `windows_home` (WSL; `none` elsewhere).
    pub fn from_facts(facts: &Facts, home: &Path) -> Self {
        let mut layer = Layer::new(Source::fact());
        for f in crate::facts::Fact::ALL {
            let v = facts.values(f);
            let value = match f {
                crate::facts::Fact::Env => Value::Array(v.iter().map(|s| (*s).into()).collect()),
                _ => v.first().map(|s| Value::from(*s)).unwrap_or(Value::Null),
            };
            layer.values.push((f.as_str().to_string(), value));
        }
        layer
            .values
            .push(("home".into(), home.display().to_string().into()));
        for (name, value) in known_folder_values(facts, home, |_| None) {
            layer.values.push((name.into(), value));
        }
        layer.values.push((
            "windows_home".into(),
            facts
                .windows_home
                .clone()
                .map(Value::from)
                .unwrap_or(Value::Null),
        ));
        let mut vars = Vars {
            home: Some(home.to_path_buf()),
            ..Vars::default()
        };
        vars.push_layer(layer);
        vars
    }

    /// Adds a layer (kept in rank order; among equal ranks, later layers win).
    pub fn push_layer(&mut self, layer: Layer) {
        let at = self
            .layers
            .iter()
            .position(|l| l.source.rank > layer.source.rank)
            .unwrap_or(self.layers.len());
        self.layers.insert(at, layer);
        self.recompute();
    }

    /// Planning only: `names` (unanswered prompts) render as [`asked_placeholder`], so a plan
    /// can be shown before they are asked; [`Vars::asked_in`] says which a text uses.
    pub fn defer_prompts(&mut self, names: &[String]) {
        self.asked = Arc::new(names.iter().cloned().collect());
        self.recompute();
    }

    /// The deferred prompt variables (see [`Vars::defer_prompts`]) that `text` uses, directly
    /// or through a variable rendered from one.
    pub fn asked_in(&self, text: &str) -> Vec<String> {
        if self.asked_all.is_empty() {
            return Vec::new();
        }
        crate::template::referenced_names(text)
            .into_iter()
            .filter(|n| self.asked_all.contains(n))
            .collect()
    }

    /// Sets the process-environment snapshot (layer 8).
    pub fn set_environment(&mut self, env: BTreeMap<String, String>) {
        self.environment = Some(Arc::new(env));
        self.recompute();
    }

    /// Sets the known-folder variables ([`crate::folders`]) from the environment `env` (e.g.
    /// `XDG_CONFIG_HOME`, `APPDATA`).
    pub fn set_known_folders(
        &mut self,
        facts: &Facts,
        home: &Path,
        env: impl Fn(&str) -> Option<String>,
    ) {
        for (name, value) in known_folder_values(facts, home, env) {
            self.set_fact(name, value);
        }
    }

    /// Sets a fact-level variable (e.g. `profile`).
    pub fn set_fact(&mut self, name: impl Into<String>, value: impl Into<Value>) {
        match self
            .layers
            .iter_mut()
            .find(|l| l.source.rank == Rank::Facts)
        {
            Some(l) => l.set(name, value.into()),
            None => {
                let mut l = Layer::new(Source::fact());
                l.set(name, value.into());
                self.layers.insert(0, l);
            }
        }
        self.recompute();
    }

    /// Sets a value in the command-line layer (as `--var` would).
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let value = Value::String(value.into());
        match self
            .layers
            .iter_mut()
            .rev()
            .find(|l| l.source == Source::cli())
        {
            Some(l) => l.set(name, value),
            None => {
                let mut l = Layer::new(Source::cli());
                l.set(name, value);
                self.layers.push(l);
            }
        }
        self.recompute();
    }

    /// These variables plus the bundle's own `vars:` layer.
    pub fn with_bundle(&self, bundle: &crate::config::Bundle) -> Vars {
        let layer = &bundle.manifest.vars;
        if layer.is_empty() {
            return self.clone();
        }
        let mut v = self.clone();
        v.push_layer(layer.clone());
        v
    }

    pub fn get(&self, name: &str) -> Option<&VarValue> {
        self.resolved.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &VarValue)> {
        self.resolved.iter()
    }

    pub fn decl(&self, name: &str) -> Option<&VarDecl> {
        self.decls.get(name)
    }

    /// Every declared variable with its merged declaration, in declaration order.
    pub fn declared(&self) -> impl Iterator<Item = (&str, &VarDecl)> {
        self.decl_order
            .iter()
            .filter_map(|n| self.decls.get(n).map(|d| (n.as_str(), d)))
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Declared-required variables with no value (or an empty one), sorted. Deferred prompts
    /// ([`Vars::defer_prompts`]) are not missing: they will be asked for.
    pub fn missing_required(&self) -> Vec<String> {
        self.decls
            .iter()
            .filter(|(_, d)| d.required == Some(true))
            .filter(|(n, _)| !self.asked.contains(*n))
            .filter(|(n, _)| {
                self.resolved.get(*n).is_none_or(|v| match &v.value {
                    Value::Null => true,
                    Value::String(s) => s.is_empty(),
                    _ => false,
                })
            })
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Declared variables with a `prompt:` and no value (unset, null or empty):
    /// `(name, prompt, sensitive, persist: local)`, in declaration order.
    pub fn unanswered_prompts(&self) -> Vec<(String, String, bool, bool)> {
        self.decl_order
            .iter()
            .filter_map(|name| self.decls.get(name).map(|d| (name, d)))
            .filter_map(|(name, d)| {
                let question = d.prompt.clone()?;
                let unset = self.resolved.get(name).is_none_or(|v| match &v.value {
                    Value::Null => true,
                    Value::String(s) => s.is_empty(),
                    _ => false,
                });
                unset.then(|| {
                    let sensitive = d.sensitive.unwrap_or_else(|| looks_sensitive(name));
                    (
                        name.clone(),
                        question,
                        sensitive,
                        d.persist == Some(Persist::Local),
                    )
                })
            })
            .collect()
    }

    /// Every problem: values that do not fit their declaration (wrong type, pattern mismatch)
    /// and templated values that could not be rendered.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// Only the values that do not fit their declaration. These fail a run up front; a value
    /// that could not be rendered fails only the steps that use it.
    pub fn value_problems(&self) -> &[String] {
        &self.value_problems
    }

    /// Why a variable is undefined although some layer sets it (its template failed).
    pub fn unrendered(&self, name: &str) -> Option<&str> {
        self.unrendered.get(name).map(String::as_str)
    }

    /// The environment a shell step receives: every variable from `dot.yml`, the profile, the
    /// bundle, `.env` files and the command line (not facts, not the process environment, which
    /// the child inherits anyway), except sensitive ones not listed in `allow_sensitive`. Names
    /// that are not valid environment variable names are left out.
    pub fn exported(&self, allow_sensitive: &[String]) -> Result<Vec<(String, String)>, String> {
        for name in allow_sensitive {
            if !self.resolved.contains_key(name) {
                return Err(format!("export_sensitive: `{name}` is not a variable"));
            }
        }
        Ok(self
            .resolved
            .iter()
            .filter(|(_, v)| !matches!(v.source.rank, Rank::Facts | Rank::Environment))
            .filter(|(n, v)| !v.sensitive || allow_sensitive.contains(n))
            .filter(|(n, _)| is_env_name(n))
            .map(|(n, v)| (n.clone(), v.as_string()))
            .collect())
    }

    fn recompute(&mut self) {
        let mut decls: BTreeMap<String, VarDecl> = BTreeMap::new();
        let mut decl_order = Vec::new();
        for layer in &self.layers {
            for (name, d) in &layer.decls {
                if !decls.contains_key(name) {
                    decl_order.push(name.clone());
                }
                let merged = d.over(decls.get(name).unwrap_or(&VarDecl::default()));
                decls.insert(name.clone(), merged);
            }
        }
        let mut resolved: BTreeMap<String, VarValue> = BTreeMap::new();
        // `${VAR}` references of the `.env` value that won, by name.
        let mut env_refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut env_applied = false;
        let apply_env =
            |resolved: &mut BTreeMap<String, VarValue>,
             env_refs: &mut BTreeMap<String, BTreeSet<String>>| {
                let Some(env) = &self.environment else {
                    return;
                };
                let names: Vec<String> = resolved
                    .iter()
                    .filter(|(_, v)| v.source.rank != Rank::Facts)
                    .map(|(n, _)| n.clone())
                    .chain(decls.keys().cloned())
                    .collect();
                for name in names {
                    if let Some(v) = env.get(&name) {
                        env_refs.remove(&name);
                        resolved.insert(
                            name,
                            VarValue {
                                value: Value::String(v.clone()),
                                source: Source::environment(),
                                sensitive: false,
                            },
                        );
                    }
                }
            };
        for layer in &self.layers {
            if !env_applied && layer.source.rank > Rank::Environment {
                apply_env(&mut resolved, &mut env_refs);
                env_applied = true;
            }
            for (name, value) in &layer.values {
                match layer.refs.get(name) {
                    Some(r) => env_refs.insert(name.clone(), r.clone()),
                    None => env_refs.remove(name),
                };
                resolved.insert(
                    name.clone(),
                    VarValue {
                        value: value.clone(),
                        source: layer.source.clone(),
                        sensitive: false,
                    },
                );
            }
        }
        if !env_applied {
            apply_env(&mut resolved, &mut env_refs);
        }

        // Sensitivity first: error messages below must not show a sensitive value.
        let declared_sensitive = |name: &str| decls.get(name).and_then(|d| d.sensitive);
        for (name, v) in resolved.iter_mut() {
            v.sensitive = declared_sensitive(name)
                .unwrap_or_else(|| v.source.rank != Rank::Facts && looks_sensitive(name));
        }
        // `.env` values interpolated from sensitive ones (to a fixed point: chains of them).
        loop {
            let newly: Vec<String> = env_refs
                .iter()
                .filter(|(n, _)| resolved.get(*n).is_some_and(|v| !v.sensitive))
                .filter(|(n, _)| declared_sensitive(n) != Some(false))
                .filter(|(_, refs)| {
                    refs.iter().any(|r| match resolved.get(r) {
                        Some(v) => v.sensitive,
                        // From the process environment: judge by the name.
                        None => looks_sensitive(r),
                    })
                })
                .map(|(n, _)| n.clone())
                .collect();
            if newly.is_empty() {
                break;
            }
            for n in newly {
                if let Some(v) = resolved.get_mut(&n) {
                    v.sensitive = true;
                }
            }
        }

        let mut value_problems = Vec::new();
        let home = self.home.as_deref();
        // Declared types apply before templates see a value; templated values are coerced
        // right after they are rendered.
        for (name, v) in resolved.iter_mut() {
            if !is_template_value(v) {
                coerce_declared(name, v, decls.get(name), home, &mut value_problems);
            }
        }
        let mut unrendered = BTreeMap::new();
        // Deferred prompts with a value by now are not deferred.
        let mut asked: BTreeSet<String> = self
            .asked
            .iter()
            .filter(|n| !resolved.contains_key(*n))
            .cloned()
            .collect();
        let mut problems = render_values(
            &mut resolved,
            &decls,
            home,
            &mut value_problems,
            &mut unrendered,
            &mut asked,
        );
        for (name, v) in resolved.iter() {
            if unrendered.contains_key(name) {
                continue;
            }
            let Some(p) = decls.get(name).and_then(|d| d.pattern.as_ref()) else {
                continue;
            };
            let Ok(re) = compile_pattern(p) else {
                value_problems.push(format!("variable `{name}`: invalid pattern `{p}`"));
                continue;
            };
            let items: Vec<String> = match &v.value {
                Value::Array(a) => a.iter().map(value_string).collect(),
                Value::Null => vec![],
                other => vec![value_string(other)],
            };
            if let Some(bad) = items.iter().find(|s| !re.is_match(s)) {
                let shown = if v.sensitive { MASKED } else { bad.as_str() };
                value_problems.push(format!(
                    "variable `{name}` (from {}): `{shown}` does not match pattern `{p}`",
                    v.source
                ));
            }
        }
        problems.extend(value_problems.iter().cloned());
        let mut template_ctx: BTreeMap<String, minijinja::Value> = resolved
            .iter()
            .filter(|(k, _)| !unrendered.contains_key(*k))
            .map(|(k, v)| (k.clone(), minijinja::Value::from_serialize(&v.value)))
            .collect();
        for n in &asked {
            template_ctx
                .entry(n.clone())
                .or_insert_with(|| asked_placeholder(n).into());
        }
        self.template_ctx = Arc::new(template_ctx);
        self.asked_all = Arc::new(asked);
        self.unrendered = Arc::new(unrendered);
        self.resolved = resolved;
        self.decls = decls;
        self.decl_order = decl_order;
        self.problems = problems;
        self.value_problems = value_problems;
    }
}

/// How a sensitive value is shown in messages.
const MASKED: &str = "***";

/// Applies the declared type to `v`, recording a problem (never showing a sensitive value)
/// when it does not fit.
fn coerce_declared(
    name: &str,
    v: &mut VarValue,
    decl: Option<&VarDecl>,
    home: Option<&Path>,
    problems: &mut Vec<String>,
) {
    let Some(ty) = decl.and_then(|d| d.ty) else {
        return;
    };
    match coerce(&v.value, ty, home) {
        Ok(c) => v.value = c,
        Err(()) => {
            let shown = if v.sensitive {
                MASKED.to_string()
            } else {
                value_string(&v.value)
            };
            problems.push(format!(
                "variable `{name}` (from {}): expected {}, got `{shown}`",
                v.source,
                ty.as_str()
            ));
        }
    }
}

fn is_template_value(v: &VarValue) -> bool {
    is_templated_rank(v.source.rank) && value_has_template(&v.value)
}

/// Whether values from this layer are templates (`vars:` in YAML); `.env` files have their own
/// `${VAR}` interpolation, and the environment and command line are taken literally.
fn is_templated_rank(r: Rank) -> bool {
    matches!(r, Rank::Repo | Rank::Profile | Rank::Bundle)
}

fn value_has_template(v: &Value) -> bool {
    match v {
        Value::String(s) => crate::template::is_template(s),
        Value::Array(a) => a.iter().any(value_has_template),
        _ => false,
    }
}

/// Renders templated `vars:` values (`greeting: "hi from {{ profile }}"`), each after the
/// values it refers to, then applies its declared type. A value built from a sensitive one
/// becomes sensitive (unless declared `sensitive: false`). A value that cannot be rendered goes
/// into `unrendered` with the reason (and so do values built from it). `asked`: variables to be
/// asked for later, rendered as placeholders; values built from one are added to it. Returns
/// the render problems; type problems go to `value_problems`.
fn render_values(
    resolved: &mut BTreeMap<String, VarValue>,
    decls: &BTreeMap<String, VarDecl>,
    home: Option<&Path>,
    value_problems: &mut Vec<String>,
    unrendered: &mut BTreeMap<String, String>,
    asked: &mut BTreeSet<String>,
) -> Vec<String> {
    use crate::template::{SecretMode, referenced_names, render_in};
    let mut pending: BTreeMap<String, BTreeSet<String>> = resolved
        .iter()
        .filter(|(_, v)| is_template_value(v))
        .map(|(n, v)| {
            let texts: Vec<String> = match &v.value {
                Value::Array(a) => a.iter().map(value_string).collect(),
                other => vec![value_string(other)],
            };
            let refs = texts.iter().flat_map(|t| referenced_names(t)).collect();
            (n.clone(), refs)
        })
        .collect();
    let mut problems = Vec::new();
    loop {
        let ready: Vec<String> = pending
            .iter()
            .filter(|(_, refs)| refs.iter().all(|r| !pending.contains_key(r)))
            .map(|(n, _)| n.clone())
            .collect();
        if ready.is_empty() {
            break;
        }
        let mut ctx: BTreeMap<String, minijinja::Value> = resolved
            .iter()
            .filter(|(k, _)| !unrendered.contains_key(*k))
            .map(|(k, v)| (k.clone(), minijinja::Value::from_serialize(&v.value)))
            .collect();
        for n in asked.iter() {
            ctx.entry(n.clone())
                .or_insert_with(|| asked_placeholder(n).into());
        }
        for name in ready {
            let refs = pending.remove(&name).unwrap_or_default();
            if refs.iter().any(|r| asked.contains(r)) {
                asked.insert(name.clone());
            }
            let from_sensitive = refs
                .iter()
                .any(|r| resolved.get(r).is_some_and(|v| v.sensitive));
            let v = resolved.get_mut(&name).expect("pending names are resolved");
            let render =
                |s: &str| render_in(&ctx, unrendered, s, &SecretMode::Forbidden).map(|r| r.text);
            let rendered = match &v.value {
                Value::String(s) => render(s).map(Value::String),
                Value::Array(a) => a
                    .iter()
                    .map(|i| match i {
                        Value::String(s) => render(s).map(Value::String),
                        other => Ok(other.clone()),
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(Value::Array),
                other => Ok(other.clone()),
            };
            if from_sensitive && decls.get(&name).and_then(|d| d.sensitive) != Some(false) {
                v.sensitive = true;
            }
            match rendered {
                Ok(r) => {
                    v.value = r;
                    coerce_declared(&name, v, decls.get(&name), home, value_problems);
                }
                Err(e) => {
                    problems.push(format!("variable `{name}` (from {}): {e}", v.source));
                    unrendered.insert(name, e);
                }
            }
        }
    }
    if !pending.is_empty() {
        let names: Vec<&str> = pending.keys().map(String::as_str).collect();
        let why = format!(
            "variables refer to each other in a cycle: {}",
            names.join(", ")
        );
        for n in pending.keys() {
            unrendered.insert(n.clone(), why.clone());
        }
        problems.push(why);
    }
    problems
}

fn is_env_name(n: &str) -> bool {
    !n.is_empty()
        && !n.starts_with(|c: char| c.is_ascii_digit())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `v` as type `ty`; `Err` when it does not fit (the caller words the error, so a sensitive
/// value is never shown).
fn coerce(v: &Value, ty: VarType, home: Option<&Path>) -> Result<Value, ()> {
    Ok(match (ty, v) {
        (_, Value::Null) => Value::Null,
        (VarType::String, Value::Array(_)) => return Err(()),
        (VarType::String, other) => Value::String(value_string(other)),
        (VarType::Bool, Value::Bool(b)) => Value::Bool(*b),
        (VarType::Bool, Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" => Value::Bool(true),
            "false" | "no" | "off" | "0" => Value::Bool(false),
            _ => return Err(()),
        },
        (VarType::Bool, _) => return Err(()),
        (VarType::Int, Value::Number(n)) if n.is_i64() || n.is_u64() => Value::Number(n.clone()),
        (VarType::Int, Value::String(s)) => Value::from(s.trim().parse::<i64>().map_err(|_| ())?),
        (VarType::Int, _) => return Err(()),
        (VarType::List, Value::Array(a)) => Value::Array(a.clone()),
        (VarType::List, Value::String(s)) => Value::Array(
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(|p| Value::String(p.to_string()))
                .collect(),
        ),
        (VarType::List, other) => Value::Array(vec![other.clone()]),
        (VarType::Path, Value::String(s)) => match home {
            Some(h) => Value::String(crate::paths::expand_tilde(s, h).display().to_string()),
            None => Value::String(s.clone()),
        },
        (VarType::Path, _) => return Err(()),
    })
}

/// One `.env`-style file to load, and the layer it forms.
#[derive(Debug, Clone)]
pub struct EnvFile {
    pub path: PathBuf,
    pub rank: Rank,
    /// How the source is shown, e.g. `.env.local`.
    pub label: String,
    /// A missing file is an error (an `--env-file`), not a silently empty layer.
    pub required: bool,
}

/// Reads `.env` files into layers with dotenv syntax. `${VAR}` interpolation sees every earlier
/// file in `files` (and, as in dotenv, the process environment first), so `.env.local` can refer
/// to a value from `.env`. Keys in `skip` (control keys such as `DOT_PROFILE`) are left out.
pub fn load_env_files(files: &[EnvFile], skip: &[&str]) -> Result<Vec<Layer>, ConfigError> {
    let mut texts = Vec::new();
    for f in files {
        match std::fs::read_to_string(&f.path) {
            Ok(t) => texts.push((f, t)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !f.required => {}
            Err(e) => return Err(ConfigError::new(&f.path, format!("cannot read: {e}"))),
        }
    }
    // Parse each file alone first: errors get their own file and line, and the entry counts
    // let the combined parse below be split back into files.
    let mut counts = Vec::new();
    for (f, text) in &texts {
        let mut n = 0;
        for item in dotenvy::from_read_iter(text.as_bytes()) {
            item.map_err(|e| env_error(&f.path, text, e))?;
            n += 1;
        }
        counts.push(n);
    }
    let mut combined = String::new();
    for (_, text) in &texts {
        combined.push_str(text);
        if !text.ends_with('\n') {
            combined.push('\n');
        }
    }
    let mut entries = dotenvy::from_read_iter(combined.as_bytes())
        .collect::<Result<Vec<(String, String)>, _>>()
        .map_err(|e| {
            let path = texts
                .last()
                .map(|(f, _)| f.path.clone())
                .unwrap_or_default();
            env_error(&path, "", e)
        })?
        .into_iter();
    let mut layers = Vec::new();
    for ((f, text), n) in texts.iter().zip(counts) {
        let mut layer = Layer::new(Source::new(f.rank, f.label.clone()));
        for (k, v) in entries.by_ref().take(n) {
            if !skip.contains(&k.as_str()) {
                layer.set(k, Value::String(v));
            }
        }
        for (k, refs) in interpolation_refs(text) {
            if layer.values.iter().any(|(n, _)| *n == k) && !refs.is_empty() {
                layer.refs.insert(k, refs);
            }
        }
        layers.push(layer);
    }
    Ok(layers)
}

/// For each `KEY=value` line of a dotenv file, the names its value interpolates (`${VAR}` or
/// `$VAR`; nothing inside single quotes). The last line for a key wins, as in dotenv.
fn interpolation_refs(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    let re = regex::Regex::new(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)").expect("valid regex");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let l = line.trim_start();
        if l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").unwrap_or(l);
        let Some((key, value)) = l.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !is_env_name(key) {
            continue;
        }
        let value = value.trim_start();
        let refs: BTreeSet<String> = if value.starts_with('\'') {
            BTreeSet::new()
        } else {
            re.captures_iter(value).map(|c| c[1].to_string()).collect()
        };
        out.insert(key.to_string(), refs);
    }
    out
}

/// A dotenv error located by file, line and column. The offending line is never shown: in
/// `.env.local`, `local.env` or an `--env-file` it may hold a credential. Only its key is named.
fn env_error(path: &Path, text: &str, e: dotenvy::Error) -> ConfigError {
    const HIDDEN: &str = "the line is not shown, it may hold a secret";
    let mut err = match &e {
        dotenvy::Error::Io(io) => return ConfigError::new(path, format!("cannot read: {io}")),
        _ => ConfigError::new(path, format!("invalid dotenv syntax ({HIDDEN})")),
    };
    if let dotenvy::Error::LineParse(line, idx) = &e {
        let first = line.lines().next().unwrap_or("");
        if let Some(n) = text.lines().position(|l| l == first) {
            err.line = Some(n + 1);
            err.column = Some(idx + 1);
        }
        let key = first
            .trim_start()
            .strip_prefix("export ")
            .unwrap_or(first.trim_start())
            .split_once('=')
            .map(|(k, _)| k.trim())
            .filter(|k| is_env_name(k));
        if let Some(k) = key {
            err.message = format!("invalid dotenv syntax in the value of `{k}` ({HIDDEN})");
        }
    }
    err
}

/// `.env.example` as a layer of declarations: every key it lists is a required variable,
/// except keys marked `# secret` (those live with the secrets provider, see `secrets.yml`),
/// which are declared sensitive instead so a value set anywhere is redacted and not exported.
/// `None` when there is no such file.
pub fn example_layer(path: &Path) -> Result<Option<Layer>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ConfigError::new(path, format!("cannot read: {e}"))),
    };
    for item in dotenvy::from_read_iter(text.as_bytes()) {
        item.map_err(|e| env_error(path, &text, e))?;
    }
    let mut layer = Layer::new(Source::new(Rank::Example, ".env.example"));
    for (key, secret) in example_keys(&text) {
        let decl = if secret {
            VarDecl {
                sensitive: Some(true),
                ..VarDecl::default()
            }
        } else {
            VarDecl {
                required: Some(true),
                ..VarDecl::default()
            }
        };
        layer.decls.retain(|(k, _)| *k != key);
        layer.decls.push((key, decl));
    }
    Ok(Some(layer))
}

/// Keys of a `.env.example` and whether each is marked secret: an inline comment starting with
/// the word `secret` (`KEY=   # secret: where to get it`), outside any quotes.
pub fn example_keys(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.trim_start();
        if l.starts_with('#') {
            continue;
        }
        let l = l.strip_prefix("export ").unwrap_or(l);
        let Some((key, value)) = l.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !is_env_name(key) {
            continue;
        }
        out.push((key.to_string(), marks_secret(inline_comment(value))));
    }
    out
}

/// The inline comment of a dotenv value: after a `#` preceded by whitespace (or starting the
/// value), outside quotes.
fn inline_comment(value: &str) -> Option<&str> {
    let mut quote: Option<char> = None;
    let mut prev_space = true;
    for (i, c) in value.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '#' && prev_space => return Some(&value[i + 1..]),
            None => {}
        }
        prev_space = c.is_whitespace();
    }
    None
}

fn marks_secret(comment: Option<&str>) -> bool {
    comment.is_some_and(|c| {
        let c = c.trim_start();
        c.get(..6).is_some_and(|w| w.eq_ignore_ascii_case("secret"))
            && !c[6..].starts_with(|ch: char| ch.is_alphanumeric() || ch == '_')
    })
}

/// Parses `KEY=value` from the command line.
pub fn parse_assignment(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_string(), v.to_string())),
        _ => Err(format!("expected KEY=value, got `{s}`")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::facts::{Os, Role};

    fn home() -> PathBuf {
        crate::paths::tests::abs("/h")
    }

    fn base() -> Vars {
        let mut f = Facts::new(Os::Linux, "x86_64", Role::Server);
        f.distro = Some("ubuntu".into());
        Vars::from_facts(&f, &home())
    }

    fn layer(source: Source, pairs: &[(&str, Value)]) -> Layer {
        let mut l = Layer::new(source);
        for (k, v) in pairs {
            l.set(*k, v.clone());
        }
        l
    }

    fn specs(yaml: &str) -> Vec<(String, VarSpec)> {
        let m: crate::config::shapes::OrderedMap<VarSpec> = serde_yaml_ng::from_str(yaml).unwrap();
        m.0
    }

    fn file_layer(rank: Rank, label: &str, pairs: &[(&str, &str)]) -> Layer {
        let mut l = Layer::new(Source::new(rank, label));
        for (k, v) in pairs {
            l.set(*k, json!(v));
        }
        l
    }

    #[test]
    fn precedence_and_sources() {
        let mut v = base();
        v.push_layer(layer(
            Source::repo(),
            &[("editor", json!("vi")), ("a", json!("repo"))],
        ));
        v.push_layer(file_layer(Rank::Local, ".env.local", &[("a", "local")]));
        v.push_layer(file_layer(
            Rank::DotEnv,
            ".env",
            &[("a", "dotenv"), ("b", "x")],
        ));
        v.push_layer(layer(Source::profile("mac"), &[("editor", json!("nvim"))]));
        let mut b = crate::config::parse_bundle(
            "vars: { editor: code, c: 1 }\nsteps: []\n",
            &crate::paths::tests::abs("/r/bundles/x/bundle.yml"),
        )
        .unwrap();
        b.name = "x".into();
        let v = v.with_bundle(&b);
        let src = |n: &str| v.get(n).unwrap().source.name.clone();
        assert_eq!(v.get("editor").unwrap().value, json!("code"));
        assert_eq!(src("editor"), "bundle x");
        assert_eq!(v.get("a").unwrap().value, json!("local"));
        assert_eq!(src("a"), ".env.local");
        assert_eq!(src("b"), ".env");
        assert_eq!(v.get("c").unwrap().value, json!(1));
        assert_eq!(src("os"), "fact");
        assert_eq!(v.get("distro").unwrap().value, json!("ubuntu"));
        assert_eq!(v.get("version").unwrap().value, Value::Null);
    }

    #[test]
    fn process_environment_overrides_only_known_names() {
        let mut v = base();
        v.push_layer(file_layer(Rank::DotEnv, ".env", &[("EDITOR", "vi")]));
        v.push_layer(Layer::from_specs(
            Source::repo(),
            specs("DECLARED: { type: string }\n")
                .iter()
                .map(|(k, s)| (k, s)),
        ));
        v.set_environment(BTreeMap::from([
            ("EDITOR".into(), "emacs".into()),
            ("DECLARED".into(), "yes".into()),
            ("UNRELATED".into(), "x".into()),
            ("os".into(), "plan9".into()),
        ]));
        assert_eq!(v.get("EDITOR").unwrap().value, json!("emacs"));
        assert_eq!(v.get("EDITOR").unwrap().source, Source::environment());
        assert_eq!(v.get("DECLARED").unwrap().value, json!("yes"));
        assert!(v.get("UNRELATED").is_none());
        assert_eq!(
            v.get("os").unwrap().value,
            json!("linux"),
            "facts are not overridden"
        );
        // --var beats the environment.
        v.set("EDITOR", "nano");
        assert_eq!(v.get("EDITOR").unwrap().source, Source::cli());
    }

    #[test]
    fn declarations_merge_coerce_and_validate() {
        let mut v = base();
        let s = specs(
            r#"
port: { type: int, default: 8080 }
debug: { type: bool }
langs: { type: list }
dir: { type: path, default: "~/src" }
email: { required: true, pattern: ".+@.+" }
name: { required: true }
gh_token: { required: false }
"#,
        );
        v.push_layer(Layer::from_specs(
            Source::repo(),
            s.iter().map(|(k, s)| (k, s)),
        ));
        v.push_layer(file_layer(
            Rank::DotEnv,
            ".env",
            &[
                ("debug", "yes"),
                ("langs", "rust, go"),
                ("email", "nope"),
                ("gh_token", "t"),
            ],
        ));
        assert_eq!(v.get("port").unwrap().value, json!(8080));
        assert_eq!(v.get("debug").unwrap().value, json!(true));
        assert_eq!(v.get("langs").unwrap().value, json!(["rust", "go"]));
        assert_eq!(
            v.get("dir").unwrap().value,
            json!(home().join("src").display().to_string())
        );
        assert_eq!(v.missing_required(), ["name"]);
        assert_eq!(v.problems().len(), 1);
        assert!(
            v.problems()[0].contains("does not match pattern"),
            "{:?}",
            v.problems()
        );
        assert!(v.get("gh_token").unwrap().sensitive);
        assert!(!v.get("email").unwrap().sensitive);

        v.push_layer(file_layer(Rank::Local, "local.env", &[("port", "abc")]));
        assert!(
            v.problems()
                .iter()
                .any(|p| p.contains("expected int, got `abc`"))
        );
    }

    #[test]
    fn sensitive_values_are_not_exported_unless_asked() {
        let mut v = base();
        let s =
            specs("api: { sensitive: true, default: k }\nplain: x\nMY_TOKEN: t\n\"bad-name\": y\n");
        v.push_layer(Layer::from_specs(
            Source::repo(),
            s.iter().map(|(k, s)| (k, s)),
        ));
        let env = v.exported(&[]).unwrap();
        assert_eq!(env, [("plain".to_string(), "x".to_string())]);
        let env = v.exported(&["api".into()]).unwrap();
        assert!(env.contains(&("api".into(), "k".into())));
        assert!(v.exported(&["nope".into()]).is_err());
    }

    #[test]
    fn yaml_values_are_templates_rendered_in_dependency_order() {
        let mut v = base();
        let s = specs(
            "c: \"{{ b }}!\"\nb: \"{{ a }}-{{ os }}\"\na: x\nl: [\"{{ a }}\", 2]\nloop1: \"{{ loop2 }}\"\nloop2: \"{{ loop1 }}\"\nbad: \"{{ nope }}\"\n",
        );
        v.push_layer(Layer::from_specs(
            Source::repo(),
            s.iter().map(|(k, s)| (k, s)),
        ));
        v.push_layer(file_layer(Rank::DotEnv, ".env", &[("literal", "{{ a }}")]));
        assert_eq!(v.get("c").unwrap().value, json!("x-linux!"));
        assert_eq!(v.get("l").unwrap().value, json!(["x", 2]));
        assert_eq!(
            v.get("literal").unwrap().value,
            json!("{{ a }}"),
            ".env is not a template"
        );
        assert!(
            v.problems()
                .iter()
                .any(|p| p.contains("cycle: loop1, loop2")),
            "{:?}",
            v.problems()
        );
        assert!(
            v.problems()
                .iter()
                .any(|p| p.contains("variable `bad` (from dot.yml): undefined variable `nope`")),
            "{:?}",
            v.problems()
        );
        assert_eq!(v.render("{{ c }}").unwrap(), "x-linux!");
    }

    #[test]
    fn var_specs_parse_strictly() {
        let s = specs("a: 1\nb: [x, 2, true]\nc: { type: int, required: true }\n");
        assert_eq!(s[0].1, VarSpec::Value(json!(1)));
        assert_eq!(s[1].1, VarSpec::Value(json!(["x", 2, true])));
        let bad = |y: &str| {
            serde_yaml_ng::from_str::<crate::config::shapes::OrderedMap<VarSpec>>(y)
                .unwrap_err()
                .to_string()
        };
        assert!(bad("a: { tpye: int }\n").contains("unknown field `tpye`"));
        assert!(bad("a: [[1]]\n").contains("nested"));
        assert!(bad("a:\n").contains("needs a value"));
        assert!(bad("a: { pattern: \"(\" }\n").contains("invalid pattern"));
        assert!(bad("a: { type: float }\n").contains("unknown variant"));
    }

    #[test]
    fn env_files_interpolate_across_layers() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        std::fs::write(
            p(".env"),
            "# comment\nDOT_TEST_BASE=/opt\nexport DOT_TEST_BIN=\"${DOT_TEST_BASE}/bin\"\nQUOTED='literal ${DOT_TEST_BASE}'\n",
        )
        .unwrap();
        std::fs::write(
            p(".env.local"),
            "DOT_TEST_LIB=${DOT_TEST_BASE}/lib\nDOT_PROFILE=mac",
        )
        .unwrap();
        let files = [
            EnvFile {
                path: p(".env"),
                rank: Rank::DotEnv,
                label: ".env".into(),
                required: false,
            },
            EnvFile {
                path: p(".env.mac"),
                rank: Rank::DotEnvProfile,
                label: ".env.mac".into(),
                required: false,
            },
            EnvFile {
                path: p(".env.local"),
                rank: Rank::Local,
                label: ".env.local".into(),
                required: false,
            },
        ];
        let layers = load_env_files(&files, &["DOT_PROFILE"]).unwrap();
        assert_eq!(layers.len(), 2, "a missing optional file is no layer");
        let get = |l: &Layer, k: &str| {
            l.values
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get(&layers[0], "DOT_TEST_BIN"), Some(json!("/opt/bin")));
        assert_eq!(
            get(&layers[0], "QUOTED"),
            Some(json!("literal ${DOT_TEST_BASE}"))
        );
        assert_eq!(get(&layers[1], "DOT_TEST_LIB"), Some(json!("/opt/lib")));
        assert_eq!(get(&layers[1], "DOT_PROFILE"), None);
        assert_eq!(layers[1].source.name, ".env.local");

        std::fs::write(p("bad.env"), "A=1\nB='unterminated\n").unwrap();
        let err = load_env_files(
            &[EnvFile {
                path: p("bad.env"),
                rank: Rank::Cli,
                label: "bad".into(),
                required: true,
            }],
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("bad.env"), "{err}");
        let missing = [EnvFile {
            path: p("nope.env"),
            rank: Rank::Cli,
            label: "x".into(),
            required: true,
        }];
        assert!(load_env_files(&missing, &[]).is_err());
    }

    #[test]
    fn templates_see_declared_types() {
        let mut v = base();
        let s = specs(
            "dir: { type: path, default: \"~/work\" }\nsub: \"{{ dir }}/sub\"\nflag: { type: bool, default: \"no\" }\nflagmsg: \"{% if flag %}on{% else %}off{% endif %}\"\nport: { type: int, default: \"{{ 8000 + 80 }}\" }\nnext: \"{{ port + 1 }}\"\n",
        );
        v.push_layer(Layer::from_specs(
            Source::repo(),
            s.iter().map(|(k, s)| (k, s)),
        ));
        let work = home().join("work").display().to_string();
        assert_eq!(v.get("dir").unwrap().value, json!(work));
        assert_eq!(v.get("sub").unwrap().value, json!(format!("{work}/sub")));
        assert_eq!(v.get("flag").unwrap().value, json!(false));
        assert_eq!(v.get("flagmsg").unwrap().value, json!("off"));
        assert_eq!(
            v.get("port").unwrap().value,
            json!(8080),
            "coerced after rendering"
        );
        assert_eq!(v.get("next").unwrap().value, json!("8081"));
        assert!(v.problems().is_empty(), "{:?}", v.problems());
    }

    #[test]
    fn values_built_from_sensitive_ones_are_sensitive() {
        let mut v = base();
        let s = specs(
            "clone_url: \"https://oauth:{{ gitlab_token }}@gitlab.com\"\nhost: \"{{ clone_url }}\"\nshown: { sensitive: false, default: \"{{ gitlab_token | length }}\" }\nplain: \"{{ os }}\"\n",
        );
        v.push_layer(Layer::from_specs(
            Source::repo(),
            s.iter().map(|(k, s)| (k, s)),
        ));
        v.push_layer(file_layer(
            Rank::Local,
            ".env.local",
            &[("gitlab_token", "glpat-abc")],
        ));
        assert!(v.get("gitlab_token").unwrap().sensitive);
        assert!(v.get("clone_url").unwrap().sensitive, "templated");
        assert!(v.get("host").unwrap().sensitive, "transitively");
        assert!(!v.get("shown").unwrap().sensitive, "declared not sensitive");
        assert!(!v.get("plain").unwrap().sensitive);
        let exported = v.exported(&[]).unwrap();
        assert!(
            !exported.iter().any(|(_, val)| val.contains("glpat-abc")),
            "{exported:?}"
        );
    }

    #[test]
    fn env_interpolation_of_a_sensitive_value_is_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env.local");
        std::fs::write(
            &p,
            "DB_PASSWORD=hunter2\nDSN=postgres://u:${DB_PASSWORD}@db\nLIT='${DB_PASSWORD}'\nOTHER=x\n",
        )
        .unwrap();
        let files = [EnvFile {
            path: p,
            rank: Rank::Local,
            label: ".env.local".into(),
            required: false,
        }];
        let mut v = base();
        for l in load_env_files(&files, &[]).unwrap() {
            v.push_layer(l);
        }
        assert_eq!(
            v.get("DSN").unwrap().value,
            json!("postgres://u:hunter2@db")
        );
        assert!(v.get("DSN").unwrap().sensitive);
        assert!(
            !v.get("LIT").unwrap().sensitive,
            "single quotes do not interpolate"
        );
        assert!(!v.get("OTHER").unwrap().sensitive);
    }

    #[test]
    fn a_sensitive_value_is_never_shown_in_a_problem() {
        let mut v = base();
        let s = specs("db_password: { type: int }\napi_key: { pattern: \"[0-9]+\" }\n");
        v.push_layer(Layer::from_specs(
            Source::repo(),
            s.iter().map(|(k, s)| (k, s)),
        ));
        v.push_layer(file_layer(
            Rank::Local,
            ".env.local",
            &[
                ("db_password", "hunter2-SENSITIVE"),
                ("api_key", "k-SENSITIVE"),
            ],
        ));
        assert_eq!(v.problems().len(), 2, "{:?}", v.problems());
        for p in v.problems() {
            assert!(!p.contains("SENSITIVE"), "{p}");
            assert!(p.contains("`***`"), "{p}");
        }
    }

    #[test]
    fn dotenv_syntax_errors_never_show_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env.local");
        std::fs::write(&p, "A=1\nother_password=hunter2hunter2 x\"\n").unwrap();
        let files = [EnvFile {
            path: p,
            rank: Rank::Local,
            label: ".env.local".into(),
            required: false,
        }];
        let e = load_env_files(&files, &[]).unwrap_err();
        let text = e.to_string();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("`other_password`"), "{text}");
        assert_eq!(e.line, Some(2), "{text}");
    }

    #[test]
    fn an_unrenderable_value_is_undefined_with_its_reason() {
        let mut v = base();
        let s = specs("url: \"http://localhost:{{ port }}\"\nfull: \"{{ url }}/x\"\n");
        v.push_layer(Layer::from_specs(
            Source::repo(),
            s.iter().map(|(k, s)| (k, s)),
        ));
        assert!(v.value_problems().is_empty());
        assert_eq!(v.problems().len(), 2, "{:?}", v.problems());
        let e = v.render("{{ url }}").unwrap_err();
        assert_eq!(
            e,
            "variable `url` could not be rendered: undefined variable `port`"
        );
        // A layer that defines `port` (a bundle's `vars:`) fixes it.
        v.push_layer(layer(Source::bundle("b"), &[("port", json!(8080))]));
        assert!(v.problems().is_empty(), "{:?}", v.problems());
        assert_eq!(v.render("{{ full }}").unwrap(), "http://localhost:8080/x");
    }

    #[test]
    fn env_example_declares_required_and_sensitive_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env.example");
        assert!(example_layer(&p).unwrap().is_none());
        std::fs::write(
            &p,
            "# comment\nEMAIL=\nVPN_PSK=   # secret: from the VPN admin\nNOTE=\"a # secret\"\nexport REGION=eu # region\n",
        )
        .unwrap();
        let mut v = base();
        v.push_layer(example_layer(&p).unwrap().unwrap());
        v.push_layer(Layer::from_specs(
            Source::repo(),
            specs("REGION: { required: false }\n")
                .iter()
                .map(|(k, s)| (k, s)),
        ));
        v.push_layer(file_layer(
            Rank::Local,
            ".env.local",
            &[("VPN_PSK", "abc123xyz")],
        ));
        assert_eq!(
            v.missing_required(),
            ["EMAIL", "NOTE"],
            "dot.yml overrides REGION"
        );
        assert!(v.get("VPN_PSK").unwrap().sensitive);
        assert!(v.exported(&[]).unwrap().iter().all(|(k, _)| k != "VPN_PSK"));
        assert!(v.decl("VPN_PSK").unwrap().required.is_none());
    }

    #[test]
    fn assignments() {
        assert_eq!(
            parse_assignment("a=b=c").unwrap(),
            ("a".into(), "b=c".into())
        );
        assert!(parse_assignment("novalue").is_err());
        assert!(parse_assignment("=x").is_err());
    }
}
