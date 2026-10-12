use super::*;
use crate::config::parse_bundle;
use crate::facts::{Os, Role};
use crate::paths::tests::abs;

fn repo(specs: &[(&str, &str)]) -> BTreeMap<String, Bundle> {
    specs
        .iter()
        .map(|(name, yaml)| {
            let file = abs(&format!("/nonexistent-dot-test/bundles/{name}/bundle.yml"));
            let b = parse_bundle(&format!("{yaml}\nsteps: []\n"), &file).unwrap();
            (b.name.clone(), b)
        })
        .collect()
}

fn facts() -> Facts {
    Facts::new(Os::Linux, "x86_64", Role::Server)
}

struct T {
    bundles: BTreeMap<String, Bundle>,
    facts: Facts,
    vars: Vars,
    providers: BTreeMap<String, String>,
}

impl T {
    fn new(specs: &[(&str, &str)]) -> Self {
        let facts = facts();
        let vars = Vars::from_facts(&facts, &abs("/h"));
        Self {
            bundles: repo(specs),
            facts,
            vars,
            providers: BTreeMap::new(),
        }
    }

    fn graph(&self) -> Graph<'_> {
        Graph {
            bundles: &self.bundles,
            facts: &self.facts,
            vars: &self.vars,
            providers: &self.providers,
        }
    }

    fn resolve(&self, roots: &[&str]) -> Result<Resolution, Vec<GraphError>> {
        self.graph().resolve(&Selection {
            roots: roots
                .iter()
                .map(|r| (r.to_string(), Reason::Requested))
                .collect(),
            implicit: true,
            expand: true,
        })
    }

    fn order(&self, roots: &[&str]) -> Vec<String> {
        match self.resolve(roots) {
            Ok(r) => r.order.into_iter().map(|r| r.name).collect(),
            Err(e) => panic!("{e:?}"),
        }
    }

    fn errors(&self, roots: &[&str]) -> Vec<String> {
        match self.resolve(roots) {
            Ok(r) => panic!("expected errors, resolved {:?}", r.names()),
            Err(e) => e.iter().map(ToString::to_string).collect(),
        }
    }
}

#[test]
fn diamond_is_ordered_once_dependencies_first() {
    let t = T::new(&[
        ("app", "requires: [left, right]"),
        ("left", "requires: [base]"),
        ("right", "requires: [base]"),
        ("base", ""),
    ]);
    assert_eq!(t.order(&["app"]), ["base", "left", "right", "app"]);
    let r = t.resolve(&["app"]).unwrap();
    let base = r.get("base").unwrap();
    assert_eq!(
        base.reasons,
        [
            Reason::RequiredBy("left".into()),
            Reason::RequiredBy("right".into())
        ]
    );
    assert_eq!(r.chain("base"), ["app", "left", "base"]);
    let deps = r.dependency_indices();
    assert_eq!(deps[3], [1, 2], "app depends on left and right");
    assert_eq!(deps[1], [0]);
}

#[test]
fn ties_are_alphabetical_and_after_only_orders() {
    let t = T::new(&[
        ("zsh", ""),
        ("alpha", "after: [zsh]"),
        ("mid", ""),
        ("unused", ""),
    ]);
    assert_eq!(t.order(&["mid", "alpha", "zsh"]), ["mid", "zsh", "alpha"]);
    assert_eq!(
        t.order(&["alpha"]),
        ["alpha"],
        "after never pulls a bundle in"
    );
}

#[test]
fn the_schedule_has_every_edge_by_kind() {
    let t = T::new(&[
        ("zsh", ""),
        ("alpha", "after: [zsh]\nrequires: [mid]"),
        ("mid", ""),
    ]);
    let r = t.resolve(&["alpha", "zsh"]).unwrap();
    assert_eq!(r.names(), ["mid", "zsh", "alpha"]);
    let s = r.schedule();
    assert_eq!(s.requires, [vec![], vec![], vec![0]]);
    assert_eq!(s.after, [vec![], vec![], vec![1]]);
    assert_eq!(s.predecessors(2).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(s.ancestors(2), [0, 1, 2].into());
    assert!(
        s.predecessors(1).next().is_none(),
        "zsh and mid may run together"
    );
}

#[test]
fn cycles_name_the_whole_loop() {
    let t = T::new(&[
        ("a", "requires: [b]"),
        ("b", "requires: [c]"),
        ("c", "after: [a]"),
        ("self", "requires: [self]"),
    ]);
    assert_eq!(
        t.errors(&["a", "c"]),
        ["dependency cycle: a -> b -> c -> a"]
    );
    assert_eq!(t.errors(&["self"]), ["dependency cycle: self -> self"]);
}

#[test]
fn missing_bundles_name_the_chain() {
    let t = T::new(&[
        ("dev", "requires: [nodejs]"),
        ("nodejs", "requires: [nope]"),
    ]);
    assert_eq!(
        t.errors(&["dev"]),
        ["unknown bundle `nope` (required via dev -> nodejs)"]
    );
    assert_eq!(t.errors(&["ghost"]), ["unknown bundle `ghost`"]);
}

#[test]
fn capabilities_with_zero_one_or_two_providers() {
    let mut t = T::new(&[
        ("app", "requires: [cap:node, cap:py]"),
        ("fnm", "provides: [node]"),
        ("nvm", "provides: [node]"),
        ("pyenv", "provides: [py]"),
    ]);
    let errs = t.errors(&["app"]);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0].contains("capability `node` (required via app) has several providers (fnm, nvm)"),
        "{errs:?}"
    );
    t.providers.insert("node".into(), "nvm".into());
    assert_eq!(t.order(&["app"]), ["nvm", "pyenv", "app"]);
    let r = t.resolve(&["app"]).unwrap();
    assert_eq!(
        r.get("nvm").unwrap().reasons,
        [Reason::Provides {
            cap: "node".into(),
            by: "app".into()
        }]
    );

    t.providers.insert("node".into(), "pyenv".into());
    assert!(t.errors(&["app"])[0].contains("`pyenv` does not provide `node`"));

    let t = T::new(&[("app", "requires: [cap:ruby]")]);
    assert_eq!(
        t.errors(&["app"]),
        ["capability `ruby` (required via app): no bundle provides it"]
    );
}

#[test]
fn a_single_matching_provider_is_chosen() {
    let t = T::new(&[
        ("app", "requires: [cap:node]"),
        ("brew-node", "provides: [node]\nwhen: { os: macos }"),
        ("apt-node", "provides: [node]\nwhen: { os: linux }"),
    ]);
    assert_eq!(t.order(&["app"]), ["apt-node", "app"]);
    let t = T::new(&[
        ("app", "requires: [cap:node]"),
        ("brew-node", "provides: [node]\nwhen: { os: macos }"),
    ]);
    let e = t.errors(&["app"]);
    assert!(
        e[0].contains("no provider matches this machine (brew-node: when: os is linux, not macos)"),
        "{e:?}"
    );
}

#[test]
fn conditional_exclusion() {
    let t = T::new(&[
        ("gui", "when: { role: desktop }"),
        ("dev", "requires: [nodejs]"),
        ("nodejs", "requires: [homebrew]"),
        ("homebrew", "when: { os: macos }"),
    ]);
    // A selected bundle that does not match is left out and reported.
    let r = t
        .graph()
        .resolve(&Selection {
            roots: vec![("gui".into(), Reason::Selected("profile p".into()))],
            implicit: true,
            expand: true,
        })
        .unwrap();
    assert!(r.order.is_empty());
    assert_eq!(r.excluded[0].name, "gui");
    assert_eq!(r.excluded[0].detail, "when: role is server, not desktop");
    // One named on the command line is an error: applying nothing silently would hide it.
    assert_eq!(
        t.errors(&["gui"]),
        ["bundle `gui` does not match this machine: when: role is server, not desktop"]
    );
    // A required one is an error naming the chain.
    assert_eq!(
        t.errors(&["dev"]),
        [
            "bundle `homebrew` (required via dev -> nodejs) does not match this machine: when: os is linux, not macos"
        ]
    );
}

#[test]
fn requires_if_follows_facts() {
    let t = T::new(&[
        (
            "nodejs",
            "requires_if:\n  - when: { os: macos }\n    requires: [homebrew]\n  - when: { os: linux }\n    requires: [apt-extra]",
        ),
        ("homebrew", "when: { os: macos }"),
        ("apt-extra", ""),
    ]);
    assert_eq!(t.order(&["nodejs"]), ["apt-extra", "nodejs"]);
}

#[test]
fn implicit_bundles_are_always_included_when_they_match() {
    let t = T::new(&[
        ("common", "implicit: true"),
        ("mac-common", "implicit: true\nwhen: { os: macos }"),
        ("zsh", ""),
    ]);
    let r = t.resolve(&["zsh"]).unwrap();
    assert_eq!(r.names(), ["common", "zsh"]);
    assert_eq!(r.get("common").unwrap().reasons, [Reason::Implicit]);
}

#[test]
fn no_deps_keeps_only_the_roots_but_orders_them() {
    let t = T::new(&[
        ("common", "implicit: true"),
        ("app", "requires: [lib, cap:node]"),
        ("lib", ""),
    ]);
    let sel = |roots: &[&str]| Selection {
        roots: roots
            .iter()
            .map(|r| (r.to_string(), Reason::Requested))
            .collect(),
        implicit: false,
        expand: false,
    };
    let r = t.graph().resolve(&sel(&["app"])).unwrap();
    assert_eq!(r.names(), ["app"]);
    let r = t.graph().resolve(&sel(&["app", "lib"])).unwrap();
    assert_eq!(r.names(), ["lib", "app"]);
}

#[test]
fn templated_when_uses_bundle_vars() {
    let t = T::new(&[
        (
            "x",
            "vars: { want: server }\nwhen: { role: \"{{ want }}\" }",
        ),
        ("bad", "when: { role: \"{{ nope }}\" }"),
    ]);
    assert_eq!(t.order(&["x"]), ["x"]);
    assert!(t.errors(&["bad"])[0].contains("undefined variable `nope`"));
}

#[test]
fn static_check_finds_everything() {
    let bundles = repo(&[
        ("a", "requires: [b, ghost, cap:none]\nafter: [typo]"),
        (
            "b",
            "requires_if:\n  - when: { os: macos }\n    requires: [a]",
        ),
    ]);
    let problems = check_all(&bundles);
    let errs: Vec<String> = problems.iter().map(|p| p.error.to_string()).collect();
    assert!(
        errs.contains(&"unknown bundle `ghost` (required via a)".to_string()),
        "{errs:?}"
    );
    // An `after:` ref is ordering only, and says so.
    assert!(
        errs.contains(&"after: unknown bundle `typo` (in a)".to_string()),
        "{errs:?}"
    );
    let key_of = |msg: &str| {
        problems
            .iter()
            .find(|p| p.error.to_string().contains(msg))
            .and_then(|p| p.key)
    };
    assert_eq!(key_of("`typo`"), Some("after"));
    assert_eq!(key_of("`ghost`"), Some("requires"));
    assert!(
        errs.iter().any(|e| e.contains("capability `none`")),
        "{errs:?}"
    );
    assert!(
        errs.contains(&"dependency cycle: a -> b -> a".to_string()),
        "{errs:?}"
    );
}

#[test]
fn after_a_capability_orders_after_its_provider() {
    let t = T::new(&[
        ("aaa", "after: [\"cap:editor\"]"),
        ("zed", "provides: [editor]"),
    ]);
    assert_eq!(t.order(&["aaa", "zed"]), ["zed", "aaa"]);
    let r = t.resolve(&["aaa", "zed"]).unwrap();
    assert!(
        r.edges
            .iter()
            .any(|e| e.from == "aaa" && e.to == "zed" && e.kind == EdgeKind::After),
        "{:?}",
        r.edges
    );
    // Ordering only: the provider is not pulled in.
    assert_eq!(t.order(&["aaa"]), ["aaa"]);
}
