use std::fs;

use super::*;
use crate::facts::{Os, Role};
use crate::paths::tests::abs;

fn parse(yaml: &str) -> Result<RepoConfig, ConfigError> {
    parse_repo(yaml, &abs("/nonexistent-dot-test/dot.yml"))
}

const FULL: &str = r#"
bundles_dir: mods
bundles: [common, zsh]
vars:
  editor: nvim
  email: { required: true, pattern: ".+@.+" }
defaults:
  link: { on_conflict: overwrite }
  shell: { interactive: true }
packages:
  fd: { apt: fd-find, brew: fd }
secrets_provider: envfile
providers: { node: fnm }
profiles:
  mac:
    match: { os: macos }
    bundles: [common, gui]
    vars: { editor: code }
    secrets_provider: lastpass
    providers: { node: nvm }
  ubuntu-server:
    match: { os: linux, role: server }
    defaults: { link: { create: false } }
  anywhere:
    match: {}
  manual:
    description: only when pinned
"#;

#[test]
fn parses_a_full_dot_yml() {
    let c = parse(FULL).unwrap();
    assert_eq!(c.bundles_dir, abs("/nonexistent-dot-test/mods"));
    assert_eq!(
        c.bundles.as_deref(),
        Some(&["common".to_string(), "zsh".to_string()][..])
    );
    assert_eq!(c.vars.values.len(), 1);
    assert_eq!(c.vars.decls.len(), 1);
    assert_eq!(c.defaults.shell.interactive, Some(true));
    assert_eq!(c.packages["fd"]["apt"], "fd-find");
    assert_eq!(c.secrets_provider.as_deref(), Some("envfile"));
    let names: Vec<&str> = c.profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["mac", "ubuntu-server", "anywhere", "manual"]);
    let mac = c.profile("mac").unwrap();
    assert_eq!(mac.secrets_provider.as_deref(), Some("lastpass"));
    assert_eq!(mac.providers["node"], "nvm");
    assert_eq!(mac.vars.source.name, "profile mac");
    assert_eq!(mac.line, Some(15));
    assert_eq!(
        c.profile("ubuntu-server").unwrap().defaults.link.create,
        Some(false)
    );
}

#[test]
fn empty_file_is_an_empty_config() {
    let c = parse("# nothing yet\n").unwrap();
    assert!(c.profiles.is_empty());
    assert_eq!(c.bundles_dir, abs("/nonexistent-dot-test/bundles"));
}

#[test]
fn strict_errors_have_lines() {
    let e = parse("bundels: [a]\n").unwrap_err();
    assert_eq!(e.line, Some(1));
    assert!(e.message.contains("unknown field `bundels`"), "{e}");
    let e = parse("profiles:\n  mac:\n    match: { os: macos }\n    bundle: [x]\n").unwrap_err();
    assert_eq!(e.line, Some(4), "{e}");
    let e = parse("packages:\n  fd: { yum: fd }\n").unwrap_err();
    assert!(e.message.contains("unknown variant `yum`"), "{e}");
    let e = parse("profiles:\n  mac:\n    match: { os: macOS }\n").unwrap_err();
    assert!(e.message.contains("unknown value `macOS`"), "{e}");
}

#[test]
fn every_bad_profile_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(REPO_FILE),
        "profiles:\n  a:\n    bundle: [x]\n  b:\n    match: { os: linux }\n  c:\n    matches: {}\n",
    )
    .unwrap();
    let errs = load_repo_config(dir.path()).unwrap_err();
    let text: Vec<String> = errs.iter().map(ToString::to_string).collect();
    assert_eq!(errs.len(), 2, "{text:?}");
    assert!(
        text[0].contains(":3:") && text[0].contains("unknown field `bundle`"),
        "{text:?}"
    );
    assert!(
        text[1].contains(":6:") && text[1].contains("profiles.c: unknown field `matches`"),
        "{text:?}"
    );
}

fn repo_with(yaml: &str) -> Repo {
    Repo {
        config: parse(yaml).unwrap(),
        bundles: BTreeMap::new(),
    }
}

fn select(
    repo: &Repo,
    facts: &Facts,
    requested: Option<&str>,
    pinned: Option<&str>,
) -> Result<Option<String>, String> {
    let vars = Vars::from_facts(facts, &abs("/h"));
    repo.select_profile(facts, &vars, requested, pinned.map(|p| (p, "local.env")))
        .map(|c| c.map(|c| c.profile.name.clone()))
}

#[test]
fn profile_selection() {
    let repo = repo_with(FULL);
    let mac = Facts::new(Os::Macos, "aarch64", Role::Desktop);
    let server = Facts::new(Os::Linux, "x86_64", Role::Server);
    let desktop = Facts::new(Os::Linux, "x86_64", Role::Desktop);
    // The most specific match wins over the catch-all.
    assert_eq!(
        select(&repo, &mac, None, None).unwrap().as_deref(),
        Some("mac")
    );
    assert_eq!(
        select(&repo, &server, None, None).unwrap().as_deref(),
        Some("ubuntu-server")
    );
    assert_eq!(
        select(&repo, &desktop, None, None).unwrap().as_deref(),
        Some("anywhere")
    );
    // --profile beats the pin, which beats matching; neither needs to match.
    assert_eq!(
        select(&repo, &mac, Some("manual"), Some("anywhere"))
            .unwrap()
            .as_deref(),
        Some("manual")
    );
    assert_eq!(
        select(&repo, &mac, None, Some("ubuntu-server"))
            .unwrap()
            .as_deref(),
        Some("ubuntu-server")
    );
    let e = select(&repo, &mac, Some("nope"), None).unwrap_err();
    assert!(
        e.contains("unknown profile `nope` (profiles: mac, ubuntu-server, anywhere, manual)"),
        "{e}"
    );
    // No profiles at all: no profile.
    assert_eq!(select(&repo_with(""), &mac, None, None).unwrap(), None);
}

#[test]
fn ambiguity_and_no_match_are_errors() {
    let repo = repo_with(
        "profiles:\n  a: { match: { os: linux } }\n  b: { match: { role: server } }\n  c: { match: { os: macos } }\n",
    );
    let server = Facts::new(Os::Linux, "x86_64", Role::Server);
    let e = select(&repo, &server, None, None).unwrap_err();
    assert!(
        e.contains("several profiles match this machine equally well (a, b)"),
        "{e}"
    );
    let win = Facts::new(Os::Windows, "x86_64", Role::Desktop);
    let e = select(&repo, &win, None, None).unwrap_err();
    assert!(e.contains("no profile matches this machine (a: os is windows, not linux; b: role is desktop, not server; c: os is windows, not macos)"), "{e}");
}

#[test]
fn find_repo_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for d in ["flag", "env", "walk/sub/deeper", "local", "none"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    for d in ["flag", "env", "walk", "local"] {
        fs::write(root.join(d).join(REPO_FILE), "").unwrap();
    }
    let cwd = root.join("walk/sub/deeper");
    let home = root.join("none");
    let found = |flag: Option<&str>, env: Option<&str>, cwd: &Path, local: Option<&str>| {
        find_repo(flag.map(Path::new), env, cwd, local, &home)
    };
    let flag = root.join("flag");
    let env = root.join("env");
    let local = root.join("local");
    let r = found(
        Some(flag.to_str().unwrap()),
        Some(env.to_str().unwrap()),
        &cwd,
        None,
    )
    .unwrap();
    // A repository named by the flag, DOT_REPO or local.env comes out in the ordinary spelling:
    // on Windows `root` is verbatim (`\\?\C:\...`, from `canonicalize`), which users never see.
    let plain = crate::paths::plain;
    assert_eq!((r.root, r.how.as_str()), (plain(flag.clone()), "--repo"));
    let r = found(None, Some(env.to_str().unwrap()), &cwd, None).unwrap();
    assert_eq!(r.root, plain(env));
    let r = found(None, None, &cwd, Some(local.to_str().unwrap())).unwrap();
    assert_eq!(r.root, root.join("walk"));
    let r = found(
        None,
        None,
        &root.join("none"),
        Some(local.to_str().unwrap()),
    )
    .unwrap();
    assert_eq!(
        (r.root, r.how.as_str()),
        (plain(local), "DOT_REPO in local.env")
    );
    assert!(
        found(None, None, &root.join("none"), None)
            .unwrap_err()
            .contains("no dotfiles repository found")
    );
    let e = found(Some(root.join("none").to_str().unwrap()), None, &cwd, None).unwrap_err();
    assert!(e.contains("has no dot.yml"), "{e}");

    // ~/.dotfiles is another setup's (dotbot's) checkout: never looked at, even with a dot.yml.
    let other = home.join(".dotfiles");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("dot.yml"), "").unwrap();
    let e = found(None, None, &root.join("none"), None).unwrap_err();
    assert!(e.contains("no dotfiles repository found"), "{e}");
    assert!(e.contains("~/.dot;"), "{e}");

    // Last: ~/.dot, where the installers clone a repository.
    let dotfiles = home.join(".dot");
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::write(dotfiles.join("dot.yml"), "").unwrap();
    let r = found(None, None, &root.join("none"), None).unwrap();
    assert_eq!(
        (r.root, r.how.as_str()),
        (crate::paths::plain(dotfiles), "~/.dot")
    );
    // Everything else still comes first.
    let r = found(None, None, &cwd, None).unwrap();
    assert_eq!(r.root, root.join("walk"));
}

/// `text` with `/` as this platform's separator: errors name files by their path, which is
/// `bundles\a\bundle.yml` on Windows.
fn native(text: &str) -> String {
    text.replace('/', std::path::MAIN_SEPARATOR_STR)
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

#[test]
fn load_and_check_report_everything() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        REPO_FILE,
        "bundles: [a, ghost]\nproviders: { node: b }\nprofiles:\n  p:\n    match: {}\n    bundles: [phantom]\n    providers: { py: nobody }\n",
    );
    write(
        root,
        "bundles/a/bundle.yml",
        "requires: [b, missing]\nsteps: []\n",
    );
    write(root, "bundles/b/bundle.yml", "requires: [a]\nsteps: []\n");
    let repo = Repo::load(root).unwrap();
    let errs: Vec<String> = repo.check().iter().map(ToString::to_string).collect();
    let has = |s: &str| errs.iter().any(|e| e.contains(s));
    assert!(has("bundles: unknown bundle `ghost`"), "{errs:#?}");
    assert!(has("bundle `b` does not provide `node`"), "{errs:#?}");
    assert!(
        has("profiles.p: bundles: unknown bundle `phantom`"),
        "{errs:#?}"
    );
    assert!(
        has("profiles.p: providers: `py`: unknown bundle `nobody`"),
        "{errs:#?}"
    );
    assert!(
        has("unknown bundle `missing` (required via a)"),
        "{errs:#?}"
    );
    assert!(has("dependency cycle: a -> b -> a"), "{errs:#?}");
    assert!(
        errs.iter()
            .any(|e| e.contains(&native("bundles/a/bundle.yml:1:"))),
        "{errs:#?}"
    );

    // Parse errors in several files are all reported, several per file where possible.
    write(
        root,
        "bundles/c/bundle.yml",
        "steps:\n  - shell: x\n    lable: y\n  - create: [~/ok]\n  - linkk: {}\n",
    );
    write(root, "bundles/d/bundle.yml", "name: a\nsteps: []\n");
    let errs: Vec<String> = Repo::load(root)
        .unwrap_err()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(errs.len(), 3, "{errs:#?}");
    assert!(
        errs[0].contains(&native("c/bundle.yml:3:5: steps[0]: unknown field `lable`")),
        "{errs:#?}"
    );
    assert!(
        errs[1].contains(&native("c/bundle.yml:5:5: steps[2]: unknown field `linkk`")),
        "{errs:#?}"
    );
    assert!(
        errs[2].contains("bundle name `a` is already used"),
        "{errs:#?}"
    );
}

#[test]
fn check_points_at_the_key_holding_the_bad_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, REPO_FILE, "");
    write(
        root,
        "bundles/a/bundle.yml",
        "requires: [b]\nafter: [nosuch]\nsteps: []\n",
    );
    write(root, "bundles/b/bundle.yml", "steps: []\n");
    let repo = Repo::load(root).unwrap();
    let errs: Vec<String> = repo.check().iter().map(ToString::to_string).collect();
    assert_eq!(errs.len(), 1, "{errs:#?}");
    assert!(
        errs[0].ends_with(&native(
            "a/bundle.yml:2: after: unknown bundle `nosuch` (in a)"
        )),
        "{errs:#?}"
    );
}
