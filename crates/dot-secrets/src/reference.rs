//! Provider references: where a secret lives, never what it is.
//!
//! | ref | provider | locator | file |
//! |---|---|---|---|
//! | `envfile:GITLAB_TOKEN` | `envfile` | `GITLAB_TOKEN` | no |
//! | `envfile-file:vpn/work.ovpn` | `envfile` | `vpn/work.ovpn` | yes |
//! | `lastpass://Dev/GitLab#password` | `lastpass` | `Dev/GitLab#password` | no |
//! | `lastpass://Dev/VPN#attach=work.ovpn` | `lastpass` | `Dev/VPN#attach=work.ovpn` | yes |
//! | `op://Private/GitLab/token` | `1password` | `Private/GitLab/token` | no |
//! | `<name>:<locator>` / `<name>-file:<locator>` | `<name>` | `<locator>` | no / yes |
//!
//! The last row is how any other registered provider (the in-memory mock, future plugins) is
//! addressed. Whether its provider exists is checked against a [`crate::Registry`], not here.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub(crate) const ENVFILE: &str = "envfile";
pub(crate) const LASTPASS: &str = "lastpass";
pub(crate) const ONEPASSWORD: &str = "1password";

/// A parsed provider reference such as `envfile:GITLAB_TOKEN` or
/// `lastpass://Dev/GitLab#password`. Refs are not secret: `Debug` and `Display` show them.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretRef {
    raw: String,
    provider: String,
    locator: String,
    file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid secret ref `{reference}`: {reason}")]
pub struct RefError {
    pub reference: String,
    pub reason: String,
}

impl SecretRef {
    pub fn parse(s: &str) -> Result<Self, RefError> {
        let err = |reason: &str| RefError {
            reference: s.to_string(),
            reason: reason.to_string(),
        };
        if s.is_empty() {
            return Err(err("empty"));
        }
        if s.trim() != s {
            return Err(err("leading or trailing whitespace"));
        }
        let Some((scheme, body)) = s.split_once(':') else {
            return Err(err(
                "no provider prefix (expected e.g. `envfile:KEY` or `lastpass://Folder/Item#password`)",
            ));
        };
        if !valid_scheme(scheme) {
            return Err(err(
                "provider prefix must be lowercase letters, digits and `-`",
            ));
        }
        let (provider, locator, file) = match scheme {
            "envfile" => {
                check_env_key(body).map_err(|r| err(&r))?;
                (ENVFILE, body, false)
            }
            "envfile-file" => {
                check_rel_path(body).map_err(|r| err(&r))?;
                (ENVFILE, body, true)
            }
            "lastpass" => {
                let rest = body
                    .strip_prefix("//")
                    .ok_or_else(|| err("expected `lastpass://<Folder>/<Item>#<field>`"))?;
                let file = check_lastpass(rest).map_err(|r| err(&r))?;
                (LASTPASS, rest, file)
            }
            "op" => {
                let rest = body
                    .strip_prefix("//")
                    .ok_or_else(|| err("expected `op://<vault>/<item>/<field>`"))?;
                check_op(rest).map_err(|r| err(&r))?;
                (ONEPASSWORD, rest, false)
            }
            _ => {
                let (provider, file) = match scheme.strip_suffix("-file") {
                    Some(p) if !p.is_empty() => (p, true),
                    _ => (scheme, false),
                };
                if matches!(provider, LASTPASS | ONEPASSWORD | "op") {
                    return Err(err(match provider {
                        LASTPASS => "expected `lastpass://<Folder>/<Item>#<field>`",
                        _ => "1Password refs use `op://<vault>/<item>/<field>`",
                    }));
                }
                if body.is_empty() {
                    return Err(err("nothing after the provider prefix"));
                }
                (provider, body, file)
            }
        };
        Ok(Self {
            raw: s.to_string(),
            provider: provider.to_string(),
            locator: locator.to_string(),
            file,
        })
    }

    /// `envfile:<key>`.
    pub fn envfile(key: &str) -> Result<Self, RefError> {
        Self::parse(&format!("envfile:{key}"))
    }

    /// `envfile-file:<rel/path>`.
    pub fn envfile_file(path: &str) -> Result<Self, RefError> {
        Self::parse(&format!("envfile-file:{path}"))
    }

    /// The ref a bare name (`ref: GITLAB_TOKEN`, no prefix) stands for under `provider`.
    /// `lastpass` and `1password` have no bare form: their refs name a vault item.
    pub fn bare(provider: &str, name: &str, file: bool) -> Result<Self, RefError> {
        let text = match provider {
            ENVFILE if file => format!("envfile-file:{name}"),
            LASTPASS | ONEPASSWORD => {
                return Err(RefError {
                    reference: name.to_string(),
                    reason: format!(
                        "provider `{provider}` needs a full ref (e.g. {}), not a bare name",
                        if provider == LASTPASS {
                            "`lastpass://Folder/Item#password`"
                        } else {
                            "`op://vault/item/field`"
                        }
                    ),
                });
            }
            p if file => format!("{p}-file:{name}"),
            p => format!("{p}:{name}"),
        };
        Self::parse(&text)
    }

    /// The provider that serves this ref (`envfile`, `lastpass`, `1password`, …).
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The provider-specific part: an env key, a relative path, `Folder/Item#field`, ….
    pub fn locator(&self) -> &str {
        &self.locator
    }

    /// Whether the ref names a whole file (`envfile-file:`, `#attach=`).
    pub fn is_file(&self) -> bool {
        self.file
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretRef").field(&self.raw).finish()
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FromStr for SecretRef {
    type Err = RefError;
    fn from_str(s: &str) -> Result<Self, RefError> {
        Self::parse(s)
    }
}

impl Serialize for SecretRef {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

fn valid_scheme(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// An environment-variable style key, as dotenv accepts it.
pub(crate) fn valid_env_key(key: &str) -> bool {
    key.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn check_env_key(key: &str) -> Result<(), String> {
    if valid_env_key(key) {
        Ok(())
    } else {
        Err("expected an env key (letters, digits, `_`; not starting with a digit)".into())
    }
}

/// A relative path with `/` separators that stays inside its base directory.
pub(crate) fn check_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if path.contains('\\') {
        return Err("use `/` as the path separator".into());
    }
    if path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') {
        return Err("path must be relative".into());
    }
    for part in path.split('/') {
        match part {
            "" | "." => return Err("path has an empty or `.` component".into()),
            ".." => return Err("path must not contain `..`".into()),
            _ => {}
        }
    }
    Ok(())
}

/// `<Folder>/<Item>#<field>` or `id/<n>#<field>`; returns whether it names an attachment.
fn check_lastpass(rest: &str) -> Result<bool, String> {
    let Some((item, field)) = rest.rsplit_once('#') else {
        return Err(
            "missing `#password`, `#username`, `#notes`, `#field=<name>` or `#attach=<file>`"
                .into(),
        );
    };
    if item.is_empty() || item.split('/').any(str::is_empty) {
        return Err("expected `<Folder>/<Item>` (or `id/<n>`) before `#`".into());
    }
    if let Some(id) = item.strip_prefix("id/")
        && !id.chars().all(|c| c.is_ascii_digit())
    {
        return Err("`id/` must be followed by a numeric item id".into());
    }
    match field {
        "password" | "username" | "notes" => Ok(false),
        f if f.strip_prefix("field=").is_some_and(|n| !n.is_empty()) => Ok(false),
        f if f.strip_prefix("attach=").is_some_and(|n| !n.is_empty()) => Ok(true),
        _ => Err(format!(
            "unknown field selector `#{field}` (expected password, username, notes, field=<name> or attach=<file>)"
        )),
    }
}

/// `<vault>/<item>[/<section>]/<field>`, optionally followed by `?<query>`.
fn check_op(rest: &str) -> Result<(), String> {
    let path = rest.split_once('?').map_or(rest, |(p, _)| p);
    let parts: Vec<&str> = path.split('/').collect();
    if !(3..=4).contains(&parts.len()) || parts.iter().any(|p| p.is_empty()) {
        return Err("expected `op://<vault>/<item>/<field>`".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> SecretRef {
        SecretRef::parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn bad(s: &str) -> String {
        SecretRef::parse(s).expect_err(s).to_string()
    }

    #[test]
    fn envfile_refs() {
        let r = ok("envfile:GITLAB_TOKEN");
        assert_eq!(
            (r.provider(), r.locator(), r.is_file()),
            ("envfile", "GITLAB_TOKEN", false)
        );
        let r = ok("envfile-file:vpn/work.ovpn");
        assert_eq!(
            (r.provider(), r.locator(), r.is_file()),
            ("envfile", "vpn/work.ovpn", true)
        );
        assert_eq!(r.to_string(), "envfile-file:vpn/work.ovpn");
        assert_eq!(
            format!("{r:?}"),
            "SecretRef(\"envfile-file:vpn/work.ovpn\")"
        );
    }

    #[test]
    fn envfile_refs_are_validated() {
        assert!(bad("envfile:").contains("env key"));
        assert!(bad("envfile:1ABC").contains("env key"));
        assert!(bad("envfile:A-B").contains("env key"));
        assert!(bad("envfile-file:../x").contains(".."));
        assert!(bad("envfile-file:/etc/passwd").contains("relative"));
        assert!(bad("envfile-file:C:/x").contains("relative"));
        assert!(bad("envfile-file:a//b").contains("empty"));
        assert!(bad("envfile-file:a\\b").contains("separator"));
    }

    #[test]
    fn lastpass_refs_parse() {
        let r = ok("lastpass://Dev/GitLab#password");
        assert_eq!(
            (r.provider(), r.locator(), r.is_file()),
            ("lastpass", "Dev/GitLab#password", false)
        );
        assert!(ok("lastpass://Dev/VPN#attach=work.ovpn").is_file());
        assert!(!ok("lastpass://id/1234#field=Token").is_file());
        assert!(
            ok("lastpass://Shared-Team/Sub/Item#notes")
                .locator()
                .starts_with("Shared-Team")
        );
        assert!(bad("lastpass:Dev/GitLab#password").contains("lastpass://"));
        assert!(bad("lastpass://Dev/GitLab").contains("#password"));
        assert!(bad("lastpass://Dev/GitLab#pin").contains("unknown field"));
        assert!(bad("lastpass://id/abc#password").contains("numeric"));
        assert!(bad("lastpass://#password").contains("Folder"));
    }

    #[test]
    fn onepassword_refs_parse() {
        let r = ok("op://Private/GitLab/token");
        assert_eq!(
            (r.provider(), r.locator()),
            ("1password", "Private/GitLab/token")
        );
        ok("op://Private/GitLab/Section/token");
        ok("op://Private/GitLab/one-time password?attribute=otp");
        assert!(bad("op://Private/GitLab").contains("op://"));
        assert!(bad("op:Private/GitLab/token").contains("op://"));
        assert!(bad("1password:x").contains("op://"));
    }

    #[test]
    fn other_providers_use_the_generic_form() {
        let r = ok("mock:KEY");
        assert_eq!(
            (r.provider(), r.locator(), r.is_file()),
            ("mock", "KEY", false)
        );
        let r = ok("mock-file:a/b");
        assert_eq!((r.provider(), r.is_file()), ("mock", true));
        assert!(bad("GITLAB_TOKEN").contains("no provider prefix"));
        assert!(bad("Mock:x").contains("lowercase"));
        assert!(bad("mock:").contains("nothing after"));
        assert!(bad(" envfile:A").contains("whitespace"));
        assert!(bad("").contains("empty"));
        assert!(bad("lastpass-file:x").contains("lastpass://"));
    }

    #[test]
    fn bare_names_expand_per_provider() {
        assert_eq!(
            SecretRef::bare("envfile", "TOKEN", false).unwrap().as_str(),
            "envfile:TOKEN"
        );
        assert_eq!(
            SecretRef::bare("envfile", "a/b.pem", true)
                .unwrap()
                .as_str(),
            "envfile-file:a/b.pem"
        );
        assert_eq!(
            SecretRef::bare("mock", "K", false).unwrap().as_str(),
            "mock:K"
        );
        assert!(
            SecretRef::bare("lastpass", "TOKEN", false)
                .unwrap_err()
                .to_string()
                .contains("full ref")
        );
        assert!(SecretRef::bare("1password", "TOKEN", false).is_err());
    }

    #[test]
    fn serde_round_trip() {
        let r: SecretRef = serde_yaml_ng::from_str("\"envfile:A\"").unwrap();
        assert_eq!(r, ok("envfile:A"));
        assert_eq!(serde_yaml_ng::to_string(&r).unwrap().trim(), "envfile:A");
        assert!(serde_yaml_ng::from_str::<SecretRef>("\"nope\"").is_err());
    }
}
