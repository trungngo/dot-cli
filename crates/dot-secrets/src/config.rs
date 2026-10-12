//! `secrets.yml`: committed, holds no values.
//!
//! ```yaml
//! default_provider: envfile
//! providers:
//!   envfile: { path: "~/.config/dot/secrets.env" }
//! secrets:
//!   gitlab_token:   { ref: "envfile:GITLAB_TOKEN" }
//!   vpn_profile: { ref: "envfile-file:vpn/work.ovpn", kind: file }
//!   npm_token:      { ref: NPM_TOKEN }            # bare: read by the default provider
//!   glab:           { ref: "lastpass://Dev/GitLab#password", fallback: "envfile:GITLAB_TOKEN" }
//! ```
//!
//! A bare ref (no `provider:` prefix) is served by `default_provider`, which a profile can
//! override (`secrets_provider`); the caller applies that with
//! [`SecretsConfig::with_provider_override`]. Parsing is strict: unknown keys and malformed refs
//! are errors with file and line.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer};

use crate::reference::{ENVFILE, RefError, SecretRef};
use crate::registry::Registry;

/// `secrets.yml`, at the root of a dotfiles repository: where each secret lives, by logical
/// name. It holds no values.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "dot secrets")]
pub struct SecretsConfig {
    /// Provider for bare refs. A profile's `secrets_provider` overrides it.
    #[serde(default = "default_provider")]
    pub default_provider: String,
    /// Settings of each provider.
    #[serde(default)]
    pub providers: ProvidersConfig,
    /// Logical secret name → where it lives.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretSpec>,
    /// The profile's `secrets_provider`, when it set one: see
    /// [`SecretsConfig::with_provider_override`].
    #[serde(skip)]
    pub(crate) preferred: Option<String>,
}

fn default_provider() -> String {
    ENVFILE.to_string()
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            default_provider: default_provider(),
            providers: ProvidersConfig::default(),
            secrets: BTreeMap::new(),
            preferred: None,
        }
    }
}

/// Per-provider settings. Typed, so a misspelt provider or option is an error.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    /// A dotenv file (and a directory of file secrets) outside the repository.
    pub envfile: Option<EnvfileConfig>,
    /// LastPass, through `lastpass-cli` (`lpass`).
    pub lastpass: Option<LastpassConfig>,
    /// 1Password, through its CLI (`op`). Not implemented yet: `op://` refs parse, and resolve
    /// to their `fallback`.
    #[serde(rename = "1password")]
    pub onepassword: Option<OnePasswordConfig>,
}

/// `envfile` settings.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvfileConfig {
    /// dotenv file holding values. Default `~/.config/dot/secrets.env`. `~` is the home
    /// directory; a relative path is relative to `~/.config/dot`.
    pub path: Option<String>,
    /// Directory holding `envfile-file:` secrets. Default: `files/` next to `path`.
    pub files_dir: Option<String>,
}

/// `lastpass` settings.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LastpassConfig {
    /// The account to log in as (`lpass login <username>`) when `lpass` is logged out.
    pub username: Option<String>,
}

/// `1password` settings (the provider is not implemented yet).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OnePasswordConfig {
    /// The account, e.g. `my.1password.com` (for the provider, once it is implemented).
    pub account: Option<String>,
}

/// Where one secret lives.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
    /// Where the value is: `envfile:NAME`, `envfile-file:path`, `lastpass://Folder/Item#field`,
    /// `op://vault/item/field` (1Password: not implemented yet, resolves to `fallback`), or a
    /// bare name for the default provider.
    #[serde(rename = "ref")]
    pub reference: RefSpec,
    /// Tried when `ref` has no value or its provider is unavailable.
    #[serde(default)]
    pub fallback: Option<RefSpec>,
    /// `value` (a string, the default) or `file` (for `secret_file`).
    #[serde(default)]
    pub kind: SecretKind,
    /// A machine may lack it: with no value (and nothing to ask on a non-interactive run, or
    /// its provider unavailable) the steps that use it are skipped instead of failing their
    /// bundle, and nobody is prompted for it. Default false.
    #[serde(default)]
    pub optional: bool,
}

/// How a secret is consumed: as a string (`{{ secret("x") }}`) or as a file (`secret_file:`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SecretKind {
    #[default]
    Value,
    File,
}

/// A ref as written in `secrets.yml`: fully qualified, or a bare name for the default provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefSpec {
    Ref(SecretRef),
    Bare(String),
}

impl RefSpec {
    pub fn parse(s: &str) -> Result<Self, RefError> {
        if s.contains(':') {
            SecretRef::parse(s).map(Self::Ref)
        } else if s.is_empty() || s.chars().any(char::is_whitespace) {
            Err(RefError {
                reference: s.to_string(),
                reason: "a bare ref must be a non-empty name without whitespace".into(),
            })
        } else {
            Ok(Self::Bare(s.to_string()))
        }
    }

    /// The concrete ref, reading a bare name with `provider`.
    pub fn to_ref(&self, provider: &str, kind: SecretKind) -> Result<SecretRef, RefError> {
        match self {
            Self::Ref(r) => Ok(r.clone()),
            Self::Bare(name) => SecretRef::bare(provider, name, kind == SecretKind::File),
        }
    }
}

impl From<SecretRef> for RefSpec {
    fn from(r: SecretRef) -> Self {
        Self::Ref(r)
    }
}

impl<'de> Deserialize<'de> for RefSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for RefSpec {
    fn schema_name() -> Cow<'static, str> {
        "SecretRef".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "`envfile:KEY`, `envfile-file:rel/path`, `lastpass://Folder/Item#field`, `op://vault/item/field` (1Password: not implemented yet), or a bare name read by the default provider.",
            "type": "string",
            "minLength": 1
        })
    }
}

/// A secret's refs with bare names resolved against the effective provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEntry {
    pub reference: SecretRef,
    pub fallback: Option<SecretRef>,
    pub kind: SecretKind,
    /// See [`SecretSpec::optional`].
    pub optional: bool,
}

/// A problem in `secrets.yml`. `line`/`column` are 1-based when known.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct SecretsConfigError {
    pub file: PathBuf,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
}

impl fmt::Display for SecretsConfigError {
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

impl SecretsConfigError {
    fn new(file: &Path, message: impl Into<String>) -> Self {
        Self {
            file: file.to_path_buf(),
            line: None,
            column: None,
            message: message.into(),
        }
    }
}

impl SecretsConfig {
    pub fn load(path: &Path) -> Result<Self, SecretsConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SecretsConfigError::new(path, format!("cannot read: {e}")))?;
        Self::from_yaml(&text, path)
    }

    /// Parses `text`; `file` is used in error messages only. An empty file is the default
    /// config (envfile, no secrets).
    pub fn from_yaml(text: &str, file: &Path) -> Result<Self, SecretsConfigError> {
        let is_blank = text
            .lines()
            .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'));
        if is_blank {
            return Ok(Self::default());
        }
        let config: Self = serde_yaml_ng::from_str(text).map_err(|err| {
            let mut message = err.to_string();
            let (line, column) = match err.location() {
                Some(loc) => {
                    let suffix = format!(" at line {} column {}", loc.line(), loc.column());
                    if let Some(stripped) = message.strip_suffix(&suffix) {
                        message = stripped.to_string();
                    }
                    (Some(loc.line()), Some(loc.column()))
                }
                None => (None, None),
            };
            SecretsConfigError {
                file: file.to_path_buf(),
                line,
                column,
                message,
            }
        })?;
        if !valid_provider_name(&config.default_provider) {
            return Err(SecretsConfigError::new(
                file,
                format!(
                    "default_provider `{}` is not a provider name",
                    config.default_provider
                ),
            ));
        }
        if let Some(bad) = config.secrets.keys().find(|n| !valid_secret_name(n)) {
            return Err(SecretsConfigError::new(
                file,
                format!("secret name `{bad}` must be letters, digits, `_`, `-` or `.`"),
            ));
        }
        Ok(config)
    }

    /// Applies a profile's `secrets_provider`: bare refs are read with it, and a secret whose
    /// `fallback` (but not `ref`) is on that provider tries the fallback first. So one mapping
    /// such as `{ ref: "lastpass://Dev/GitLab#password", fallback: "envfile:GITLAB_TOKEN" }`
    /// reads lastpass on a `secrets_provider: lastpass` profile and the envfile on a
    /// `secrets_provider: envfile` one, each keeping the other as its fallback. `None` keeps the
    /// config as it is.
    pub fn with_provider_override(mut self, provider: Option<&str>) -> Self {
        if let Some(p) = provider {
            self.default_provider = p.to_string();
            self.preferred = Some(p.to_string());
        }
        self
    }

    /// The effective refs for `name`: `None` if it is not declared.
    pub fn entry(&self, name: &str) -> Option<Result<SecretEntry, RefError>> {
        let spec = self.secrets.get(name)?;
        let provider = &self.default_provider;
        Some((|| {
            let mut reference = spec.reference.to_ref(provider, spec.kind)?;
            let mut fallback = spec
                .fallback
                .as_ref()
                .map(|f| f.to_ref(provider, spec.kind))
                .transpose()?;
            if let (Some(pref), Some(fb)) = (&self.preferred, fallback.as_mut())
                && reference.provider() != pref
                && fb.provider() == pref
            {
                std::mem::swap(&mut reference, fb);
            }
            Ok(SecretEntry {
                reference,
                fallback,
                kind: spec.kind,
                optional: spec.optional,
            })
        })())
    }

    /// Problems `dot check` should report: refs that do not resolve to a ref under the
    /// effective provider, and refs whose provider is not in `registry`. Never touches a
    /// provider.
    pub fn validate(&self, registry: &Registry) -> Vec<String> {
        let mut problems = Vec::new();
        for name in self.secrets.keys() {
            match self.entry(name) {
                Some(Ok(entry)) => {
                    for r in std::iter::once(&entry.reference).chain(entry.fallback.as_ref()) {
                        if !registry.contains(r.provider()) {
                            problems.push(format!(
                                "secret `{name}`: `{r}` needs provider `{}`, which {}",
                                r.provider(),
                                crate::registry::missing_provider_hint(r.provider())
                            ));
                        }
                    }
                }
                Some(Err(e)) => problems.push(format!("secret `{name}`: {e}")),
                None => {}
            }
        }
        problems
    }
}

fn valid_provider_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_secret_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// JSON Schema for `secrets.yml`.
pub fn json_schema() -> Schema {
    schemars::schema_for!(SecretsConfig)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<SecretsConfig, SecretsConfigError> {
        SecretsConfig::from_yaml(text, Path::new("secrets.yml"))
    }

    const PLAN_EXAMPLE: &str = r#"
default_provider: envfile
providers:
  envfile:   { path: "~/.config/dot/secrets.env" }
  # lastpass:  { username: me@example.com }
secrets:
  gitlab_token:   { ref: "envfile:GITLAB_TOKEN" }
  vpn_profile: { ref: "envfile-file:vpn/work.ovpn", kind: file }
  glab:           { ref: "lastpass://Dev/GitLab#password", fallback: "envfile:GITLAB_TOKEN" }
  npm:            { ref: NPM_TOKEN }
"#;

    #[test]
    fn parses_the_plan_example() {
        let c = parse(PLAN_EXAMPLE).unwrap();
        assert_eq!(c.default_provider, "envfile");
        assert_eq!(
            c.providers.envfile.as_ref().unwrap().path.as_deref(),
            Some("~/.config/dot/secrets.env")
        );
        let vpn = c.entry("vpn_profile").unwrap().unwrap();
        assert_eq!(vpn.kind, SecretKind::File);
        assert!(vpn.reference.is_file());
        let glab = c.entry("glab").unwrap().unwrap();
        assert_eq!(glab.reference.provider(), "lastpass");
        assert_eq!(glab.fallback.unwrap().as_str(), "envfile:GITLAB_TOKEN");
        assert_eq!(
            c.entry("npm").unwrap().unwrap().reference.as_str(),
            "envfile:NPM_TOKEN"
        );
        assert!(c.entry("nope").is_none());
    }

    #[test]
    fn provider_override_changes_bare_refs_only() {
        let c = parse(PLAN_EXAMPLE)
            .unwrap()
            .with_provider_override(Some("lastpass"));
        assert!(
            c.entry("npm")
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("full ref")
        );
        assert_eq!(
            c.entry("gitlab_token").unwrap().unwrap().reference.as_str(),
            "envfile:GITLAB_TOKEN"
        );
        let c = c.with_provider_override(Some("mock"));
        assert_eq!(
            c.entry("npm").unwrap().unwrap().reference.as_str(),
            "mock:NPM_TOKEN"
        );
        let c = c.with_provider_override(None);
        assert_eq!(c.default_provider, "mock");
    }

    #[test]
    fn bare_file_secret_uses_the_file_form() {
        let c = parse("secrets:\n  key: { ref: ssh/id.pem, kind: file }\n").unwrap();
        assert_eq!(
            c.entry("key").unwrap().unwrap().reference.as_str(),
            "envfile-file:ssh/id.pem"
        );
    }

    #[test]
    fn empty_file_is_the_default() {
        let c = parse("# nothing yet\n\n").unwrap();
        assert_eq!(c.default_provider, "envfile");
        assert!(c.secrets.is_empty());
    }

    #[test]
    fn errors_carry_line_numbers() {
        let e = parse("secrets:\n  a: { ref: \"envfile:A\" }\n  b: { ref: \"envfile:1\" }\n")
            .unwrap_err();
        assert_eq!(e.line, Some(3), "{e}");
        assert!(e.message.contains("env key"), "{e}");
        assert!(e.to_string().starts_with("secrets.yml:3:"), "{e}");

        let e = parse("secrets:\n  a: { ref: \"envfile:A\", knd: file }\n").unwrap_err();
        assert_eq!(e.line, Some(2));
        assert!(e.message.contains("unknown field `knd`"), "{e}");

        let e = parse("providers:\n  lastpas: {}\n").unwrap_err();
        assert!(e.message.contains("lastpas"), "{e}");

        let e = parse("secrets:\n  a: { ref: \"envfile:A\", kind: blob }\n").unwrap_err();
        assert!(e.message.contains("blob"), "{e}");

        let e = parse("secrets:\n  \"a b\": { ref: \"envfile:A\" }\n").unwrap_err();
        assert!(e.message.contains("a b"), "{e}");

        let e = parse("default_provider: Env File\n").unwrap_err();
        assert!(e.message.contains("default_provider"), "{e}");
    }

    #[test]
    fn validate_reports_unknown_providers_and_bare_problems() {
        let c = parse(
            "default_provider: lastpass\nsecrets:\n  a: { ref: A }\n  b: { ref: \"mock:B\" }\n  c: { ref: \"envfile:C\" }\n",
        )
        .unwrap();
        let reg = Registry::new();
        let problems = c.validate(&reg);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("secret `a`") && problems[0].contains("full ref"));
        assert!(problems[1].contains("provider `mock`"));
        assert!(problems[2].contains("provider `envfile`"));
    }

    #[test]
    fn schema_is_generated() {
        let s = serde_json::to_string(&json_schema()).unwrap();
        assert!(
            s.contains("default_provider") && s.contains("1password") && s.contains("SecretRef")
        );
    }
}
