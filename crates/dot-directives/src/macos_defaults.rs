//! `macos_defaults`: set macOS preferences with `defaults write`. Satisfied when
//! `defaults read-type` and `defaults read` already report the wanted type and value. macOS
//! only: planning it on another system is a config error (put `when: { os: macos }` on the
//! step or bundle).

use anyhow::{Context as _, Result};
use dot_core::config::{DefaultsType, MacosDefault};
use dot_core::facts::Os;
use dot_core::{Action, Outcome, PlanCtx, Resource, RunCtx, Status, Undo};
use dot_platform::exec::{Cmd, System};

pub fn plan(
    entries: &[MacosDefault],
    ctx: &PlanCtx<'_>,
    sys: &System,
) -> Result<Vec<Box<dyn Action>>> {
    if ctx.facts.os != Os::Macos {
        return Err(ctx.config_error(format!(
            "macos_defaults only works on macOS (this is {}); add `when: {{ os: macos }}` to \
             the step or its bundle",
            ctx.facts.os.as_str()
        )));
    }
    let text = |what: &str, s: &str| -> Result<String> {
        let r = ctx.render_text(s, &format!("macos_defaults {what}"))?;
        if !r.secrets.is_empty() {
            return Err(ctx.config_error(format!(
                "macos_defaults {what}: secret() cannot be used here: it would appear in a \
                 command line"
            )));
        }
        Ok(r.text)
    };
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
    for d in entries {
        let domain = text("domain", &d.domain)?;
        let key = text("key", &d.key)?;
        let value =
            d.ty.normalize(&text("value", &d.value)?)
                .map_err(|e| ctx.config_error(e))?;
        actions.push(Box::new(DefaultsAction {
            domain,
            key,
            ty: d.ty,
            value,
            current_host: d.current_host,
            sys: sys.clone(),
        }));
    }
    Ok(actions)
}

pub struct DefaultsAction {
    pub domain: String,
    pub key: String,
    pub ty: DefaultsType,
    /// Canonical for `ty` (see [`DefaultsType::normalize`]).
    pub value: String,
    pub current_host: bool,
    sys: System,
}

impl DefaultsAction {
    fn defaults(&self, verb: &str) -> Cmd {
        let c = Cmd::new("defaults");
        let c = if self.current_host {
            c.arg("-currentHost")
        } else {
            c
        };
        c.args([verb, &self.domain, &self.key])
    }

    fn write_cmd(&self) -> Cmd {
        self.defaults("write")
            .args([format!("-{}", self.ty.as_str()), self.value.clone()])
    }

    /// Whether `defaults read` output `have` is the wanted value.
    fn same_value(&self, have: &str) -> bool {
        match self.ty {
            DefaultsType::Bool => matches!(
                (have.trim(), self.value.as_str()),
                ("1", "true") | ("0", "false")
            ),
            DefaultsType::Int => have.trim().parse::<i64>().ok() == self.value.parse().ok(),
            DefaultsType::Float => match (have.trim().parse::<f64>(), self.value.parse::<f64>()) {
                (Ok(a), Ok(b)) => (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0),
                _ => false,
            },
            DefaultsType::String => have.strip_suffix('\n').unwrap_or(have) == self.value,
        }
    }
}

impl Action for DefaultsAction {
    fn describe(&self) -> String {
        self.write_cmd().line()
    }

    fn status(&self) -> Result<Status> {
        let read_type = self.defaults("read-type");
        let out = match self.sys.run(&read_type) {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Status::Unknown),
            Err(e) => return Err(e).with_context(|| format!("running `{}`", read_type.line())),
        };
        // Not set at all, or set with another type.
        let want = format!("Type is {}", self.ty.read_type_name());
        if !out.success() || out.stdout.trim() != want {
            return Ok(Status::Pending);
        }
        let read = self.defaults("read");
        let out = self
            .sys
            .run(&read)
            .with_context(|| format!("running `{}`", read.line()))?;
        Ok(if out.success() && self.same_value(&out.stdout) {
            Status::Satisfied
        } else {
            Status::Pending
        })
    }

    /// `defaults:<domain> <key>`. The earlier value is not known: undone by hand.
    fn resources(&self) -> Vec<Resource> {
        let host = if self.current_host {
            "-currentHost "
        } else {
            ""
        };
        vec![
            Resource::new("defaults", format!("{} {}", self.domain, self.key)).with_undo(
                Undo::Manual {
                    note: format!(
                        "set it back by hand, or `defaults {host}delete {} {}` for the system \
                         default",
                        self.domain, self.key
                    ),
                },
            ),
        ]
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        crate::syscmd::run(&self.sys, &self.write_cmd(), ctx)?;
        Ok(Outcome::Changed)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use dot_platform::exec::Output;

    use super::*;
    use crate::testutil::Fixture;
    use crate::testutil_sys::{fake_builtins, plan_on};

    const YAML: &str = r#"steps:
  - macos_defaults:
      - { domain: com.apple.dock, key: autohide, type: bool, value: true }
      - { domain: com.apple.dock, key: tilesize, type: int, value: 48 }
      - { domain: NSGlobalDomain, key: AppleInterfaceStyle, type: string, value: Dark }
      - { domain: com.apple.x, key: speed, type: float, value: "{{ speed }}", current_host: true }
"#;

    #[test]
    fn reads_compares_and_writes() {
        let f = Fixture::new(YAML);
        let (b, fake) = fake_builtins();
        // `speed` is undefined: a located error.
        let err = format!("{:#}", plan_on(&f, &b, Os::Macos, 0).err().unwrap());
        assert!(
            err.contains("bundle.yml:2:5: steps[0]:") && err.contains("`speed`"),
            "{err}"
        );

        let f = Fixture::new(&YAML.replace("{{ speed }}", "0.5"));
        let actions = plan_on(&f, &b, Os::Macos, 0).unwrap();
        let described: Vec<String> = actions.iter().map(|a| a.describe()).collect();
        assert_eq!(
            described,
            [
                "defaults write com.apple.dock autohide -bool true",
                "defaults write com.apple.dock tilesize -int 48",
                "defaults write NSGlobalDomain AppleInterfaceStyle -string Dark",
                "defaults -currentHost write com.apple.x speed -float 0.5",
            ]
        );
        // Nothing set yet.
        fake.on("defaults", Output::exit(1, ""));
        assert!(
            actions
                .iter()
                .all(|a| a.status().unwrap() == Status::Pending)
        );
        // Set, with the right types and values.
        fake.on(
            "defaults read-type com.apple.dock autohide",
            Output::ok("Type is boolean\n"),
        );
        fake.on("defaults read com.apple.dock autohide", Output::ok("1\n"));
        fake.on(
            "defaults read-type com.apple.dock tilesize",
            Output::ok("Type is integer\n"),
        );
        fake.on("defaults read com.apple.dock tilesize", Output::ok("48\n"));
        fake.on(
            "defaults read-type NSGlobalDomain",
            Output::ok("Type is string\n"),
        );
        fake.on("defaults read NSGlobalDomain", Output::ok("Dark\n"));
        fake.on(
            "defaults -currentHost read-type",
            Output::ok("Type is float\n"),
        );
        fake.on(
            "defaults -currentHost read com.apple.x",
            Output::ok("0.50000\n"),
        );
        assert!(
            actions
                .iter()
                .all(|a| a.status().unwrap() == Status::Satisfied)
        );
        // A different value, or the same digits with another type, is pending.
        fake.on("defaults read com.apple.dock autohide", Output::ok("0\n"));
        fake.on(
            "defaults read-type com.apple.dock tilesize",
            Output::ok("Type is string\n"),
        );
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(actions[1].status().unwrap(), Status::Pending);

        fake.on("defaults write", Output::ok(""));
        fake.clear_calls();
        f.apply(&actions[..1]);
        assert_eq!(
            fake.calls(),
            ["defaults write com.apple.dock autohide -bool true"]
        );
    }

    #[test]
    fn other_systems_are_a_config_error() {
        let f = Fixture::new(YAML);
        let (b, fake) = fake_builtins();
        let err = format!("{:#}", plan_on(&f, &b, Os::Linux, 0).err().unwrap());
        assert!(err.contains("only works on macOS (this is linux)"), "{err}");
        assert!(err.contains("bundle.yml:2:5"), "{err}");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn values_must_fit_their_type_when_parsed() {
        let f = Fixture::new("steps: []\n");
        for (yaml, want) in [
            (
                "{ domain: d, key: k, type: bool, value: yes }",
                "`yes` is not a valid bool value",
            ),
            (
                "{ domain: d, key: k, type: int, value: 1.5 }",
                "`1.5` is not a valid int value",
            ),
            (
                "{ domain: d, key: k, type: date, value: x }",
                "unknown variant `date`",
            ),
            (
                "{ domain: d, key: k, type: int, value: 1, extra: 2 }",
                "unknown field `extra`",
            ),
            (
                "{ domain: '', key: k, type: int, value: 1 }",
                "must not be empty",
            ),
        ] {
            let file = f.root().join("bad.yml");
            std::fs::write(&file, format!("steps:\n  - macos_defaults: {yaml}\n")).unwrap();
            let err = dot_core::config::load_bundle(&file)
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{yaml}: {err}");
            assert!(err.contains("bad.yml:2:"), "{err}");
        }
    }
}
