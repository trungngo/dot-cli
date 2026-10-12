//! Evaluating `when:` conditions against [`Facts`].

use crate::config::When;
use crate::facts::Facts;
use crate::template::{Vars, is_template};

impl When {
    /// Renders templated values and validates them the way the parser validates literal ones.
    pub fn render(&self, vars: &Vars) -> Result<When, String> {
        let mut out = self.clone();
        for (fact, values) in out.0.iter_mut() {
            for v in values.iter_mut() {
                if !is_template(v) {
                    continue;
                }
                let rendered = vars.render(v).map_err(|e| format!("{e} (in `{v}`)"))?;
                fact.check_value(&rendered)
                    .map_err(|e| format!("`{v}` rendered to `{rendered}`: {e}"))?;
                *v = rendered;
            }
        }
        Ok(out)
    }

    /// `Ok(())` when every listed fact has one of its listed values and none of its negated
    /// ones (`"!container"`); otherwise the reason, e.g. "os is macos, not linux" or "env is
    /// container, which `!container` excludes".
    pub fn eval(&self, facts: &Facts) -> Result<(), String> {
        for (fact, wanted) in &self.0 {
            let (negated, plain): (Vec<&String>, Vec<&String>) =
                wanted.iter().partition(|w| w.starts_with('!'));
            if let Some(n) = negated.iter().find(|n| facts.test(*fact, &n[1..])) {
                return Err(format!(
                    "{fact} is {}, which `{n}` excludes",
                    facts.describe(*fact)
                ));
            }
            if !plain.is_empty() && !plain.iter().any(|w| facts.test(*fact, w)) {
                return Err(format!(
                    "{fact} is {}, not {}",
                    facts.describe(*fact),
                    plain
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" or ")
                ));
            }
        }
        Ok(())
    }

    pub fn matches(&self, facts: &Facts) -> bool {
        self.eval(facts).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::facts::{Fact, Facts, Os, Role};

    use super::*;

    fn facts() -> Facts {
        let mut f = Facts::new(Os::Linux, "x86_64", Role::Server);
        f.distro = Some("ubuntu".into());
        f.family = Some("debian".into());
        f.version = Some("24.04".into());
        f.env = vec!["ssh".into()];
        f.pkg = Some("apt".into());
        f
    }

    fn when(pairs: &[(Fact, &[&str])]) -> When {
        When(
            pairs
                .iter()
                .map(|(f, v)| (*f, v.iter().map(|s| s.to_string()).collect()))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    #[test]
    fn empty_matches() {
        assert!(when(&[]).matches(&facts()));
    }

    #[test]
    fn single_and_many_values() {
        assert!(when(&[(Fact::Os, &["linux"])]).matches(&facts()));
        assert!(when(&[(Fact::Os, &["macos", "linux"])]).matches(&facts()));
        assert!(!when(&[(Fact::Os, &["macos"])]).matches(&facts()));
    }

    #[test]
    fn all_keys_must_match() {
        let w = when(&[(Fact::Os, &["linux"]), (Fact::Role, &["desktop"])]);
        assert_eq!(w.eval(&facts()).unwrap_err(), "role is server, not desktop");
        let w = when(&[(Fact::Os, &["linux"]), (Fact::Distro, &["ubuntu"])]);
        assert!(w.matches(&facts()));
        let w = when(&[(Fact::Arch, &["aarch64"])]);
        assert!(!w.matches(&facts()));
    }

    #[test]
    fn negated_values_exclude() {
        // Not in a container (the server profile's CI runs in one, without systemd).
        let w = when(&[(Fact::Env, &["!container"])]);
        assert!(w.matches(&facts()));
        let mut c = facts();
        c.env = vec!["container".into(), "ssh".into()];
        assert_eq!(
            w.eval(&c).unwrap_err(),
            "env is container, ssh, which `!container` excludes"
        );
        // Plain and negated together: one of the plain ones, none of the negated.
        let w = when(&[(Fact::Env, &["ssh", "!wsl"])]);
        assert!(w.matches(&facts()));
        assert!(w.matches(&c), "ssh, and not wsl");
        c.env = vec!["wsl".into(), "ssh".into()];
        assert!(!w.matches(&c));
        let w = when(&[(Fact::Os, &["!windows"]), (Fact::Distro, &["!fedora"])]);
        assert!(w.matches(&facts()));
        // `env: none`: no special environment at all.
        let none = when(&[(Fact::Env, &["none"])]);
        assert!(!none.matches(&facts()), "ssh");
        let mut bare = facts();
        bare.env.clear();
        assert!(none.matches(&bare));
        assert!(when(&[(Fact::Env, &["!none"])]).matches(&facts()));
        // Values are checked with or without `!`.
        assert!(Fact::Env.check_value("!container").is_ok());
        assert!(Fact::Env.check_value("none").is_ok());
        assert!(Fact::Env.check_value("!docker").is_err());
        assert!(Fact::Os.check_value("!plan9").is_err());
    }

    #[test]
    fn templated_values_are_rendered_then_checked() {
        let mut vars = Vars::default();
        vars.set("target_os", "linux");
        vars.set("bad", "plan9");
        let w = when(&[(Fact::Os, &["{{ target_os }}"])]);
        assert!(w.render(&vars).unwrap().matches(&facts()));
        let w = when(&[(Fact::Os, &["{{ bad }}"])]);
        assert!(w.render(&vars).unwrap_err().contains("rendered to `plan9`"));
        let w = when(&[(Fact::Os, &["{{ nope }}"])]);
        assert!(
            w.render(&vars)
                .unwrap_err()
                .contains("undefined variable `nope`")
        );
    }

    #[test]
    fn basic_facts() {
        let w = when(&[(Fact::Family, &["debian"]), (Fact::Pkg, &["apt", "brew"])]);
        assert!(w.matches(&facts()));
        let w = when(&[(Fact::Version, &[">=22.04, <25"])]);
        assert!(w.matches(&facts()));
        let w = when(&[(Fact::Version, &["<24"])]);
        assert_eq!(w.eval(&facts()).unwrap_err(), "version is 24.04, not <24");
        let w = when(&[(Fact::Env, &["wsl", "ssh"])]);
        assert!(w.matches(&facts()));
        let w = when(&[(Fact::Env, &["container"])]);
        assert_eq!(w.eval(&facts()).unwrap_err(), "env is ssh, not container");
        let mut f = facts();
        f.env.clear();
        assert_eq!(w.eval(&f).unwrap_err(), "env is none, not container");
    }

    #[test]
    fn unknown_fact_value_never_matches() {
        let mut f = facts();
        f.distro = None;
        let w = when(&[(Fact::Distro, &["ubuntu"])]);
        assert_eq!(w.eval(&f).unwrap_err(), "distro is unknown, not ubuntu");
    }
}
