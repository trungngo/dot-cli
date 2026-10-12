use std::collections::BTreeMap;
use std::sync::Arc;

use dot_core::event::Recorder;
use dot_core::facts::Role;
use dot_core::{Event, EventSink, Options, SecretRefs, engine};
use dot_platform::exec::fake::{FakeRunner, Mode};
use dot_platform::pkg::CaskFetcher;

use super::*;
use crate::Builtins;
use crate::testutil::Fixture;

/// Cask metadata without the network: `app-*` casks install an app, `pkg-*` casks run a
/// `.pkg` installer, anything else is "offline".
struct FakeApi;
impl CaskFetcher for FakeApi {
    fn fetch(&self, token: &str) -> Result<String> {
        if token.starts_with("app-") {
            Ok(r#"{"artifacts":[{"app":["X.app"]}]}"#.into())
        } else if token.starts_with("pkg-") {
            Ok(r#"{"artifacts":[{"pkg":["X.pkg"]}]}"#.into())
        } else {
            anyhow::bail!("could not resolve host formulae.brew.sh")
        }
    }
}

struct Machine {
    runner: Arc<FakeRunner>,
    packages: Packages,
    _state: tempfile::TempDir,
}

fn machine(programs: &[&str]) -> Machine {
    let runner = Arc::new(FakeRunner::with_programs(programs));
    let state = tempfile::tempdir().unwrap();
    let packages = Packages::new(
        runner.clone(),
        Some(state.path().to_path_buf()),
        Arc::new(FakeApi),
    );
    Machine {
        runner,
        packages,
        _state: state,
    }
}

fn ubuntu() -> Facts {
    let mut f = Facts::new(Os::Linux, "x86_64", Role::Desktop);
    f.distro = Some("ubuntu".into());
    f.family = Some("debian".into());
    f.pkg = Some("apt".into());
    f
}

fn mac() -> Facts {
    let mut f = Facts::new(Os::Macos, "aarch64", Role::Desktop);
    f.pkg = Some("brew".into());
    f
}

fn name_map() -> PackageMap {
    let mut m = PackageMap::new();
    m.insert(
        "fd".into(),
        BTreeMap::from([
            ("apt".to_string(), "fd-find".to_string()),
            ("brew".to_string(), "fd".to_string()),
        ]),
    );
    m
}

fn context(f: &Fixture, facts: Facts) -> dot_core::Context {
    let mut ctx = dot_core::Context::new(facts, f.home().to_path_buf());
    ctx.packages = name_map();
    ctx
}

fn plan_step(
    f: &Fixture,
    ctx: &dot_core::Context,
    m: &Machine,
    step: usize,
) -> Result<Vec<Box<dyn Action>>> {
    let secrets = SecretRefs::default();
    let pctx = PlanCtx::new(ctx, &f.bundle, &f.bundle.steps[step], &secrets);
    let dot_core::Directive::Package(spec) = &f.bundle.steps[step].directive else {
        panic!("not a package step")
    };
    plan(spec, &pctx, &m.packages, false)
}

fn apply(f: &Fixture, a: &dyn Action) -> Outcome {
    f.apply_one(a).unwrap_or_else(|e| panic!("{e:#}"))
}

const DPKG: &str = "dpkg-query -W -f ${Package}\t${db:Status-Abbrev}\n";
/// Rule prefixes split on whitespace, and the format argument holds a tab.
const DPKG_RULE: &str = "dpkg-query -W";

#[test]
fn short_form_maps_names_and_names_exactly_what_is_missing() {
    let f = Fixture::new("steps:\n  - package: [fd, ripgrep, git]\n");
    let m = machine(&["apt-get", "dpkg-query", "sudo"]);
    m.runner.on(DPKG_RULE, 1, "git\tii \nripgrep\tun \n");
    let ctx = context(&f, ubuntu());
    let actions = plan_step(&f, &ctx, &m, 0).unwrap();
    assert_eq!(actions.len(), 1);
    assert!(m.runner.calls().is_empty(), "planning itself runs nothing");
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert_eq!(
        m.runner.lines(),
        [format!("{DPKG} fd-find ripgrep git")],
        "one batched query, with the mapped name"
    );
    assert_eq!(
        actions[0].describe(),
        "apt: install fd-find, ripgrep (already installed: git) [sudo]"
    );
    assert!(actions[0].needs_sudo());

    m.runner.clear_calls();
    assert_eq!(apply(&f, actions[0].as_ref()), Outcome::Changed);
    let streamed = m.runner.lines_of(Mode::Stream);
    assert_eq!(streamed.len(), 2, "{streamed:?}");
    assert!(streamed[0].ends_with("apt-get update -q"), "{streamed:?}");
    assert!(
        streamed[1].starts_with("sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y"),
        "{streamed:?}"
    );
    assert!(
        streamed[1].ends_with(" fd-find ripgrep"),
        "only the missing ones"
    );

    // Everything installed: satisfied, nothing to do, no sudo needed.
    m.runner
        .on(DPKG_RULE, 0, "fd-find\tii \nripgrep\tii \ngit\tii \n");
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert_eq!(
        actions[0].describe(),
        "apt: fd-find, ripgrep, git installed"
    );
    let as_root = machine(&["apt-get", "dpkg-query"]);
    as_root.runner.set_root(true);
    let a = plan_step(&f, &ctx, &as_root, 0).unwrap();
    assert!(!a[0].needs_sudo(), "root needs no sudo");
}

#[test]
fn short_form_without_a_known_manager_is_a_located_error() {
    let f = Fixture::new("steps:\n  - package: [fd]\n");
    let m = machine(&[]);
    let mut facts = ubuntu();
    facts.family = None;
    facts.pkg = None;
    let e = format!(
        "{:#}",
        plan_step(&f, &context(&f, facts), &m, 0).err().unwrap()
    );
    assert!(
        e.contains("bundle.yml:2:5: steps[0]: package: no package manager"),
        "{e}"
    );
    // The family says which manager to expect even before it is on PATH.
    let mut facts = ubuntu();
    facts.pkg = None;
    let a = plan_step(&f, &context(&f, facts), &m, 0).unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Unknown);
    assert_eq!(
        a[0].describe(),
        "apt: install fd-find (apt is not installed yet) [sudo]"
    );
}

#[test]
fn step_form_uses_only_this_machines_managers() {
    let f = Fixture::new(
        "steps:\n  - package:\n      brew: [fnm]\n      cask: [app-wezterm]\n      apt: [fd-find]\n      winget: [Git.Git]\n",
    );
    let m = machine(&["brew"]);
    let on_mac = plan_step(&f, &context(&f, mac()), &m, 0).unwrap();
    let kinds: Vec<String> = on_mac.iter().map(|a| a.describe()).collect();
    assert_eq!(
        kinds,
        ["brew: install fnm", "brew cask: install app-wezterm"],
        "before status, the plan knows nothing more"
    );
    let on_ubuntu = plan_step(&f, &context(&f, ubuntu()), &m, 0).unwrap();
    assert_eq!(on_ubuntu.len(), 1);
    assert!(on_ubuntu[0].describe().starts_with("apt: install fd-find"));
    let mut win = Facts::new(Os::Windows, "x86_64", Role::Desktop);
    win.pkg = Some("winget".into());
    let on_windows = plan_step(&f, &context(&f, win), &m, 0).unwrap();
    assert_eq!(on_windows.len(), 1);
    assert!(
        !on_windows[0].needs_sudo(),
        "per-user winget installs need no elevation"
    );
}

#[test]
fn machine_wide_winget_installs_need_an_elevated_terminal() {
    let f = Fixture::new(
        "steps:\n  - package:\n      winget: [Git.Git, { name: Microsoft.PowerToys, scope: machine }]\n",
    );
    let m = machine(&["winget"]);
    let mut win = Facts::new(Os::Windows, "x86_64", Role::Desktop);
    win.pkg = Some("winget".into());
    m.runner.on("winget list", 1, "");
    let a = &plan_step(&f, &context(&f, win.clone()), &m, 0).unwrap()[0];
    assert_eq!(a.status().unwrap(), Status::Pending);
    assert!(a.needs_sudo(), "the run checks for elevation up front");
    assert!(
        a.describe()
            .ends_with("[machine-wide: needs an elevated terminal]"),
        "{}",
        a.describe()
    );
    m.runner.clear_calls();
    assert_eq!(apply(&f, a.as_ref()), Outcome::Changed);
    let installs = m.runner.lines_of(Mode::Stream);
    assert!(installs[0].starts_with("winget install --exact --id Git.Git --silent --accept"));
    assert!(
        installs[1].starts_with(
            "winget install --exact --id Microsoft.PowerToys --silent --scope machine"
        ),
        "{installs:?}"
    );
    // Only the machine-wide one installed already: nothing needs elevation.
    m.runner.on(
        "winget list --exact --id Microsoft.PowerToys",
        0,
        "Microsoft.PowerToys\n",
    );
    let a = &plan_step(&f, &context(&f, win.clone()), &m, 0).unwrap()[0];
    assert_eq!(a.status().unwrap(), Status::Pending);
    assert!(!a.needs_sudo());
    // Elevated already: nothing to ask for either.
    m.runner.on("winget list", 1, "");
    m.runner.set_root(true);
    let a = &plan_step(&f, &context(&f, win), &m, 0).unwrap()[0];
    assert_eq!(a.status().unwrap(), Status::Pending);
    assert!(!a.needs_sudo());
}

#[test]
fn machine_wide_winget_upgrades_need_an_elevated_terminal_too() {
    let f = Fixture::new(
        "steps:\n  - package: { winget: [Git.Git, { name: Microsoft.PowerToys, scope: machine }], upgrade: true }\n",
    );
    let m = machine(&["winget"]);
    let mut win = Facts::new(Os::Windows, "x86_64", Role::Desktop);
    win.pkg = Some("winget".into());
    m.runner
        .on("winget list --exact --id Git.Git", 0, "Git.Git\n")
        .on(
            "winget list --exact --id Microsoft.PowerToys",
            0,
            "Microsoft.PowerToys\n",
        )
        .on(
            "winget list --exact --id Git.Git --upgrade-available",
            0,
            "Name Id Version Available Source\n-------------------------------\nGit Git.Git 2.45.0 2.46.0 winget\n",
        )
        .on(
            "winget list --exact --id Microsoft.PowerToys --upgrade-available",
            1,
            "",
        );
    // Only the per-user package is outdated: no elevation.
    let a = &plan_step(&f, &context(&f, win.clone()), &m, 0).unwrap()[0];
    assert_eq!(a.status().unwrap(), Status::Pending);
    assert!(!a.needs_sudo(), "{}", a.describe());
    // The machine-wide one is outdated too: its upgrade needs an elevated terminal.
    m.runner.on(
        "winget list --exact --id Microsoft.PowerToys --upgrade-available",
        0,
        "Name Id Version Available Source\n-------------------------------\nPowerToys Microsoft.PowerToys 0.80.0 0.81.0 winget\n",
    );
    let a = &plan_step(&f, &context(&f, win), &m, 0).unwrap()[0];
    assert_eq!(a.status().unwrap(), Status::Pending);
    assert!(a.needs_sudo());
    let d = a.describe();
    assert!(
        d.contains("upgrade Git.Git 2.45.0 -> 2.46.0, Microsoft.PowerToys 0.80.0 -> 0.81.0"),
        "{d}"
    );
    assert!(
        d.ends_with("[machine-wide: needs an elevated terminal]"),
        "{d}"
    );
}

#[test]
fn casks_get_adopt_only_when_they_install_an_app() {
    let f = Fixture::new(
        "steps:\n  - package:\n      cask: [app-wezterm, pkg-karabiner, font-fira, app-installed]\n",
    );
    let m = machine(&["brew"]);
    m.runner.on("brew list --cask", 1, "app-installed 1.0\n");
    let a = &plan_step(&f, &context(&f, mac()), &m, 0).unwrap()[0];
    assert_eq!(a.status().unwrap(), Status::Pending);
    let d = a.describe();
    assert!(
        d.starts_with(
            "brew cask: install app-wezterm, pkg-karabiner, font-fira (already installed: \
             app-installed)"
        ),
        "{d}"
    );
    assert!(d.contains("--adopt for app-wezterm"), "{d}");
    assert!(
        d.contains("cannot tell whether cask font-fira installs an app"),
        "{d}"
    );
    assert!(a.needs_sudo(), "a .pkg installer asks for sudo");
    assert!(a.asks_for_sudo(), "Homebrew's own sudo asks");
    assert!(
        !a.needs_tty(),
        "nobody to ask: passwordless sudo, checked up front"
    );
    assert!(
        d.contains("pkg-karabiner: Homebrew asks for the sudo password in the terminal"),
        "{d}"
    );

    m.runner.clear_calls();
    assert_eq!(apply(&f, a.as_ref()), Outcome::Changed);
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        [
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 SUDO_ASKPASS=/usr/bin/false \
             HOMEBREW_NO_SUDO=1 brew install --cask --adopt app-wezterm",
            // Its sudo free (passwordless: checked up front); the others' never asks.
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 brew install --cask pkg-karabiner",
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 SUDO_ASKPASS=/usr/bin/false \
             HOMEBREW_NO_SUDO=1 brew install --cask font-fira",
        ]
    );
    let warnings: Vec<String> = f
        .recorder
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Output { line, .. } if line.starts_with("warning:") => Some(line),
            _ => None,
        })
        .collect();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("font-fira") && w.contains("without --adopt")),
        "{warnings:?}"
    );
}

/// Interactive: a cask whose `.pkg` installer asks for the sudo password is installed (and
/// upgraded) with the terminal, on its own, the view paused around it; its output is still
/// streamed (sudo opens the terminal itself), so `--json` and the log keep it.
#[test]
fn pkg_casks_get_the_terminal_when_someone_is_there() {
    let f = Fixture::new(
        "steps:\n  - package: { cask: [app-wezterm, pkg-karabiner, pkg-old], upgrade: true }\n",
    );
    let m = machine(&["brew"]);
    let packages = m
        .packages
        .clone()
        .with_sudo(dot_platform::Sudo::new(m.runner.clone(), true));
    m.runner.on("brew list --cask", 1, "pkg-old 1.0\n");
    m.runner
        .on("brew list --cask --versions pkg-old", 0, "pkg-old 1.0\n");
    m.runner.on(
        "brew outdated --cask --json=v2",
        1,
        r#"{"formulae":[],"casks":[{"name":"pkg-old","installed_versions":"1.0","current_version":"2.0"}]}"#,
    );
    let ctx = context(&f, mac());
    let secrets = SecretRefs::default();
    let pctx = PlanCtx::new(&ctx, &f.bundle, &f.bundle.steps[0], &secrets);
    let dot_core::Directive::Package(spec) = &f.bundle.steps[0].directive else {
        panic!()
    };
    let a = plan(spec, &pctx, &packages, false).unwrap().remove(0);
    assert_eq!(a.status().unwrap(), Status::Pending);
    assert!(a.asks_for_sudo());
    assert!(a.needs_tty(), "the step runs alone, with the terminal");
    let d = a.describe();
    assert!(
        d.contains("pkg-karabiner, pkg-old: Homebrew asks for the sudo password in the terminal"),
        "{d}"
    );

    m.runner.clear_calls();
    m.runner.on(
        "brew install --cask pkg-karabiner",
        0,
        "==> Running installer for pkg-karabiner with sudo\n",
    );
    assert_eq!(apply(&f, a.as_ref()), Outcome::Changed);
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        [
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 SUDO_ASKPASS=/usr/bin/false \
             HOMEBREW_NO_SUDO=1 brew install --cask --adopt app-wezterm",
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 brew install --cask pkg-karabiner",
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 brew upgrade --cask pkg-old",
        ]
    );
    assert!(m.runner.lines_of(Mode::Interactive).is_empty());
    let mut tty: Vec<String> = f
        .recorder
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::NeedsTty {
                step: Some(_),
                reason,
            } => Some(reason),
            Event::TtyReleased { step: Some(_) } => Some("released".into()),
            Event::Output { line, .. } if line.contains("Running installer") => {
                Some(format!("output: {line}"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(tty.len(), 5, "{tty:?}");
    assert_eq!(
        tty[1], "output: ==> Running installer for pkg-karabiner with sudo",
        "{tty:?}"
    );
    tty.remove(1);
    assert!(
        tty[0].contains("Homebrew asks for your sudo password to install pkg-karabiner"),
        "{tty:?}"
    );
    assert!(
        tty[0].contains("discards cached sudo credentials"),
        "{tty:?}"
    );
    assert_eq!(tty[1], "released");
    assert!(tty[2].contains("to upgrade pkg-old"), "{tty:?}");
    assert_eq!(tty[3], "released");

    // A failed install on the terminal still gives the terminal back, and fails the step.
    let a = plan(spec, &pctx, &packages, false).unwrap().remove(0);
    a.status().unwrap();
    m.runner.on("brew install --cask pkg-karabiner", 1, "");
    let e = f.apply_one(a.as_ref()).unwrap_err();
    assert!(format!("{e:#}").contains("exited with code 1"), "{e:#}");
    let events = f.recorder.events();
    assert!(
        matches!(events.last(), Some(Event::TtyReleased { .. })),
        "{events:?}"
    );
}

#[test]
fn snap_falls_back_to_apt_without_snapd() {
    let f = Fixture::new(
        "steps:\n  - package:\n      snap: [fd, hello, { name: code, classic: true }]\n",
    );
    let m = machine(&["apt-get", "dpkg-query", "sudo", "snap"]);
    let a = plan_step(&f, &context(&f, ubuntu()), &m, 0).unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(
        a[0].describe(),
        "apt: install fd-find, hello, code [snapd is not available: installing with apt; sudo]"
    );

    // With snapd running: snaps, the classic one on its own.
    m.runner.add_file("/run/snapd.socket");
    m.runner.on("snap list", 0, "Name Version\nhello 2.10\n");
    let a = plan_step(&f, &context(&f, ubuntu()), &m, 0).unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    assert_eq!(
        a[0].describe(),
        "snap: install fd, code (already installed: hello) [sudo]"
    );
    apply(&f, a[0].as_ref());
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        [
            "sudo -n snap install fd",
            "sudo -n snap install --classic code"
        ]
    );
}

#[test]
fn a_snap_only_package_waits_for_snapd_instead_of_falling_back() {
    let f = Fixture::new(
        "steps:\n  - package:\n      snap: [hello, { name: microk8s, classic: true, fallback: false }]\n",
    );
    let m = machine(&["apt-get", "dpkg-query", "sudo", "snap"]);
    let a = plan_step(&f, &context(&f, ubuntu()), &m, 0).unwrap();
    assert_eq!(a.len(), 2);
    assert!(a[0].describe().starts_with("apt: install hello"));
    // Never `apt-get install microk8s`: unknown until snapd is there (an earlier step may
    // install it), and a clear failure if it is still not there.
    assert_eq!(a[1].status().unwrap(), Status::Unknown);
    assert_eq!(
        a[1].describe(),
        "snap: install microk8s [snap only (`fallback: false`): snapd is not available (yet)]"
    );
    let e = format!("{:#}", f.apply_one(a[1].as_ref()).unwrap_err());
    assert!(e.contains("can only be installed as a snap"), "{e}");
    assert!(m.runner.lines_of(Mode::Stream).is_empty());
    // snapd came up (installed by an earlier step): the snap is installed.
    m.runner.add_file("/run/snapd.socket");
    m.runner.on("snap list", 0, "Name Version\n");
    assert_eq!(a[1].status().unwrap(), Status::Pending);
    apply(&f, a[1].as_ref());
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        ["sudo -n snap install --classic microk8s"]
    );
    // `fallback` is for snaps only.
    let e = dot_core::config::parse_bundle(
        "steps:\n  - package: { apt: [{ name: x, fallback: false }] }\n",
        std::path::Path::new("/b/bundle.yml"),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("`fallback` is only for snap packages"), "{e}");
}

#[test]
fn a_missing_manager_is_unknown_and_fails_clearly_when_applied() {
    let f = Fixture::new("steps:\n  - package: { brew: [fnm] }\n");
    let m = machine(&[]);
    let a = plan_step(&f, &context(&f, mac()), &m, 0).unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Unknown);
    assert_eq!(
        a[0].describe(),
        "brew: install fnm (brew is not installed yet)"
    );
    assert!(f.apply_err(a[0].as_ref()).contains("brew is not installed"));
    // Installed by an earlier step: the same action now works.
    m.runner.add_program("brew");
    m.runner.on("brew list", 1, "");
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    assert_eq!(apply(&f, a[0].as_ref()), Outcome::Changed);
}

#[test]
fn a_template_that_renders_to_several_words_is_an_error() {
    let f = Fixture::new(
        "vars:\n  pkgs: fd ripgrep\nsteps:\n  - package: [\"{{ pkgs }}\"]\n  - package: [\"{{ nope }}\"]\n",
    );
    let m = machine(&["apt-get", "dpkg-query"]);
    let mut ctx = context(&f, ubuntu());
    ctx.vars = ctx.vars.with_bundle(&f.bundle);
    let e = format!("{:#}", plan_step(&f, &ctx, &m, 0).err().unwrap());
    assert!(e.contains("renders to `fd ripgrep`"), "{e}");
    let e = format!("{:#}", plan_step(&f, &ctx, &m, 1).err().unwrap());
    assert!(e.contains("undefined variable `nope`"), "{e}");
}

fn builtins(m: &Machine) -> Builtins {
    Builtins {
        packages: m.packages.clone(),
        ..Builtins::default()
    }
}

#[test]
fn engine_dry_run_installs_nothing_and_a_real_run_converges() {
    let f = Fixture::new("steps:\n  - package: [fd, jq]\n");
    let m = machine(&["apt-get", "dpkg-query", "sudo"]);
    m.runner.on(DPKG_RULE, 1, "");
    let ctx = context(&f, ubuntu());
    let bundles = std::slice::from_ref(&f.bundle);

    let plan = engine::plan(bundles, &builtins(&m), &ctx);
    assert_eq!(plan.sudo_bundles(), ["demo"]);
    let view = plan.view();
    let dot_core::event::StepViewState::Ready(actions) = &view.bundles[0].steps[0].state else {
        panic!()
    };
    assert_eq!(actions[0].description, "apt: install fd-find, jq [sudo]");
    assert_eq!(actions[0].status, Status::Pending);

    m.runner.clear_calls();
    let rec = Recorder::default();
    let sink = EventSink::new().with(Box::new(rec.clone()));
    let summary = engine::run(
        bundles,
        &builtins(&m),
        &ctx,
        &sink,
        Options {
            dry_run: true,
            ..Default::default()
        },
    );
    assert_eq!(summary.changed, 1);
    assert!(
        m.runner.lines_of(Mode::Stream).is_empty(),
        "a dry run refreshes and installs nothing"
    );

    let summary = engine::run(
        bundles,
        &builtins(&m),
        &ctx,
        &EventSink::new(),
        Options::default(),
    );
    assert!(summary.success(), "{summary:?}");
    assert_eq!(summary.changed, 1);
    let installs: Vec<String> = m
        .runner
        .lines_of(Mode::Stream)
        .into_iter()
        .filter(|l| l.contains("apt-get install"))
        .collect();
    assert_eq!(installs.len(), 1);
    assert!(installs[0].ends_with(" fd-find jq"), "{installs:?}");

    // Now installed: the next run has nothing to do and needs no sudo.
    m.runner.on(DPKG_RULE, 0, "fd-find\tii \njq\tii \n");
    let plan = engine::plan(bundles, &builtins(&m), &ctx);
    assert!(plan.sudo_bundles().is_empty());
    let summary = engine::run(
        bundles,
        &builtins(&m),
        &ctx,
        &EventSink::new(),
        Options::default(),
    );
    assert_eq!((summary.changed, summary.ok), (0, 1), "{summary:?}");
}

#[test]
fn engine_reports_the_missing_packages_when_applying_too() {
    let f = Fixture::new("steps:\n  - package: { brew: [fnm, jq] }\n");
    let m = machine(&["brew"]);
    m.runner.on("brew list", 1, "jq 1.7\n");
    let rec = Recorder::default();
    let sink = EventSink::new().with(Box::new(rec.clone()));
    engine::run(
        std::slice::from_ref(&f.bundle),
        &builtins(&m),
        &context(&f, mac()),
        &sink,
        Options::default(),
    );
    let finished: Vec<String> = rec
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::ActionFinished { description, .. } => Some(description),
            _ => None,
        })
        .collect();
    assert_eq!(finished, ["brew: install fnm (already installed: jq)"]);
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        ["NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_SUDO=1 brew install --formula fnm"]
    );
}

#[test]
fn a_failed_install_fails_the_step_with_the_reason() {
    let f = Fixture::new("steps:\n  - package: { brew: [nope] }\n");
    let m = machine(&["brew"]);
    m.runner.on("brew list", 1, "");
    m.runner.on_err(
        "brew install",
        1,
        "",
        "Error: No available formula with the name \"nope\".\n",
    );
    let a = plan_step(&f, &context(&f, mac()), &m, 0).unwrap();
    a[0].status().unwrap();
    let e = f.apply_err(a[0].as_ref());
    assert!(e.contains("No available formula"), "{e}");
}

#[test]
fn an_install_that_leaves_the_package_missing_warns() {
    let f = Fixture::new("steps:\n  - package: { brew: [python] }\n");
    let m = machine(&["brew"]);
    m.runner.on("brew list", 1, "");
    let a = plan_step(&f, &context(&f, mac()), &m, 0).unwrap();
    assert_eq!(apply(&f, a[0].as_ref()), Outcome::Changed);
    let out: Vec<String> = f
        .recorder
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Output { line, .. } => Some(line),
            _ => None,
        })
        .collect();
    assert!(
        out.iter().any(|l| l.contains("still does not list python")),
        "{out:?}"
    );
}

fn plan_upgrading(
    f: &Fixture,
    ctx: &dot_core::Context,
    m: &Machine,
    upgrade_all: bool,
) -> Vec<Box<dyn Action>> {
    let secrets = SecretRefs::default();
    let pctx = PlanCtx::new(ctx, &f.bundle, &f.bundle.steps[0], &secrets);
    let dot_core::Directive::Package(spec) = &f.bundle.steps[0].directive else {
        panic!("not a package step")
    };
    plan(spec, &pctx, &m.packages, upgrade_all).unwrap()
}

#[test]
fn upgrade_names_the_versions_and_upgrades_only_the_outdated() {
    let f = Fixture::new("steps:\n  - package: { brew: [fd, jq, fnm], upgrade: true }\n");
    let m = machine(&["brew"]);
    m.runner.on("brew list --formula", 1, "fd 10.1\njq 1.7\n");
    m.runner.on(
        "brew outdated --formula --json=v2",
        1,
        r#"{"formulae":[{"name":"fd","installed_versions":["10.1"],"current_version":"10.2"}],"casks":[]}"#,
    );
    let a = plan_upgrading(&f, &context(&f, mac()), &m, false);
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    assert_eq!(
        a[0].describe(),
        "brew: install fnm; upgrade fd 10.1 -> 10.2 (up to date: jq)"
    );
    m.runner.clear_calls();
    assert_eq!(apply(&f, a[0].as_ref()), Outcome::Changed);
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        [
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_SUDO=1 brew install --formula fnm",
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_SUDO=1 brew upgrade --formula fd",
        ]
    );

    // Nothing outdated: satisfied.
    m.runner
        .on("brew list --formula", 0, "fd 10.2\njq 1.7\nfnm 1\n");
    m.runner.on(
        "brew outdated --formula --json=v2",
        0,
        r#"{"formulae":[],"casks":[]}"#,
    );
    let a = plan_upgrading(&f, &context(&f, mac()), &m, false);
    assert_eq!(a[0].status().unwrap(), Status::Satisfied);
    assert_eq!(a[0].describe(), "brew: fd, jq, fnm installed");
}

#[test]
fn by_default_nothing_is_upgraded_unless_the_run_says_so() {
    let f = Fixture::new("steps:\n  - package: { brew: [fd] }\n");
    let m = machine(&["brew"]);
    m.runner.on("brew list --formula", 0, "fd 10.1\n");
    m.runner.on(
        "brew outdated --formula --json=v2",
        1,
        r#"{"formulae":[{"name":"fd","installed_versions":["10.1"],"current_version":"10.2"}],"casks":[]}"#,
    );
    let a = plan_upgrading(&f, &context(&f, mac()), &m, false);
    assert_eq!(a[0].status().unwrap(), Status::Satisfied);
    assert!(
        !m.runner.lines().iter().any(|l| l.contains("outdated")),
        "install-missing-only never asks"
    );
    // `dot apply --upgrade`.
    let a = plan_upgrading(&f, &context(&f, mac()), &m, true);
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    assert_eq!(a[0].describe(), "brew: upgrade fd 10.1 -> 10.2");
}

#[test]
fn apt_upgrades_are_checked_after_refreshing_stale_lists() {
    let f = Fixture::new("steps:\n  - package: { apt: [fd-find, git], upgrade: true }\n");
    let m = machine(&["apt-get", "dpkg-query", "apt-cache", "sudo"]);
    m.runner.on(DPKG_RULE, 0, "fd-find\tii \ngit\tii \n");
    m.runner.on(
        "apt-cache policy",
        0,
        "fd-find:\n  Installed: 8.7.0-3\n  Candidate: 8.7.1-1\ngit:\n  Installed: 1:2.43\n  Candidate: 1:2.43\n",
    );
    let a = plan_upgrading(&f, &context(&f, ubuntu()), &m, false);
    // Never refreshed: what is outdated is only known after `apt-get update`.
    assert_eq!(a[0].status().unwrap(), Status::Unknown);
    let d = a[0].describe();
    assert!(
        d.contains("upgrades of fd-find, git are checked after refreshing the package metadata"),
        "{d}"
    );
    assert!(d.ends_with("; sudo]"), "{d}");
    assert!(a[0].needs_sudo());
    assert!(
        !m.runner.lines().iter().any(|l| l.contains("apt-cache")),
        "no answer from stale lists"
    );
    m.runner.clear_calls();
    assert_eq!(apply(&f, a[0].as_ref()), Outcome::Changed);
    let streamed = m.runner.lines_of(Mode::Stream);
    assert!(streamed[0].ends_with("apt-get update -q"), "{streamed:?}");
    assert!(
        streamed[1].ends_with("--only-upgrade fd-find"),
        "{streamed:?}"
    );
    assert_eq!(streamed.len(), 2);

    // Refreshed today: known up front.
    let a = plan_upgrading(&f, &context(&f, ubuntu()), &m, false);
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    assert_eq!(
        a[0].describe(),
        "apt: upgrade fd-find 8.7.0-3 -> 8.7.1-1 (up to date: git) [sudo]"
    );
}

#[test]
fn a_name_pattern_installs_the_newest_package_it_matches() {
    // dotnet on Ubuntu: the highest `dotnet-sdk-N.M` in the archive, never pinned.
    let f = Fixture::new("steps:\n  - package: { apt: ['dotnet-sdk-*'] }\n");
    let m = machine(&["apt-get", "apt-cache", "dpkg-query", "sudo"]);
    // Nothing known yet (the repository was added earlier in the run): unknown, not an error.
    m.runner.once("apt-cache pkgnames dotnet-sdk-", 0, "", "");
    let ctx = context(&f, ubuntu());
    let actions = plan_step(&f, &ctx, &m, 0).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Unknown);
    assert!(
        actions[0]
            .describe()
            .contains("no package matches dotnet-sdk-* yet"),
        "{}",
        actions[0].describe()
    );
    assert!(actions[0].resources().is_empty(), "no package named yet");
    // After the refresh that applying does first, the newest by version is installed.
    m.runner.on(
        "apt-cache pkgnames dotnet-sdk-",
        0,
        // The Ubuntu archive's names: `-source-built-artifacts` is another package.
        "dotnet-sdk-8.0\ndotnet-sdk-8.0-source-built-artifacts\ndotnet-sdk-10.0\n\
         dotnet-sdk-10.0-source-built-artifacts\ndotnet-sdk-9.0\ndotnet-sdk-aot-10.0\n\
         dotnet-sdk-dbg-10.0\n",
    );
    m.runner.on(DPKG_RULE, 1, "");
    m.runner.clear_calls();
    assert_eq!(apply(&f, actions[0].as_ref()), Outcome::Changed);
    let streamed = m.runner.lines_of(Mode::Stream);
    assert!(streamed[0].ends_with("apt-get update -q"), "{streamed:?}");
    assert!(
        streamed
            .last()
            .unwrap()
            .ends_with(" install -y dotnet-sdk-10.0")
            || streamed.last().unwrap().ends_with(" dotnet-sdk-10.0"),
        "{streamed:?}"
    );
    assert_eq!(actions[0].resources()[0].key(), "pkg:apt:dotnet-sdk-10.0");
    // Installed: satisfied until a newer one appears.
    m.runner.on(DPKG_RULE, 0, "dotnet-sdk-10.0\tii \n");
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert_eq!(actions[0].describe(), "apt: dotnet-sdk-10.0 installed");
    // A manager that cannot search is told so.
    let f = Fixture::new("steps:\n  - package: { brew: ['go@*'] }\n");
    let m = machine(&["brew"]);
    let actions = plan_step(&f, &context(&f, mac()), &m, 0).unwrap();
    let e = format!("{:#}", actions[0].status().unwrap_err());
    assert!(e.contains("cannot look packages up by a pattern"), "{e}");
    assert!(dot_platform::pkg::name_matches(
        "dotnet-sdk-*",
        "dotnet-sdk-10.0"
    ));
    assert!(!dot_platform::pkg::name_matches(
        "dotnet-sdk-?.0",
        "dotnet-sdk-10.0"
    ));
}

#[test]
fn vscode_extensions_are_packages_on_every_os() {
    let f = Fixture::new(
        "steps:\n  - package: { vscode: [ms-python.python, rust-lang.rust-analyzer] }\n",
    );
    for facts in [ubuntu(), mac()] {
        let m = machine(&["code"]);
        m.runner
            .on("code --list-extensions", 0, "MS-Python.python\nother.ext\n");
        let actions = plan_step(&f, &context(&f, facts), &m, 0).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(
            actions[0].describe(),
            "vscode extensions: install rust-lang.rust-analyzer (already installed: \
             ms-python.python)"
        );
        assert!(!actions[0].needs_sudo());
        m.runner.clear_calls();
        assert_eq!(apply(&f, actions[0].as_ref()), Outcome::Changed);
        assert!(
            m.runner
                .lines()
                .contains(&"code --install-extension rust-lang.rust-analyzer".to_string()),
            "{:?}",
            m.runner.lines()
        );
        assert_eq!(
            actions[0].resources()[1].key(),
            "pkg:vscode:rust-lang.rust-analyzer"
        );
    }
    // Without VS Code: unknown (an earlier bundle may install it).
    let m = machine(&[]);
    let actions = plan_step(&f, &context(&f, ubuntu()), &m, 0).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Unknown);
}
