use std::sync::Arc;

use anyhow::Result;
use dot_core::Stream;
use dot_core::config::PkgManager;

use super::*;
use crate::exec::fake::{FakeRunner, Mode};

struct NoNetwork;
impl CaskFetcher for NoNetwork {
    fn fetch(&self, _: &str) -> Result<String> {
        anyhow::bail!("offline")
    }
}

fn setup(programs: &[&str]) -> (Arc<FakeRunner>, Packages, tempfile::TempDir) {
    let r = Arc::new(FakeRunner::with_programs(programs));
    let state = tempfile::tempdir().unwrap();
    let p = Packages::new(
        r.clone(),
        Some(state.path().to_path_buf()),
        Arc::new(NoNetwork),
    );
    (r, p, state)
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn install(
    p: &Packages,
    m: PkgManager,
    pkgs: &[&str],
    opts: InstallOpts,
) -> (Result<()>, Vec<(Stream, String)>) {
    let mut lines = Vec::new();
    let r = p.manager(m).install(&names(pkgs), opts, &mut |s, l| {
        lines.push((s, l.to_string()))
    });
    (r, lines)
}

#[test]
fn brew_formula_and_cask() {
    let (r, p, _s) = setup(&["brew"]);
    r.on(
        "brew list --formula",
        1,
        "fd 10.2.0\nripgrep 14.1.1 14.1.0\n",
    );
    let brew = p.manager(PkgManager::Brew);
    assert!(brew.available());
    assert!(!brew.needs_root());
    assert_eq!(
        brew.installed(&names(&["fd", "jq", "homebrew/core/ripgrep"]))
            .unwrap(),
        [true, false, true]
    );
    assert_eq!(
        r.lines(),
        [
            "HOMEBREW_NO_SUDO=1 brew list --formula --versions fd jq homebrew/core/ripgrep",
            // Not listed under its own name: asked about alone.
            "HOMEBREW_NO_SUDO=1 brew list --formula --versions jq",
        ]
    );
    r.clear_calls();
    install(&p, PkgManager::Brew, &["jq", "fnm"], InstallOpts::default())
        .0
        .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_SUDO=1 brew install --formula jq fnm"
        ]
    );

    r.clear_calls();
    install(
        &p,
        PkgManager::Cask,
        &["wezterm"],
        InstallOpts {
            adopt: true,
            ..Default::default()
        },
    )
    .0
    .unwrap();
    // `--adopt` only for casks.
    install(
        &p,
        PkgManager::Brew,
        &["jq"],
        InstallOpts {
            adopt: true,
            ..Default::default()
        },
    )
    .0
    .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            // A cask's sudo, should it run one, cannot wait for a password nobody can type, and
            // (sudo is not passwordless) Homebrew keeps the sudo timestamp.
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 SUDO_ASKPASS=/usr/bin/false \
             HOMEBREW_NO_SUDO=1 brew install --cask --adopt wezterm",
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_SUDO=1 brew install --formula jq",
        ]
    );

    // A `.pkg` cask: its sudo free to ask (on the controlling terminal, whatever its stdio), its
    // output still streamed.
    r.clear_calls();
    let (res, lines) = install(
        &p,
        PkgManager::Cask,
        &["temurin"],
        InstallOpts {
            sudo: true,
            ..Default::default()
        },
    );
    res.unwrap();
    let mut upgraded = Vec::new();
    p.manager(PkgManager::Cask)
        .upgrade_with_sudo(&names(&["temurin"]), &mut |_, l| {
            upgraded.push(l.to_string())
        })
        .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 brew install --cask temurin",
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 brew upgrade --cask temurin",
        ]
    );
    assert!(r.lines_of(Mode::Interactive).is_empty());
    assert!(lines.is_empty() && upgraded.is_empty());
    r.on_err(
        "brew install --cask temurin",
        1,
        "==> Running installer for temurin with sudo\n",
        "sudo: 3 incorrect password attempts\n",
    );
    let (res, lines) = install(
        &p,
        PkgManager::Cask,
        &["temurin"],
        InstallOpts {
            sudo: true,
            ..Default::default()
        },
    );
    let e = format!("{:#}", res.unwrap_err());
    assert!(e.contains("exited with code 1"), "{e}");
    assert!(!e.contains("metadata"), "{e}");
    assert_eq!(lines.len(), 2, "its output is streamed: {lines:?}");
    // Formulae never let sudo ask.
    r.clear_calls();
    p.manager(PkgManager::Brew)
        .upgrade_with_sudo(&names(&["jq"]), &mut |_, _| {})
        .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        ["NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_SUDO=1 brew upgrade --formula jq"]
    );

    // A streamed cask whose sudo wanted a password (or was disabled) says what to do.
    for err in [
        "sudo: no password was provided\nsudo: a password is required\n",
        "Error: sudo is disabled by HOMEBREW_NO_SUDO.\n",
    ] {
        r.on_err("brew install --cask sneaky", 1, "", err);
        let e = install(&p, PkgManager::Cask, &["sneaky"], InstallOpts::default())
            .0
            .unwrap_err();
        let e = format!("{e:#}");
        assert!(
            e.contains("Homebrew ran sudo for sneaky, although the cask's metadata does not say"),
            "{e}"
        );
        assert!(
            e.contains("run `brew install --cask sneaky` yourself"),
            "{e}"
        );
    }
    r.on_err(
        "brew upgrade --cask sneaky",
        1,
        "",
        "sudo: a password is required\n",
    );
    let e = p
        .manager(PkgManager::Cask)
        .upgrade(&names(&["sneaky"]), &mut |_, _| {})
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("run `brew upgrade --cask sneaky` yourself"),
        "{e:#}"
    );

    r.set_root(true);
    let e = install(&p, PkgManager::Brew, &["jq"], InstallOpts::default()).0;
    assert!(e.unwrap_err().to_string().contains("root"));
}

#[test]
fn casks_keep_homebrew_sudo_when_sudo_is_passwordless() {
    // Passwordless sudo never asks, so nothing is gained by HOMEBREW_NO_SUDO, and a cask that
    // moves an app with sudo after all still works.
    let (r, p, _s) = setup(&["brew", "sudo"]);
    r.on("sudo -n -k true", 0, "");
    for name in ["wezterm", "kitty"] {
        install(&p, PkgManager::Cask, &[name], InstallOpts::default())
            .0
            .unwrap();
    }
    assert_eq!(
        r.lines(),
        [
            "sudo -n -k true",
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 SUDO_ASKPASS=/usr/bin/false brew install \
             --cask wezterm",
            // Checked once.
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 SUDO_ASKPASS=/usr/bin/false brew install \
             --cask kitty",
        ]
    );

    // A password needed: Homebrew keeps the timestamp `dot`'s own root steps use.
    let (r, p, _s) = setup(&["brew", "sudo"]);
    r.on("sudo -n -k true", 1, "");
    install(&p, PkgManager::Cask, &["wezterm"], InstallOpts::default())
        .0
        .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 SUDO_ASKPASS=/usr/bin/false \
          HOMEBREW_NO_SUDO=1 brew install --cask wezterm"
        ]
    );
}

#[test]
fn brew_aliases_and_renames_count_as_installed() {
    // `brew list` prints the canonical name: `python3` is an alias of `python@3.14`.
    let (r, p, _s) = setup(&["brew"]);
    r.on(
        "brew list --formula --versions python3",
        0,
        "python@3.14 3.14.7\n",
    );
    let brew = p.manager(PkgManager::Brew);
    assert_eq!(brew.installed(&names(&["python3"])).unwrap(), [true]);

    // In a batch with a missing package (exit 1), the unmatched names are asked about alone.
    r.on(
        "brew list --formula --versions python3 nope",
        1,
        "python@3.14 3.14.7\n",
    );
    r.on("brew list --formula --versions nope", 1, "");
    r.clear_calls();
    assert_eq!(
        brew.installed(&names(&["python3", "nope"])).unwrap(),
        [true, false]
    );
    assert_eq!(
        r.lines(),
        [
            "HOMEBREW_NO_SUDO=1 brew list --formula --versions python3 nope",
            "HOMEBREW_NO_SUDO=1 brew list --formula --versions python3",
            "HOMEBREW_NO_SUDO=1 brew list --formula --versions nope",
        ]
    );
}

#[test]
fn install_failure_names_the_command_and_last_line() {
    let (r, p, _s) = setup(&["brew"]);
    r.on_err(
        "brew install",
        1,
        "",
        "==> Downloading\nError: No formulae found for nope.\n",
    );
    let (res, lines) = install(&p, PkgManager::Brew, &["nope"], InstallOpts::default());
    let e = res.unwrap_err().to_string();
    assert!(
        e.contains("brew install --formula nope` exited with code 1: Error: No formulae found"),
        "{e}"
    );
    assert!(lines.contains(&(Stream::Stderr, "==> Downloading".into())));
}

#[test]
fn a_query_that_cannot_answer_is_an_error() {
    let (r, p, _s) = setup(&["brew"]);
    r.on_err("brew list", 2, "", "Error: something broke");
    let e = p
        .manager(PkgManager::Brew)
        .installed(&names(&["fd"]))
        .unwrap_err();
    assert!(e.to_string().contains("something broke"), "{e}");
    let (_, p, _s) = setup(&[]);
    assert!(!p.manager(PkgManager::Brew).available());
    assert!(
        p.manager(PkgManager::Brew)
            .installed(&names(&["fd"]))
            .is_err()
    );
}

#[test]
fn apt_queries_dpkg_and_installs_through_sudo() {
    let (r, p, _s) = setup(&["apt-get", "dpkg-query", "sudo"]);
    r.on_err(
        "dpkg-query",
        1,
        "fd-find\tii \ncurl\trc \ngit\thi \n",
        "dpkg-query: no packages found matching ripgrep\n",
    );
    let apt = p.manager(PkgManager::Apt);
    assert!(apt.needs_root());
    assert_eq!(
        apt.installed(&names(&["fd-find", "ripgrep", "curl", "git:amd64"]))
            .unwrap(),
        [true, false, false, true]
    );
    assert_eq!(
        r.lines(),
        ["dpkg-query -W -f ${Package}\t${db:Status-Abbrev}\n fd-find ripgrep curl git:amd64"]
    );
    r.clear_calls();
    install(
        &p,
        PkgManager::Apt,
        &["ripgrep", "curl"],
        InstallOpts::default(),
    )
    .0
    .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y -q -o \
          Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold ripgrep curl"
        ]
    );

    // As root: no sudo, the environment set directly.
    r.clear_calls();
    r.set_root(true);
    install(&p, PkgManager::Apt, &["curl"], InstallOpts::default())
        .0
        .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream)[0],
        "DEBIAN_FRONTEND=noninteractive apt-get install -y -q -o \
         Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold curl"
    );
}

#[test]
fn apt_without_sudo_or_root_says_so() {
    let (_, p, _s) = setup(&["apt-get", "dpkg-query"]);
    let e = install(&p, PkgManager::Apt, &["curl"], InstallOpts::default())
        .0
        .unwrap_err();
    assert!(e.to_string().contains("needs root"), "{e}");
}

fn refresh(p: &Packages, m: PkgManager) -> Vec<(Stream, String)> {
    let mut lines = Vec::new();
    p.manager(m)
        .refresh(&mut |s, l| lines.push((s, l.to_string())))
        .unwrap();
    lines
}

/// apt's refresh stamp in the state store under `dir`.
fn stamp(dir: &std::path::Path) -> Option<u64> {
    dot_core::StateStore::at(dir)
        .section::<linux::AptStamp>(dot_core::sections::APT)
        .refreshed
}

#[test]
fn apt_refreshes_at_most_once_a_day() {
    let (r, p, state) = setup(&["apt-get", "dpkg-query", "sudo"]);
    refresh(&p, PkgManager::Apt);
    assert_eq!(
        r.lines(),
        ["sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update -q"]
    );
    assert!(stamp(state.path()).is_some());
    // Once per run...
    refresh(&p, PkgManager::Apt);
    assert_eq!(r.lines().len(), 1);
    // ... and, in the next run, not again within a day.
    let (r2, _, _s2) = setup(&["apt-get", "dpkg-query", "sudo"]);
    let p2 = Packages::new(
        r2.clone(),
        Some(state.path().to_path_buf()),
        Arc::new(NoNetwork),
    );
    let lines = refresh(&p2, PkgManager::Apt);
    assert!(r2.calls().is_empty());
    assert!(lines[0].1.contains("not refreshing"), "{lines:?}");
    // An old stamp means refresh.
    dot_core::StateStore::at(state.path())
        .update_section(dot_core::sections::APT, |s: &mut linux::AptStamp| {
            s.refreshed = Some(1000)
        })
        .unwrap();
    let p3 = Packages::new(
        r2.clone(),
        Some(state.path().to_path_buf()),
        Arc::new(NoNetwork),
    );
    refresh(&p3, PkgManager::Apt);
    assert_eq!(r2.lines().len(), 1);
    let _ = p;
}

#[test]
fn a_failed_apt_refresh_warns_and_writes_no_stamp() {
    let (r, p, state) = setup(&["apt-get", "dpkg-query", "sudo"]);
    r.on_err(
        "sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update",
        100,
        "",
        "E: Could not resolve 'archive.ubuntu.com'",
    );
    let lines = refresh(&p, PkgManager::Apt);
    let warning = &lines.last().unwrap().1;
    assert!(
        warning.starts_with("warning: `apt-get update` exited with code 100"),
        "{warning}"
    );
    assert!(warning.contains("continuing"), "{warning}");
    assert_eq!(stamp(state.path()), None);
}

#[test]
fn apt_never_trusts_a_stale_list_that_lacks_a_candidate() {
    let (r, p, state) = setup(&["apt-get", "dpkg-query", "sudo"]);
    // Lists refreshed an hour ago (by the stamp, in a version-0 state directory: migrated on
    // first read), but they do not know the package yet.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        state.path().join("apt-update.stamp"),
        format!("{}", now - 3600),
    )
    .unwrap();
    r.once(
        "sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install",
        100,
        "",
        "E: Unable to locate package eza",
    );
    refresh(&p, PkgManager::Apt);
    assert!(r.calls().is_empty(), "stamp is fresh");
    let (res, lines) = install(&p, PkgManager::Apt, &["eza"], InstallOpts::default());
    res.unwrap();
    let cmds = r.lines_of(Mode::Stream);
    assert_eq!(cmds.len(), 3, "{cmds:?}");
    assert!(cmds[0].contains("apt-get install"));
    assert!(cmds[1].contains("apt-get update"));
    assert!(cmds[2].contains("apt-get install"));
    assert!(
        lines
            .iter()
            .any(|(_, l)| l.contains("refreshing the package lists"))
    );

    // Already refreshed in this run: the failure stands.
    r.once(
        "sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install",
        100,
        "",
        "E: Unable to locate package nope",
    );
    let (res, _) = install(&p, PkgManager::Apt, &["nope"], InstallOpts::default());
    assert!(
        res.unwrap_err()
            .to_string()
            .contains("Unable to locate package nope")
    );
}

#[test]
fn rpm_based() {
    let (r, p, _s) = setup(&["dnf", "zypper", "rpm", "sudo"]);
    r.on("rpm -q", 1, "git\npackage nope is not installed\n");
    let dnf = p.manager(PkgManager::Dnf);
    assert_eq!(
        dnf.installed(&names(&["git", "nope"])).unwrap(),
        [true, false]
    );
    assert_eq!(r.lines(), ["rpm -q --qf %{NAME}\n git nope"]);
    install(&p, PkgManager::Dnf, &["nope"], InstallOpts::default())
        .0
        .unwrap();
    install(&p, PkgManager::Zypper, &["nope"], InstallOpts::default())
        .0
        .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "sudo -n dnf install -y nope",
            "sudo -n zypper --non-interactive install --auto-agree-with-licenses nope"
        ]
    );
    assert!(refresh(&p, PkgManager::Dnf).is_empty());
}

#[test]
fn pacman_and_apk() {
    let (r, p, _s) = setup(&["pacman", "apk", "sudo"]);
    r.on_err(
        "pacman -Q",
        1,
        "git 2.45-1\n",
        "error: package 'fd' was not found\n",
    );
    assert_eq!(
        p.manager(PkgManager::Pacman)
            .installed(&names(&["fd", "git"]))
            .unwrap(),
        [false, true]
    );
    r.on("apk info -e", 1, "curl\n");
    assert_eq!(
        p.manager(PkgManager::Apk)
            .installed(&names(&["curl", "fd"]))
            .unwrap(),
        [true, false]
    );
    r.set_root(true);
    install(&p, PkgManager::Pacman, &["fd"], InstallOpts::default())
        .0
        .unwrap();
    install(&p, PkgManager::Apk, &["fd"], InstallOpts::default())
        .0
        .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        ["pacman -S --needed --noconfirm fd", "apk add --no-cache fd"]
    );
}

#[test]
fn snap_batches_all_but_classic() {
    let (r, p, _s) = setup(&["snap", "sudo"]);
    r.on(
        "snap list",
        0,
        "Name    Version   Rev    Tracking       Publisher   Notes\ncore22  20240111  1122   latest/stable  canonical✓  base\nhello   2.10      42     latest/stable  canonical✓  -\n",
    );
    assert_eq!(
        p.manager(PkgManager::Snap)
            .installed(&names(&["hello", "Name", "code"]))
            .unwrap(),
        [true, false, false]
    );
    install(&p, PkgManager::Snap, &["a", "b"], InstallOpts::default())
        .0
        .unwrap();
    install(
        &p,
        PkgManager::Snap,
        &["code", "go"],
        InstallOpts {
            classic: true,
            ..Default::default()
        },
    )
    .0
    .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "sudo -n snap install a b",
            "sudo -n snap install --classic code",
            "sudo -n snap install --classic go"
        ]
    );
    assert!(!p.snapd_available(), "no daemon socket");
    r.add_file("/run/snapd.socket");
    assert!(p.snapd_available());
}

#[test]
fn winget_and_scoop() {
    let (r, p, _s) = setup(&["winget", "scoop"]);
    r.on(
        "winget list --exact --id Git.Git",
        0,
        "Name Id Version\n--------\nGit Git.Git 2.45\n",
    );
    r.on(
        "winget list --exact --id Nope.Nope",
        -1978335212,
        "No installed package found matching input criteria.\n",
    );
    let w = p.manager(PkgManager::Winget);
    assert!(!w.needs_root());
    assert_eq!(
        w.installed(&names(&["git.git", "Nope.Nope"])).unwrap(),
        [false, false]
    );
    assert_eq!(w.installed(&names(&["Git.Git"])).unwrap(), [true]);
    r.clear_calls();
    r.on("winget install", 1, "Installer failed: access denied\n");
    let e = install(
        &p,
        PkgManager::Winget,
        &["Nope.Nope"],
        InstallOpts::default(),
    )
    .0
    .unwrap_err()
    .to_string();
    assert!(e.contains("elevated (Administrator) terminal"), "{e}");
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "winget install --exact --id Nope.Nope --silent --accept-package-agreements --accept-source-agreements --disable-interactivity"
        ]
    );
    r.clear_calls();
    r.on("winget install", 0, "");
    install(
        &p,
        PkgManager::Winget,
        &["Git.Git"],
        InstallOpts {
            machine: true,
            ..Default::default()
        },
    )
    .0
    .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "winget install --exact --id Git.Git --silent --scope machine --accept-package-agreements --accept-source-agreements --disable-interactivity"
        ]
    );

    r.on(
        "scoop list",
        0,
        "Installed apps:\n\nName    Version Source Updated             Info\n----    ------- ------ -------             ----\n7zip    23.01   main   2024-01-01 10:00:00\nvscode  1.90    extras 2024-01-01 10:00:00\n",
    );
    assert_eq!(
        p.manager(PkgManager::Scoop)
            .installed(&names(&["7zip", "extras/vscode", "git"]))
            .unwrap(),
        [true, true, false]
    );
    install(
        &p,
        PkgManager::Scoop,
        &["git", "fd"],
        InstallOpts::default(),
    )
    .0
    .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream).last().unwrap(),
        "scoop install git fd"
    );
}

#[test]
fn cask_info_is_read_from_the_api_once_per_run() {
    struct Counting(std::sync::Mutex<Vec<String>>);
    impl CaskFetcher for Counting {
        fn fetch(&self, token: &str) -> Result<String> {
            self.0.lock().unwrap().push(token.to_string());
            match token {
                "wezterm" => Ok(
                    r#"{"token":"wezterm","artifacts":[{"app":["WezTerm.app"]},{"zap":[]}]}"#
                        .into(),
                ),
                "karabiner-elements" => {
                    Ok(r#"{"artifacts":[{"pkg":["Karabiner.pkg"]},{"uninstall":[]}]}"#.into())
                }
                "font-fira-code" => Ok(r#"{"artifacts":[{"font":["FiraCode.ttf"]}]}"#.into()),
                _ => anyhow::bail!("offline"),
            }
        }
    }
    let fetcher = Arc::new(Counting(Default::default()));
    let p = Packages::new(Arc::new(FakeRunner::default()), None, fetcher.clone());
    assert_eq!(
        p.cask_info("wezterm"),
        Ok(CaskInfo {
            app: true,
            pkg: false
        })
    );
    assert_eq!(
        p.cask_info("wezterm"),
        Ok(CaskInfo {
            app: true,
            pkg: false
        })
    );
    assert_eq!(
        p.cask_info("karabiner-elements"),
        Ok(CaskInfo {
            app: false,
            pkg: true
        })
    );
    assert_eq!(p.cask_info("font-fira-code"), Ok(CaskInfo::default()));
    assert!(p.cask_info("other").unwrap_err().contains("offline"));
    assert!(p.cask_info("other").is_err());
    assert!(p.cask_info("owner/tap/x").unwrap_err().contains("taps"));
    assert_eq!(
        *fetcher.0.lock().unwrap(),
        ["wezterm", "karabiner-elements", "font-fira-code", "other"]
    );
}

#[test]
fn curl_fetcher_argv() {
    let r = Arc::new(FakeRunner::with_programs(&["curl"]));
    r.on("curl", 0, "{}");
    assert_eq!(CurlFetcher::new(r.clone()).fetch("wezterm").unwrap(), "{}");
    assert_eq!(
        r.lines(),
        ["curl -fsSL --max-time 15 https://formulae.brew.sh/api/cask/wezterm.json"]
    );
    assert!(parse_cask_info("{}").is_err());
    assert!(parse_cask_info("not json").is_err());
}

#[test]
fn cask_info_needs_root_as_homebrew_says() {
    let pkg = |json: &str| parse_cask_info(json).unwrap().pkg;
    assert!(pkg(r#"{"artifacts":[{"pkg":["Temurin.pkg"]}]}"#));
    assert!(pkg(
        r#"{"artifacts":[{"keyboard_layout":["Layout.bundle"]}]}"#
    ));
    // An installer script only with `sudo: true`; a manual installer never.
    assert!(pkg(
        r#"{"artifacts":[{"installer":[{"script":{"executable":"install.sh","sudo":true}}]}]}"#
    ));
    assert!(!pkg(
        r#"{"artifacts":[{"installer":[{"script":{"executable":"Miniconda3.sh","args":["-b"]}}]}]}"#
    ));
    assert!(!pkg(
        r#"{"artifacts":[{"installer":[{"script":{"executable":"x.sh","sudo":false}}]}]}"#
    ));
    assert!(!pkg(
        r#"{"artifacts":[{"installer":[{"manual":"Install X.app"}]}]}"#
    ));
    // Install-time steps with `sudo: true` (or one that always runs as root); uninstall steps
    // and the `uninstall` stanza do not run on install.
    assert!(pkg(
        r#"{"artifacts":[{"postflight_steps":[{"steps":[{"type":"run","sudo":true}]}]}]}"#
    ));
    assert!(pkg(
        r#"{"artifacts":[{"preflight_steps":[{"steps":[{"type":"delete_keychain_certificate"}]}]}]}"#
    ));
    assert!(!pkg(
        r#"{"artifacts":[{"postflight_steps":[{"steps":[{"type":"move","sudo":"if_needed"}]}]}]}"#
    ));
    assert!(!pkg(
        r#"{"artifacts":[{"uninstall_preflight_steps":[{"steps":[{"type":"run","sudo":true}]}]},{"uninstall":[{"script":{"executable":"x","sudo":true}}]},{"app":["X.app"]}]}"#
    ));
}

#[test]
fn the_lock_serialises_installs() {
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::scope(|s| {
        for _ in 0..4 {
            let (active, max) = (active.clone(), max.clone());
            s.spawn(move || {
                let _g = lock();
                let n = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                max.fetch_max(n, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
                active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
    });
    assert_eq!(max.load(std::sync::atomic::Ordering::SeqCst), 1);
}

fn outdated(p: &Packages, m: PkgManager, pkgs: &[&str]) -> Vec<(String, String, String)> {
    p.manager(m)
        .outdated(&names(pkgs))
        .unwrap()
        .into_iter()
        .map(|o| (o.name, o.current, o.latest))
        .collect()
}

fn upgrade(p: &Packages, m: PkgManager, pkgs: &[&str]) -> Result<()> {
    p.manager(m).upgrade(&names(pkgs), &mut |_, _| {})
}

fn o(n: &str, c: &str, l: &str) -> (String, String, String) {
    (n.into(), c.into(), l.into())
}

#[test]
fn brew_outdated_and_upgrade() {
    let (r, p, _s) = setup(&["brew"]);
    r.on(
        "brew outdated --formula --json=v2",
        1,
        r#"{"formulae":[{"name":"dav1d","installed_versions":["1.5.2","1.5.3"],"current_version":"1.5.4","pinned":false},
                       {"name":"node","installed_versions":["20.0"],"current_version":"22.0","pinned":true}],"casks":[]}"#,
    );
    assert_eq!(
        outdated(&p, PkgManager::Brew, &["dav1d", "node", "jq"]),
        [o("dav1d", "1.5.3", "1.5.4")],
        "pinned formulae are left alone"
    );
    r.on(
        "brew outdated --cask --json=v2",
        1,
        r#"{"formulae":[],"casks":[{"name":"wezterm","installed_versions":"2024","current_version":"2025"}]}"#,
    );
    assert_eq!(
        outdated(&p, PkgManager::Cask, &["wezterm"]),
        [o("wezterm", "2024", "2025")]
    );
    assert_eq!(
        r.lines_of(Mode::Output),
        [
            "HOMEBREW_NO_SUDO=1 HOMEBREW_NO_AUTO_UPDATE=1 brew outdated --formula --json=v2 \
             dav1d node jq",
            "HOMEBREW_NO_SUDO=1 HOMEBREW_NO_AUTO_UPDATE=1 brew outdated --cask --json=v2 wezterm",
        ]
    );
    upgrade(&p, PkgManager::Cask, &["wezterm"]).unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 SUDO_ASKPASS=/usr/bin/false \
             HOMEBREW_NO_SUDO=1 brew upgrade --cask wezterm"
        ]
    );
    r.on("brew outdated", 1, "Error: not json");
    assert!(
        p.manager(PkgManager::Brew)
            .outdated(&names(&["x"]))
            .is_err()
    );
}

#[test]
fn apt_outdated_upgrade_staleness_and_invalidation() {
    let (r, p, state) = setup(&["apt-get", "dpkg-query", "apt-cache", "sudo"]);
    r.on(
        "apt-cache policy",
        0,
        "fd-find:\n  Installed: 8.7.0-3\n  Candidate: 8.7.1-1\n  Version table:\n \
         *** 8.7.0-3 500\ngit:\n  Installed: 1:2.43.0-1\n  Candidate: 1:2.43.0-1\n\
         gh:\n  Installed: (none)\n  Candidate: 2.60.0\n",
    );
    r.on("dpkg-query -W", 0, "fd-find\tii \ngit\tii \nlibc6\thi \n");
    assert_eq!(
        outdated(
            &p,
            PkgManager::Apt,
            &["fd-find:amd64", "git", "gh", "libc6"]
        ),
        [o("fd-find:amd64", "8.7.0-3", "8.7.1-1")]
    );
    assert_eq!(
        r.lines_of(Mode::Output),
        [
            "dpkg-query -W -f ${Package}\t${db:Status-Abbrev}\n fd-find:amd64 git gh libc6",
            "LC_ALL=C apt-cache policy fd-find:amd64 git gh"
        ],
        "a held package (libc6, `hi`) is not asked about: apt would refuse to upgrade it"
    );
    r.clear_calls();
    r.on("dpkg-query -W", 0, "libc6\thi \n");
    assert!(outdated(&p, PkgManager::Apt, &["libc6"]).is_empty());
    assert!(
        !r.lines().iter().any(|l| l.contains("apt-cache")),
        "only held packages: nothing to ask"
    );
    r.clear_calls();
    upgrade(&p, PkgManager::Apt, &["fd-find"]).unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y -q -o \
             Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold --only-upgrade \
             fd-find"
        ]
    );

    let apt = p.manager(PkgManager::Apt);
    assert!(apt.stale(), "never refreshed");
    apt.refresh(&mut |_, _| {}).unwrap();
    assert!(!apt.stale());
    assert!(stamp(state.path()).is_some());
    // A repository was added: the next refresh must really refresh.
    apt.invalidate();
    assert_eq!(stamp(state.path()), None);
    assert!(apt.stale());
    r.clear_calls();
    apt.refresh(&mut |_, _| {}).unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        ["sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update -q"]
    );
}

#[test]
fn rpm_based_outdated_and_upgrade() {
    let (r, p, _s) = setup(&["dnf", "rpm", "zypper", "sudo"]);
    r.on(
        "dnf check-update -q",
        100,
        "\ngh.x86_64                 2.60.0-1          gh-cli\nfd-find.x86_64   10.2.0-1.fc40   updates\n",
    );
    r.on(
        "rpm -q",
        1,
        "gh 2.50.0-1\nfd-find 10.1.0-1.fc40\npackage jq is not installed\n",
    );
    assert_eq!(
        outdated(&p, PkgManager::Dnf, &["gh", "jq"]),
        [o("gh", "2.50.0-1", "2.60.0-1")]
    );
    assert_eq!(
        r.lines_of(Mode::Output),
        [
            "dnf check-update -q gh jq",
            "rpm -q --qf %{NAME} %{VERSION}-%{RELEASE}\n gh jq"
        ]
    );
    r.on("dnf check-update -q", 0, "");
    assert!(outdated(&p, PkgManager::Dnf, &["gh"]).is_empty());
    upgrade(&p, PkgManager::Dnf, &["gh"]).unwrap();

    r.on(
        "zypper --non-interactive --quiet list-updates",
        0,
        "S | Repository | Name | Current Version | Available Version | Arch\n\
         --+------------+------+-----------------+-------------------+-------\n\
         v | gh         | gh   | 2.50.0-1        | 2.60.0-1          | x86_64\n",
    );
    assert_eq!(
        outdated(&p, PkgManager::Zypper, &["gh", "jq"]),
        [o("gh", "2.50.0-1", "2.60.0-1")]
    );
    upgrade(&p, PkgManager::Zypper, &["gh"]).unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "sudo -n dnf upgrade -y gh",
            "sudo -n zypper --non-interactive update --auto-agree-with-licenses gh"
        ]
    );
}

#[test]
fn pacman_apk_snap_outdated_and_upgrade() {
    let (r, p, _s) = setup(&["pacman", "apk", "snap", "sudo"]);
    r.on(
        "pacman -Qu",
        0,
        "fd 10.1.0-1 -> 10.2.0-1\nripgrep 14.0-1 -> 14.1-1 [ignored]\n",
    );
    assert_eq!(
        outdated(&p, PkgManager::Pacman, &["fd", "jq"]),
        [o("fd", "10.1.0-1", "10.2.0-1")]
    );
    r.on("pacman -Qu", 1, "");
    assert!(outdated(&p, PkgManager::Pacman, &["fd"]).is_empty());
    upgrade(&p, PkgManager::Pacman, &["fd"]).unwrap();

    r.on(
        "apk version",
        0,
        "Installed:                                Available:\nbusybox-extras-1.36.1-r15   < 1.36.1-r16\n",
    );
    assert_eq!(
        outdated(&p, PkgManager::Apk, &["busybox-extras"]),
        [o("busybox-extras", "1.36.1-r15", "1.36.1-r16")]
    );
    upgrade(&p, PkgManager::Apk, &["busybox-extras"]).unwrap();

    r.on(
        "snap refresh --list",
        0,
        "Name  Version  Rev  Size  Publisher  Notes\ncode  1.95.0   180  300MB  vscode  classic\n",
    );
    r.on(
        "snap list",
        0,
        "Name  Version  Rev  Tracking  Publisher  Notes\ncode  1.94.0  170  latest/stable  vscode  classic\nhello 2.10 42 latest/stable canonical -\n",
    );
    assert_eq!(
        outdated(&p, PkgManager::Snap, &["code", "hello"]),
        [o("code", "1.94.0", "1.95.0")]
    );
    upgrade(&p, PkgManager::Snap, &["code"]).unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "sudo -n pacman -S --noconfirm fd",
            "sudo -n apk add --no-cache --upgrade busybox-extras",
            "sudo -n snap refresh code"
        ]
    );
    let queries = r.lines_of(Mode::Output);
    assert_eq!(queries[..2], ["pacman -Qu fd jq", "pacman -Qu fd"]);
    assert!(queries.contains(&"apk version -l < busybox-extras".to_string()));
}

#[test]
fn winget_and_scoop_outdated_and_upgrade() {
    let (r, p, _s) = setup(&["winget", "scoop"]);
    r.on(
        "winget list --exact --id Vim.Vim --upgrade-available",
        0,
        "Name  Id       Version  Available  Source\n---------------------------------------\nVim   Vim.Vim  9.0   9.1     winget\n",
    );
    r.on(
        "winget list --exact --id Neovim.Neovim",
        1,
        "No installed package found",
    );
    assert_eq!(
        outdated(&p, PkgManager::Winget, &["Vim.Vim", "Neovim.Neovim"]),
        [o("Vim.Vim", "9.0", "9.1")]
    );
    upgrade(&p, PkgManager::Winget, &["Vim.Vim"]).unwrap();
    r.on(
        "scoop status",
        0,
        "Name   Installed Version Latest Version Missing Dependencies Info\n----   ----------------- -------------- -------------------- ----\nneovim 0.10.0            0.10.1\n",
    );
    assert_eq!(
        outdated(&p, PkgManager::Scoop, &["neovim", "fd"]),
        [o("neovim", "0.10.0", "0.10.1")]
    );
    upgrade(&p, PkgManager::Scoop, &["neovim"]).unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "winget upgrade --exact --id Vim.Vim --silent --accept-package-agreements \
             --accept-source-agreements --disable-interactivity",
            "scoop update neovim"
        ]
    );
    assert_eq!(
        windows::parse_winget_upgrade_row(
            "Name Id Version Available Source\n----\nVisual Studio Code  Microsoft.VisualStudioCode  1.94.0  1.95.0  winget\n",
            "microsoft.visualstudiocode"
        ),
        Some(("1.94.0".into(), "1.95.0".into())),
        "names with spaces"
    );
}

#[test]
fn uninstall_commands() {
    let (r, p, _s) = setup(&["apt-get", "dpkg-query", "sudo", "brew", "snap", "scoop"]);
    let mut out = |_: Stream, _: &str| {};
    p.manager(PkgManager::Apt)
        .uninstall(&names(&["fd-find"]), &mut out)
        .unwrap();
    p.manager(PkgManager::Cask)
        .uninstall(&names(&["wezterm"]), &mut out)
        .unwrap();
    p.manager(PkgManager::Snap)
        .uninstall(&names(&["code"]), &mut out)
        .unwrap();
    p.manager(PkgManager::Scoop)
        .uninstall(&names(&["fd"]), &mut out)
        .unwrap();
    assert_eq!(
        r.lines_of(Mode::Stream),
        [
            "sudo -n env DEBIAN_FRONTEND=noninteractive apt-get remove -y -q fd-find",
            "NONINTERACTIVE=1 brew uninstall --cask wezterm",
            "sudo -n snap remove code",
            "scoop uninstall fd",
        ]
    );
}

#[test]
fn a_pattern_prefers_a_bare_version_to_a_longer_name() {
    use super::newest;
    let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let ubuntu = names(&[
        "dotnet-sdk-8.0",
        "dotnet-sdk-8.0-source-built-artifacts",
        "dotnet-sdk-10.0-source-built-artifacts",
        "dotnet-sdk-10.0",
        "dotnet-sdk-aot-10.0",
        "dotnet-sdk-dbg-10.0",
    ]);
    assert_eq!(
        newest("dotnet-sdk-*", &ubuntu).map(String::as_str),
        Some("dotnet-sdk-10.0")
    );
    // Only longer names: still the newest version.
    let only = names(&["x-1.0-extra", "x-2.0-extra"]);
    assert_eq!(
        newest("x-*", &only).map(String::as_str),
        Some("x-2.0-extra")
    );
    // A literal suffix after the wildcard is not part of the version.
    let jdk = names(&["openjdk-17-jdk", "openjdk-21-jdk", "openjdk-8-jdk"]);
    assert_eq!(
        newest("openjdk-*-jdk", &jdk).map(String::as_str),
        Some("openjdk-21-jdk")
    );
}
