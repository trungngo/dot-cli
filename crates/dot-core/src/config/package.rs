//! The `package:` directive's config: its YAML shape (with a hand-written `Deserialize` so every
//! mistake is reported where it is, with the line), its JSON Schema, and the validated model.
//!
//! Two forms:
//!
//! ```yaml
//! - package: [fd, ripgrep]            # short: this machine's manager (the `pkg` fact), names
//!                                     # mapped through dot.yml's `packages:`
//! - package:                          # step form: manager -> names, taken as written
//!     brew: [fnm]
//!     cask: [wezterm]
//!     apt: [fd-find]
//!     snap: [{ name: code, classic: true }]
//!     winget: [{ name: Git.Git, scope: machine }]   # for every user: needs an elevated terminal
//!     vscode: [ms-python.python]      # VS Code extensions, on every OS (`code` on PATH)
//!     upgrade: true                     # also upgrade the installed ones that are outdated
//! - package: { names: [fd], upgrade: true }   # the short form, with options
//! ```
//!
//! By default a step only installs what is missing; `upgrade: true` (or `dot apply --upgrade`
//! for every step) also upgrades what is installed but outdated.

use std::fmt;

use schemars::{JsonSchema, Schema, json_schema};
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

use super::shapes::{FromScalar, ScalarOr};

/// A package manager a `package:` step can name. `cask` is Homebrew's cask side (macOS apps);
/// `snap` falls back to the distro's manager where `snapd` is not available; `vscode` installs
/// VS Code extensions (through `code`) on every OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PkgManager {
    Brew,
    Cask,
    Apt,
    Dnf,
    Pacman,
    Zypper,
    Apk,
    Snap,
    Winget,
    Scoop,
    Vscode,
}

impl PkgManager {
    pub const ALL: [PkgManager; 11] = [
        PkgManager::Brew,
        PkgManager::Cask,
        PkgManager::Apt,
        PkgManager::Dnf,
        PkgManager::Pacman,
        PkgManager::Zypper,
        PkgManager::Apk,
        PkgManager::Snap,
        PkgManager::Winget,
        PkgManager::Scoop,
        PkgManager::Vscode,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PkgManager::Brew => "brew",
            PkgManager::Cask => "cask",
            PkgManager::Apt => "apt",
            PkgManager::Dnf => "dnf",
            PkgManager::Pacman => "pacman",
            PkgManager::Zypper => "zypper",
            PkgManager::Apk => "apk",
            PkgManager::Snap => "snap",
            PkgManager::Winget => "winget",
            PkgManager::Scoop => "scoop",
            PkgManager::Vscode => "vscode",
        }
    }

    pub fn parse(s: &str) -> Option<PkgManager> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// The manager a `pkg` fact value names (`brew`, `apt`, ...; never `cask`, `snap` or
    /// `vscode`).
    pub fn from_fact(pkg: &str) -> Option<PkgManager> {
        Self::parse(pkg)
            .filter(|m| !matches!(m, PkgManager::Cask | PkgManager::Snap | PkgManager::Vscode))
    }
}

impl fmt::Display for PkgManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of a manager's list in the step form.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PackageItemOpts {
    /// Package name as the manager knows it.
    pub name: String,
    /// snap only: install with `--classic` confinement.
    pub classic: Option<bool>,
    /// snap only: `false` for a snap that has no package in the distro's manager (microk8s),
    /// or whose name there is another program (Debian's `slack`): without snapd it is not
    /// installed with the distro's manager, and the step waits for (or fails without) snapd.
    /// Default true.
    pub fallback: Option<bool>,
    /// winget only: `machine` installs for every user (needs an elevated terminal, checked
    /// before anything runs); `user` (the default) lets the package decide.
    pub scope: Option<WingetScope>,
}

/// winget's `--scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WingetScope {
    User,
    Machine,
}

impl FromScalar for PackageItemOpts {
    fn from_scalar(name: String) -> Self {
        Self {
            name,
            classic: None,
            scope: None,
            fallback: None,
        }
    }
}

/// `package:` as written.
#[derive(Debug, Clone)]
pub struct PackageField(pub PackageStep);

/// A validated `package:` step: what to install, and whether to upgrade.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageStep {
    pub spec: PackageSpec,
    /// Also upgrade installed packages that are outdated (`upgrade: true`).
    pub upgrade: bool,
}

/// A validated `package:` step.
#[derive(Debug, Clone, PartialEq)]
pub enum PackageSpec {
    /// `package: [fd, ripgrep]`: names for this machine's package manager, each mapped through
    /// `dot.yml`'s `packages:` (a name without a mapping is used as is).
    Names(Vec<String>),
    /// `package: {brew: [...], apt: [...]}`: per manager, in file order. Only the managers that
    /// belong to this machine are used.
    ByManager(Vec<(PkgManager, Vec<PackageItem>)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageItem {
    pub name: String,
    /// snap `--classic`.
    pub classic: bool,
    /// winget `--scope machine`.
    pub machine: bool,
    /// snap: installed with the distro's manager where snapd is not available (default).
    pub fallback: bool,
}

impl PackageItem {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            classic: false,
            machine: false,
            fallback: true,
        }
    }
}

fn check_name<E: de::Error>(name: &str) -> Result<(), E> {
    if name.trim().is_empty() {
        return Err(E::custom("a package name is empty"));
    }
    if name.chars().any(char::is_whitespace) && !crate::template::is_template(name) {
        return Err(E::custom(format!(
            "`{name}` is not a package name (it contains whitespace); list each \
             package separately"
        )));
    }
    Ok(())
}

impl<'de> Deserialize<'de> for PackageField {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V_;
        impl<'de> Visitor<'de> for V_ {
            type Value = PackageField;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a list of package names or a mapping of package manager to names")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut out: Vec<String> = Vec::new();
                while let Some(name) = seq.next_element::<String>()? {
                    check_name::<A::Error>(&name)?;
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
                Ok(PackageField(PackageStep {
                    spec: PackageSpec::Names(names_of::<A::Error>(out)?),
                    upgrade: false,
                }))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out: Vec<(PkgManager, Vec<PackageItem>)> = Vec::new();
                let mut upgrade: Option<bool> = None;
                let mut names: Option<Vec<String>> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "upgrade" {
                        if upgrade.is_some() {
                            return Err(de::Error::custom("duplicate key `upgrade`"));
                        }
                        upgrade = Some(map.next_value()?);
                        continue;
                    }
                    if key == "names" {
                        if names.is_some() {
                            return Err(de::Error::custom("duplicate key `names`"));
                        }
                        let raw: Vec<String> = map.next_value()?;
                        let mut list: Vec<String> = Vec::new();
                        for n in raw {
                            check_name::<A::Error>(&n)?;
                            if !list.contains(&n) {
                                list.push(n);
                            }
                        }
                        names = Some(names_of::<A::Error>(list)?);
                        continue;
                    }
                    let Some(manager) = PkgManager::parse(&key) else {
                        let known: Vec<&str> = PkgManager::ALL.iter().map(|m| m.as_str()).collect();
                        return Err(de::Error::custom(format!(
                            "unknown package manager `{key}`, expected one of: {} (or `names`, \
                             `upgrade`)",
                            known.join(", ")
                        )));
                    };
                    if out.iter().any(|(m, _)| *m == manager) {
                        return Err(de::Error::custom(format!("duplicate key `{key}`")));
                    }
                    let raw: Vec<ScalarOr<PackageItemOpts>> = map.next_value()?;
                    if raw.is_empty() {
                        return Err(de::Error::custom(format!(
                            "`{key}`: expected at least one package"
                        )));
                    }
                    let mut items: Vec<PackageItem> = Vec::new();
                    for ScalarOr(o) in raw {
                        check_name::<A::Error>(&o.name)?;
                        if o.classic.is_some() && manager != PkgManager::Snap {
                            return Err(de::Error::custom(format!(
                                "`{key}`: `classic` is only for snap packages"
                            )));
                        }
                        if o.fallback.is_some() && manager != PkgManager::Snap {
                            return Err(de::Error::custom(format!(
                                "`{key}`: `fallback` is only for snap packages"
                            )));
                        }
                        if o.scope.is_some() && manager != PkgManager::Winget {
                            return Err(de::Error::custom(format!(
                                "`{key}`: `scope` is only for winget packages"
                            )));
                        }
                        if !items.iter().any(|i| i.name == o.name) {
                            items.push(PackageItem {
                                name: o.name,
                                classic: o.classic.unwrap_or(false),
                                machine: o.scope == Some(WingetScope::Machine),
                                fallback: o.fallback.unwrap_or(true),
                            });
                        }
                    }
                    out.push((manager, items));
                }
                let upgrade = upgrade.unwrap_or(false);
                let spec = match (names, out.is_empty()) {
                    (Some(_), false) => {
                        return Err(de::Error::custom(
                            "`names` (this machine's manager) and package manager keys cannot \
                             be combined; use one or the other",
                        ));
                    }
                    (Some(n), true) => PackageSpec::Names(n),
                    (None, false) => PackageSpec::ByManager(out),
                    (None, true) => {
                        return Err(de::Error::custom(
                            "expected at least one package manager (or `names`)",
                        ));
                    }
                };
                Ok(PackageField(PackageStep { spec, upgrade }))
            }
        }
        d.deserialize_any(V_)
    }
}

fn names_of<E: de::Error>(names: Vec<String>) -> Result<Vec<String>, E> {
    if names.is_empty() {
        return Err(E::custom("expected at least one package"));
    }
    Ok(names)
}

/// The JSON Schema of `package:` (used by the hand-written step schema).
pub fn package_schema() -> Schema {
    let name = json_schema!({ "type": "string", "minLength": 1, "pattern": "^\\S+$|\\{\\{|\\{%" });
    let list = json_schema!({ "type": "array", "items": name, "minItems": 1 });
    let snap_item = json_schema!({
        "anyOf": [
            name,
            {
                "type": "object",
                "properties": {
                    "name": name,
                    "classic": { "type": "boolean", "description": "Install with --classic confinement." },
                    "fallback": { "type": "boolean", "description": "false: never install it with the distro's package manager when snapd is not available (a snap-only package)." }
                },
                "required": ["name"],
                "additionalProperties": false
            }
        ]
    });
    let snap = json_schema!({
        "type": "array",
        "items": snap_item,
        "minItems": 1,
        "description": "Snaps; installed with the distro's package manager where snapd is not available."
    });
    let winget = json_schema!({
        "type": "array",
        "items": {
            "anyOf": [
                name,
                {
                    "type": "object",
                    "properties": {
                        "name": name,
                        "scope": {
                            "enum": ["user", "machine"],
                            "description": "`machine`: install for every user (needs an elevated terminal)."
                        }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }
            ]
        },
        "minItems": 1,
        "description": "winget package ids (Windows)."
    });
    let mut props = serde_json::Map::new();
    for m in PkgManager::ALL {
        let schema = if m == PkgManager::Snap {
            snap.clone()
        } else if m == PkgManager::Winget {
            winget.clone()
        } else {
            let mut l = list.clone();
            if m == PkgManager::Cask {
                l.insert(
                    "description".into(),
                    "Homebrew casks (macOS); `--adopt` is added for casks that install an app."
                        .into(),
                );
            }
            l
        };
        props.insert(m.as_str().to_string(), schema.into());
    }
    props.insert(
        "upgrade".into(),
        serde_json::json!({ "type": "boolean", "description": "Also upgrade installed packages that are outdated (default: false)." }),
    );
    let mut names = list.clone();
    names.insert(
        "description".into(),
        "Names for this machine's package manager, as in the list form (not with manager keys)."
            .into(),
    );
    props.insert("names".into(), names.into());
    json_schema!({
        "description": "Packages to install: a list of names for this machine's package manager (mapped through dot.yml's `packages:`), or a mapping of package manager to names (plus `upgrade`).",
        "anyOf": [
            list,
            {
                "type": "object",
                "properties": props,
                "additionalProperties": false,
                "minProperties": 1
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigError, Directive, parse_bundle};

    fn parse_step(yaml: &str) -> Result<PackageStep, ConfigError> {
        let file = crate::paths::tests::abs("/nonexistent-dot-test/bundles/demo/bundle.yml");
        let b = parse_bundle(yaml, &file)?;
        match &b.steps[0].directive {
            Directive::Package(p) => Ok(p.clone()),
            other => panic!("{other:?}"),
        }
    }

    fn parse(yaml: &str) -> Result<PackageSpec, ConfigError> {
        parse_step(yaml).map(|p| {
            assert!(!p.upgrade);
            p.spec
        })
    }

    #[test]
    fn upgrade_and_names() {
        let p = parse_step("steps:\n  - package: { apt: [fd-find], upgrade: true }\n").unwrap();
        assert!(p.upgrade);
        assert_eq!(
            p.spec,
            PackageSpec::ByManager(vec![(PkgManager::Apt, vec![PackageItem::new("fd-find")])])
        );
        let p =
            parse_step("steps:\n  - package: { names: [fd, jq, fd], upgrade: true }\n").unwrap();
        assert_eq!(p.spec, PackageSpec::Names(vec!["fd".into(), "jq".into()]));
        assert!(p.upgrade);
        let e = err("steps:\n  - package: { names: [fd], brew: [jq] }\n");
        assert!(e.message.contains("cannot be combined"), "{e}");
        let e = err("steps:\n  - package: { upgrade: true }\n");
        assert!(e.message.contains("at least one package manager"), "{e}");
        let e = err("steps:\n  - package: { names: [], upgrade: true }\n");
        assert!(e.message.contains("at least one package"), "{e}");
        let e = err("steps:\n  - package: { apt: [x], upgrade: yes }\n");
        assert!(e.message.contains("invalid type"), "{e}");
    }

    fn err(yaml: &str) -> ConfigError {
        match parse(yaml) {
            Ok(p) => panic!("expected an error, parsed {p:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn short_form() {
        assert_eq!(
            parse("steps:\n  - package: [fd, ripgrep, fd]\n").unwrap(),
            PackageSpec::Names(vec!["fd".into(), "ripgrep".into()])
        );
        assert_eq!(
            parse("steps:\n  - package: [\"{{ editor }}\"]\n").unwrap(),
            PackageSpec::Names(vec!["{{ editor }}".into()])
        );
    }

    #[test]
    fn step_form() {
        let p = parse(
            "steps:\n  - package:\n      brew: [fnm]\n      cask: [wezterm]\n      snap: [hello, { name: code, classic: true }]\n      apt: [fd-find]\n",
        )
        .unwrap();
        let PackageSpec::ByManager(m) = p else {
            panic!()
        };
        let keys: Vec<PkgManager> = m.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            [
                PkgManager::Brew,
                PkgManager::Cask,
                PkgManager::Snap,
                PkgManager::Apt
            ]
        );
        assert_eq!(
            m[2].1,
            [
                PackageItem::new("hello"),
                PackageItem {
                    name: "code".into(),
                    classic: true,
                    machine: false,
                    fallback: true,
                }
            ]
        );
    }

    #[test]
    fn winget_scope() {
        let p = parse(
            "steps:\n  - package:\n      winget: [Git.Git, { name: Microsoft.PowerToys, scope: machine }, { name: X.Y, scope: user }]\n",
        )
        .unwrap();
        let PackageSpec::ByManager(m) = p else {
            panic!()
        };
        let machine: Vec<bool> = m[0].1.iter().map(|i| i.machine).collect();
        assert_eq!(machine, [false, true, false]);
        let schema = serde_json::to_value(crate::config::json_schema()).unwrap();
        let w =
            &schema["$defs"]["Step"]["properties"]["package"]["anyOf"][1]["properties"]["winget"];
        assert_eq!(
            w["items"]["anyOf"][1]["properties"]["scope"]["enum"][1],
            "machine"
        );
    }

    #[test]
    fn mistakes_are_errors_with_lines() {
        let e = err("steps:\n  - package:\n      brew: [fnm]\n      brw: [x]\n");
        assert!(e.message.contains("unknown package manager `brw`"), "{e}");
        assert!(e.message.contains("brew, cask, apt"), "{e}");
        assert_eq!(e.line, Some(3), "{e}");

        let e = err("steps:\n  - package:\n      apt:\n        - { name: x, classic: true }\n");
        assert!(e.message.contains("`classic` is only for snap"), "{e}");
        assert!(e.line.is_some(), "{e}");

        let e = err("steps:\n  - package:\n      apt:\n        - { name: x, scope: machine }\n");
        assert!(e.message.contains("`scope` is only for winget"), "{e}");

        let e = err("steps:\n  - package:\n      snap:\n        - { name: x, clasic: true }\n");
        assert!(e.message.contains("unknown field `clasic`"), "{e}");
        assert_eq!(e.line, Some(4), "{e}");

        let e = err("steps:\n  - package: []\n");
        assert!(e.message.contains("at least one package"), "{e}");
        let e = err("steps:\n  - package: { apt: [] }\n");
        assert!(
            e.message.contains("`apt`: expected at least one package"),
            "{e}"
        );
        let e = err("steps:\n  - package: {}\n");
        assert!(e.message.contains("at least one package manager"), "{e}");
        let e = err("steps:\n  - package: [\"fd ripgrep\"]\n");
        assert!(e.message.contains("list each package separately"), "{e}");
        let e = err("steps:\n  - package: { brew: [a], brew: [b] }\n");
        assert!(e.message.contains("duplicate"), "{e}");
        let e = err("steps:\n  - package: fd\n");
        assert!(e.message.contains("a list of package names"), "{e}");
        let e = err("steps:\n  - package: [fd]\n    shell: x\n");
        assert!(e.message.contains("found 2: shell, package"), "{e}");
        assert_eq!(e.line, Some(2), "{e}");
    }

    #[test]
    fn schema_covers_every_manager() {
        let schema = serde_json::to_value(crate::config::json_schema()).unwrap();
        let p = &schema["$defs"]["Step"]["properties"]["package"];
        let alts = p["anyOf"].as_array().unwrap();
        assert_eq!(alts[0]["type"], "array");
        let props = alts[1]["properties"].as_object().unwrap();
        let keys: Vec<&str> = props.keys().map(String::as_str).collect();
        for m in PkgManager::ALL {
            assert!(keys.contains(&m.as_str()), "{m}");
        }
        assert_eq!(alts[1]["additionalProperties"], serde_json::json!(false));
        assert_eq!(props["snap"]["items"]["anyOf"][1]["required"][0], "name");
    }
}
