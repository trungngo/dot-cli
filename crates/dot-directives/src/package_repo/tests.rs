use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;

use dot_core::facts::{Facts, Role};
use dot_core::{Planner, SecretRefs};
use dot_platform::exec::fake::{FakeRunner, Mode};
use dot_platform::pkg::CaskFetcher;

use super::*;
use crate::Builtins;
use crate::testutil::Fixture;

const ARMORED: &str =
    "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nmQINBFit...\n-----END PGP PUBLIC KEY BLOCK-----\n";
const FPR: &str = "9DC858229FC7DD38854AE2D88D81803C0EBFCD88";

/// Keys and suite probes without the network: `keys` maps a URL to the bytes it serves,
/// `suites` a Release URL to whether it exists (`Err`: offline).
#[derive(Default)]
struct FakeNet {
    keys: BTreeMap<String, Vec<u8>>,
    suites: BTreeMap<String, Result<bool, String>>,
    log: std::sync::Mutex<Vec<String>>,
}

impl RepoFetcher for FakeNet {
    fn download(&self, url: &str, dest: &Path) -> Result<()> {
        self.log.lock().unwrap().push(format!("GET {url}"));
        match self.keys.get(url) {
            Some(b) => Ok(fs::write(dest, b)?),
            None => bail!("downloading {url} failed: curl exited with code 22"),
        }
    }

    fn exists(&self, url: &str) -> Result<bool, String> {
        self.log.lock().unwrap().push(format!("HEAD {url}"));
        self.suites.get(url).cloned().unwrap_or(Ok(false))
    }
}

struct NoCasks;
impl CaskFetcher for NoCasks {
    fn fetch(&self, _: &str) -> Result<String> {
        bail!("offline")
    }
}

struct Machine {
    runner: Arc<FakeRunner>,
    builtins: Builtins,
    net: Arc<FakeNet>,
    dir: tempfile::TempDir,
}

fn machine(programs: &[&str], net: FakeNet) -> Machine {
    let runner = Arc::new(FakeRunner::with_programs(programs));
    let dir = tempfile::tempdir().unwrap();
    let packages = Packages::new(
        runner.clone(),
        Some(dir.path().join("state")),
        Arc::new(NoCasks),
    );
    let net = Arc::new(net);
    let builtins = Builtins {
        packages,
        repos: RepoNet(net.clone()),
        ..Builtins::default()
    };
    Machine {
        runner,
        builtins,
        net,
        dir,
    }
}

fn facts(os: Os, pkg: &str) -> Facts {
    let mut f = Facts::new(os, "x86_64", Role::Desktop);
    f.pkg = Some(pkg.into());
    if os == Os::Linux {
        f.family = Some("debian".into());
        f.distro = Some("ubuntu".into());
        f.codename = Some("questing".into());
    }
    f
}

fn plan_on(f: &Fixture, m: &Machine, facts: Facts) -> Result<Vec<Box<dyn Action>>> {
    let ctx = dot_core::Context::new(facts, f.home().to_path_buf());
    let refs = SecretRefs::default();
    let s = &f.bundle.steps[0];
    let pctx = PlanCtx::new(&ctx, &f.bundle, s, &refs);
    m.builtins.plan(&s.directive, &pctx)
}

fn apply(f: &Fixture, a: &dyn Action) -> Outcome {
    f.apply_one(a).unwrap_or_else(|e| panic!("{e:#}"))
}

/// A docker-like apt repository with its files in the machine's temp dir.
fn docker(m: &Machine, extra: &str) -> Fixture {
    let d = m.dir.path();
    Fixture::new(&format!(
        "steps:\n  - package_repo:\n      name: docker\n      apt:\n        url: https://download.docker.com/linux/ubuntu\n        key: https://download.docker.com/linux/ubuntu/gpg\n        suites: \"{{{{ codename }}}}\"\n        fallback_suite: noble\n        components: [stable]\n        keyring: {}/keyrings/docker.gpg\n        file: {}/sources/docker.sources\n{extra}",
        d.display(),
        d.display()
    ))
}

fn sources(m: &Machine, suite: &str) -> String {
    format!(
        "# Managed by dot (package_repo docker); changes are overwritten.\nTypes: deb\n\
         URIs: https://download.docker.com/linux/ubuntu\nSuites: {suite}\nComponents: stable\n\
         Architectures: amd64\nSigned-By: {}/keyrings/docker.gpg\n",
        m.dir.path().display()
    )
}

#[test]
fn apt_key_is_dearmored_checked_and_installed_as_root_then_the_source_falls_back() {
    let mut net = FakeNet::default();
    net.keys.insert(
        "https://download.docker.com/linux/ubuntu/gpg".into(),
        ARMORED.as_bytes().to_vec(),
    );
    net.suites.insert(
        "https://download.docker.com/linux/ubuntu/dists/questing/Release".into(),
        Ok(false),
    );
    let m = machine(&["apt-get", "dpkg-query", "sudo", "gpg"], net);
    m.runner.on(
        "gpg --batch --no-tty --homedir",
        0,
        &format!("pub:-:4096:1:8D81803C0EBFCD88:1:::-:::scESA:\nfpr:::::::::{FPR}:\n"),
    );
    let f = docker(
        &m,
        &format!(
            "        fingerprint: \"{}\"\n",
            "9DC8 5822 9FC7 DD38 854A  E2D8 8D81 803C 0EBF CD88"
        ),
    );
    let actions = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(actions.len(), 2);
    let d = m.dir.path().display().to_string();
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert_eq!(
        actions[0].describe(),
        format!(
            "apt keyring {d}/keyrings/docker.gpg <- https://download.docker.com/linux/ubuntu/gpg \
             (fingerprint checked before installing; sudo)"
        )
    );
    assert_eq!(actions[1].status().unwrap(), Status::Pending);
    assert_eq!(
        actions[1].describe(),
        format!(
            "apt source {d}/sources/docker.sources (suite noble: the repository has nothing for \
             questing; new file, 7 lines; sudo)"
        )
    );
    assert!(actions.iter().all(|a| a.needs_sudo()));
    assert!(m.runner.calls().is_empty(), "no key yet: nothing to check");

    // Applying: download, dearmor, check the fingerprint, then install as root.
    assert_eq!(apply(&f, actions[0].as_ref()), Outcome::Changed);
    let calls = m.runner.lines();
    assert_eq!(calls.len(), 4, "{calls:#?}");
    assert!(
        calls[0].starts_with("gpg --batch --no-tty --homedir "),
        "{calls:#?}"
    );
    assert!(calls[0].contains(" --yes --output ") && calls[0].contains(" --dearmor "));
    assert!(calls[1].contains(" --show-keys --with-colons --with-fingerprint "));
    assert!(
        calls[1].ends_with("/key.gpg"),
        "the dearmored key is checked: {calls:#?}"
    );
    assert_eq!(calls[2], format!("sudo -n install -d -m 0755 {d}/keyrings"));
    assert!(
        calls[3].starts_with("sudo -n install -m 0644 ")
            && calls[3].ends_with(&format!("/key.gpg {d}/keyrings/docker.gpg")),
        "{calls:#?}"
    );
    m.runner.clear_calls();
    assert_eq!(apply(&f, actions[1].as_ref()), Outcome::Changed);
    let calls = m.runner.lines();
    assert_eq!(calls[0], format!("sudo -n install -d -m 0755 {d}/sources"));
    assert!(calls[1].ends_with(&format!(" {d}/sources/docker.sources")));
    assert_eq!(
        m.net.log.lock().unwrap().clone(),
        [
            "HEAD https://download.docker.com/linux/ubuntu/dists/questing/Release",
            "GET https://download.docker.com/linux/ubuntu/gpg"
        ],
        "probed once per plan"
    );

    // With the files in place: satisfied (the keyring's fingerprint checked with gpg).
    fs::create_dir_all(m.dir.path().join("keyrings")).unwrap();
    fs::create_dir_all(m.dir.path().join("sources")).unwrap();
    fs::write(m.dir.path().join("keyrings/docker.gpg"), [0x99, 0x02]).unwrap();
    fs::write(
        m.dir.path().join("sources/docker.sources"),
        sources(&m, "noble"),
    )
    .unwrap();
    let actions = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert_eq!(actions[1].status().unwrap(), Status::Satisfied);
    assert_eq!(
        actions[0].describe(),
        format!("apt keyring {d}/keyrings/docker.gpg")
    );
}

#[test]
fn a_key_with_the_wrong_fingerprint_is_refused_before_anything_is_installed() {
    let mut net = FakeNet::default();
    net.keys.insert(
        "https://download.docker.com/linux/ubuntu/gpg".into(),
        vec![0x99, 0x02, 0x0d],
    );
    let m = machine(&["apt-get", "dpkg-query", "sudo", "gpg"], net);
    m.runner.on(
        "gpg",
        0,
        "pub:-:4096:1:7777888899990000:1:::-:::scESA:\n\
         fpr:::::::::1111222233334444555566667777888899990000:\n",
    );
    let f = docker(&m, &format!("        fingerprint: {FPR}\n"));
    let actions = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    let e = f.apply_err(actions[0].as_ref());
    assert!(
        e.contains("its key is 1111222233334444555566667777888899990000, not 9DC8"),
        "{e}"
    );
    assert!(e.contains("refusing to install it"), "{e}");
    let calls = m.runner.lines();
    assert_eq!(calls.len(), 1, "a binary key is not dearmored: {calls:?}");
    assert!(!calls.iter().any(|c| c.contains("install")), "{calls:?}");

    // An installed keyring without the fingerprint is replaced.
    fs::create_dir_all(m.dir.path().join("keyrings")).unwrap();
    fs::write(m.dir.path().join("keyrings/docker.gpg"), [0x99]).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert!(
        actions[0]
            .describe()
            .contains("the installed keyring is not the pinned key (its key is 1111222233334444555566667777888899990000, not 9DC8"),
        "{}",
        actions[0].describe()
    );
}

#[test]
fn a_download_with_an_extra_key_is_refused_even_though_it_holds_the_pinned_one() {
    let mut net = FakeNet::default();
    net.keys.insert(
        "https://download.docker.com/linux/ubuntu/gpg".into(),
        vec![0x99, 0x02, 0x0d],
    );
    let m = machine(&["apt-get", "dpkg-query", "sudo", "gpg"], net);
    // The vendor's key and a second primary key in one file.
    m.runner.on(
        "gpg",
        0,
        &format!(
            "pub:-:4096:1:8D81803C0EBFCD88:1:::-:::scESA:\nfpr:::::::::{FPR}:\n\
             pub:-:255:22:0123456789ABAAAA:1:::-:::scESC:\n\
             fpr:::::::::0123456789ABCDEF0123456789ABCDEF0123AAAA:\n"
        ),
    );
    let f = docker(&m, &format!("        fingerprint: {FPR}\n"));
    let actions = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("it holds 2 keys"), "{e}");
    assert!(e.contains("refusing to install it"), "{e}");
    assert!(
        !m.runner.lines().iter().any(|c| c.contains("install -m")),
        "nothing installed: {:?}",
        m.runner.lines()
    );
    // A keyring already in place with the extra key is not "good" either.
    fs::create_dir_all(m.dir.path().join("keyrings")).unwrap();
    fs::write(m.dir.path().join("keyrings/docker.gpg"), [0x99]).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
}

#[test]
fn a_published_suite_is_used_and_an_offline_probe_keeps_the_fallback() {
    let mut net = FakeNet::default();
    net.suites.insert(
        "https://download.docker.com/linux/ubuntu/dists/questing/Release".into(),
        Ok(true),
    );
    let m = machine(&["apt-get", "dpkg-query", "sudo"], net);
    let f = docker(&m, "");
    let a = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert!(a[1].describe().starts_with(&format!(
        "apt source {}/sources/docker.sources (new file",
        m.dir.path().display()
    )));
    // The fallback is in place and the vendor now publishes questing: switched.
    fs::create_dir_all(m.dir.path().join("sources")).unwrap();
    fs::write(
        m.dir.path().join("sources/docker.sources"),
        sources(&m, "noble"),
    )
    .unwrap();
    let a = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(a[1].status().unwrap(), Status::Pending);
    assert!(
        a[1].describe().contains("+1/-1 lines"),
        "{}",
        a[1].describe()
    );

    // Offline: the fallback that is there is kept; with nothing there, unknown.
    let mut net = FakeNet::default();
    net.suites.insert(
        "https://download.docker.com/linux/ubuntu/dists/questing/Release".into(),
        Err("curl exited with code 6: Could not resolve host".into()),
    );
    let m2 = machine(&["apt-get", "dpkg-query", "sudo"], net);
    let f2 = docker(&m2, "");
    let a = plan_on(&f2, &m2, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(a[1].status().unwrap(), Status::Unknown);
    let e = f2.apply_err(a[1].as_ref());
    assert!(
        e.contains("cannot tell whether the repository has suite questing"),
        "{e}"
    );
    fs::create_dir_all(m2.dir.path().join("sources")).unwrap();
    fs::write(
        m2.dir.path().join("sources/docker.sources"),
        sources(&m2, "noble"),
    )
    .unwrap();
    let a = plan_on(&f2, &m2, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(a[1].status().unwrap(), Status::Satisfied);
    assert!(
        a[1].describe().contains("is unknown"),
        "{}",
        a[1].describe()
    );

    // The current suite in place: no probe at all.
    fs::write(
        m2.dir.path().join("sources/docker.sources"),
        sources(&m2, "questing"),
    )
    .unwrap();
    m2.net.log.lock().unwrap().clear();
    let a = plan_on(&f2, &m2, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(a[1].status().unwrap(), Status::Satisfied);
    assert!(m2.net.log.lock().unwrap().is_empty());
}

#[test]
fn a_repository_change_makes_the_next_package_step_refresh() {
    let m = machine(&["apt-get", "dpkg-query", "sudo"], FakeNet::default());
    let apt = m.builtins.packages.manager(PkgManager::Apt);
    apt.refresh(&mut |_, _| {}).unwrap();
    assert!(!apt.stale(), "refreshed");
    let f = Fixture::new(&format!(
        "steps:\n  - package_repo:\n      name: gh\n      apt: {{ url: https://cli.github.com/packages, suites: stable, file: {}/gh.sources }}\n",
        m.dir.path().display()
    ));
    let a = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(a.len(), 1, "no key: no keyring");
    apply(&f, a[0].as_ref());
    assert!(apt.stale(), "the stamp was reset");
    m.runner.clear_calls();
    apt.refresh(&mut |_, _| {}).unwrap();
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        ["sudo -n env DEBIAN_FRONTEND=noninteractive apt-get update -q"]
    );
}

#[test]
fn codename_must_be_known_and_if_missing_keeps_an_existing_file() {
    let m = machine(&["apt-get", "dpkg-query", "sudo"], FakeNet::default());
    let f = docker(&m, "        if_missing: true\n");
    let mut fx = facts(Os::Linux, "apt");
    fx.codename = None;
    let e = format!("{:#}", plan_on(&f, &m, fx).err().unwrap());
    assert!(e.contains("the `codename` fact is unknown"), "{e}");
    fs::create_dir_all(m.dir.path().join("sources")).unwrap();
    fs::write(
        m.dir.path().join("sources/docker.sources"),
        "maintained by the package\n",
    )
    .unwrap();
    let a = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(a[1].status().unwrap(), Status::Satisfied);
    assert!(a[1].describe().ends_with("(exists; kept as it is)"));
    assert!(m.net.log.lock().unwrap().is_empty(), "no probe needed");
}

#[test]
fn only_this_machines_manager_is_used() {
    let m = machine(
        &["brew", "dnf", "rpm", "sudo", "scoop", "winget"],
        FakeNet::default(),
    );
    let f = Fixture::new(&format!(
        "steps:\n  - package_repo:\n      name: gh\n      apt: {{ url: https://cli.github.com/packages, suites: stable }}\n      dnf: {{ baseurl: https://cli.github.com/packages/rpm, gpgkey: https://cli.github.com/packages/key, file: {}/gh.repo }}\n      brew: cli/tap\n      scoop: {{ bucket: gh, url: https://example.com/gh.git }}\n      winget: {{ source: gh, url: https://w.example.com }}\n",
        m.dir.path().display()
    ));
    let kinds = |fx: Facts| -> Vec<String> {
        plan_on(&f, &m, fx)
            .unwrap()
            .iter()
            .map(|a| {
                a.describe()
                    .split(' ')
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    };
    let mut fedora = facts(Os::Linux, "dnf");
    fedora.family = Some("rhel".into());
    assert_eq!(kinds(fedora.clone()), ["dnf repository"]);
    assert_eq!(kinds(facts(Os::Macos, "brew")), ["brew tap"]);
    assert_eq!(
        kinds(facts(Os::Windows, "winget")),
        ["scoop bucket", "winget source"]
    );

    // dnf: a .repo file, written as root.
    let a = plan_on(&f, &m, fedora).unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    apply(&f, a[0].as_ref());
    assert!(m.runner.lines().last().unwrap().ends_with("/gh.repo"));
    fs::write(
        m.dir.path().join("gh.repo"),
        repo::dnf_repo_file(
            "gh",
            "gh",
            "https://cli.github.com/packages/rpm",
            Some("https://cli.github.com/packages/key"),
        ),
    )
    .unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Satisfied);
}

#[test]
fn brew_taps_are_tapped_and_trusted() {
    let m = machine(&["brew"], FakeNet::default());
    let f = Fixture::new("steps:\n  - package_repo: { name: hashicorp, brew: hashicorp/tap }\n");
    m.runner.on("brew tap", 0, "homebrew/core\n");
    m.runner.on(
        "brew trust --json v1",
        0,
        r#"{"taps":[],"formulae":[],"casks":[],"commands":[]}"#,
    );
    let a = plan_on(&f, &m, facts(Os::Macos, "brew")).unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    assert_eq!(
        a[0].describe(),
        "brew tap hashicorp/tap (tap; brew trust --tap)"
    );
    assert!(!a[0].needs_sudo());
    m.runner.clear_calls();
    apply(&f, a[0].as_ref());
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        [
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_SUDO=1 \
             brew tap hashicorp/tap",
            "NONINTERACTIVE=1 HOMEBREW_NO_ENV_HINTS=1 HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_SUDO=1 \
             brew trust --tap hashicorp/tap",
        ]
    );
    m.runner.on("brew tap", 0, "homebrew/core\nhashicorp/tap\n");
    m.runner
        .on("brew trust --json v1", 0, r#"{"taps":["hashicorp/tap"]}"#);
    assert_eq!(a[0].status().unwrap(), Status::Satisfied);

    // A Homebrew without `brew trust`: tapping is enough.
    m.runner
        .on("brew trust", 1, "Error: Unknown command: trust");
    assert_eq!(a[0].status().unwrap(), Status::Satisfied);
    // Not installed yet: unknown.
    m.runner.remove_program("brew");
    assert_eq!(a[0].status().unwrap(), Status::Unknown);
}

#[test]
fn zypper_scoop_and_winget_argv() {
    let m = machine(
        &["zypper", "rpm", "sudo", "scoop", "winget"],
        FakeNet::default(),
    );
    let f = Fixture::new(
        "steps:\n  - package_repo:\n      name: gh\n      zypper: { url: https://cli.github.com/packages/rpm, gpgkey: https://k }\n      scoop: extras\n      winget: { source: corp, url: https://w.example.com, type: Microsoft.Rest }\n",
    );
    let mut suse = facts(Os::Linux, "zypper");
    suse.family = Some("suse".into());
    m.runner.on(
        "zypper --non-interactive repos --uri",
        0,
        "# | Alias | Name | Enabled | GPG Check | Refresh | URI\n--+--\n1 | gh | gh | Yes | Yes | Yes | https://old.example/rpm\n",
    );
    let a = plan_on(&f, &m, suse).unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    assert!(a[0].describe().contains("replaces https://old.example/rpm"));
    m.runner.clear_calls();
    apply(&f, a[0].as_ref());
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        [
            "sudo -n rpm --import https://k",
            "sudo -n zypper --non-interactive removerepo gh",
            "sudo -n zypper --non-interactive addrepo --refresh https://cli.github.com/packages/rpm gh",
        ]
    );

    m.runner.on(
        "scoop bucket list",
        0,
        "Name Source Updated Manifests\n---- ------ ------- ---------\nmain https://x 2026 1400\n",
    );
    m.runner.on(
        "winget source list",
        0,
        "Name    Argument\n----------------\nwinget  https://cdn.winget.microsoft.com/cache\n",
    );
    let a = plan_on(&f, &m, facts(Os::Windows, "winget")).unwrap();
    assert_eq!(
        a.iter().map(|a| a.status().unwrap()).collect::<Vec<_>>(),
        [Status::Pending, Status::Pending]
    );
    // The listing uses only arguments `winget source list` accepts.
    let lists: Vec<String> = m
        .runner
        .lines_of(Mode::Output)
        .into_iter()
        .filter(|l| l.contains(" list"))
        .collect();
    assert!(
        lists.contains(&"winget source list --disable-interactivity".to_string()),
        "{lists:?}"
    );
    assert!(
        lists
            .iter()
            .all(|l| !l.contains("accept-source-agreements")),
        "{lists:?}"
    );
    // A scoop bucket needs nothing; a winget source an elevated terminal (checked up front).
    assert!(!a[0].needs_sudo());
    assert!(a[1].needs_sudo());
    assert!(
        a[1].describe().contains("needs an elevated terminal"),
        "{}",
        a[1].describe()
    );
    m.runner.set_root(true);
    assert!(!a[1].needs_sudo(), "already elevated");
    assert!(!a[1].describe().contains("elevated terminal"));
    m.runner.set_root(false);
    m.runner.clear_calls();
    apply(&f, a[0].as_ref());
    m.runner
        .on("winget source add", 1, "Administrator required");
    let e = f.apply_err(a[1].as_ref());
    assert!(e.contains("elevated (Administrator) terminal"), "{e}");
    assert_eq!(
        m.runner.lines_of(Mode::Stream),
        [
            "scoop bucket add extras",
            "winget source add --name corp --arg https://w.example.com --type Microsoft.Rest \
             --accept-source-agreements --disable-interactivity"
        ]
    );
}

#[test]
fn an_asc_keyring_stays_armored_and_a_list_file_is_one_line() {
    let mut net = FakeNet::default();
    net.keys.insert(
        "https://download.docker.com/linux/ubuntu/gpg".into(),
        ARMORED.as_bytes().to_vec(),
    );
    net.suites.insert(
        "https://download.docker.com/linux/ubuntu/dists/questing/Release".into(),
        Ok(true),
    );
    let m = machine(&["apt-get", "dpkg-query", "sudo", "gpg"], net);
    m.runner.on(
        "gpg --batch --no-tty --homedir",
        0,
        &format!("pub:-:4096:1:8D81803C0EBFCD88:1:::-:::scESA:\nfpr:::::::::{FPR}:\n"),
    );
    let d = m.dir.path().display().to_string();
    let f = Fixture::new(&format!(
        "steps:\n  - package_repo:\n      name: docker\n      apt:\n        url: https://download.docker.com/linux/ubuntu\n        key: https://download.docker.com/linux/ubuntu/gpg\n        fingerprint: {FPR}\n        suites: \"{{{{ codename }}}}\"\n        components: [stable]\n        keyring: {d}/keyrings/docker.asc\n        file: {d}/sources.list.d/docker.list\n"
    ));
    let a = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(apply(&f, a[0].as_ref()), Outcome::Changed);
    let calls = m.runner.lines();
    assert!(
        !calls.iter().any(|c| c.contains("--dearmor")),
        "an .asc keyring is installed armored: {calls:#?}"
    );
    assert!(
        calls
            .last()
            .unwrap()
            .ends_with(&format!("/key.download {d}/keyrings/docker.asc")),
        "{calls:#?}"
    );
    // The source is written in the one-line format a `.list` file must have.
    m.runner.clear_calls();
    assert_eq!(apply(&f, a[1].as_ref()), Outcome::Changed);
    let install = m.runner.lines().last().unwrap().clone();
    assert!(
        install.ends_with(&format!(" {d}/sources.list.d/docker.list")),
        "{install}"
    );
    // The temporary copy is gone: what satisfies the step is the one-line rendering.
    let d822 = repo::Deb822 {
        name: "docker".into(),
        url: "https://download.docker.com/linux/ubuntu".into(),
        suites: vec!["questing".into()],
        components: vec!["stable".into()],
        architectures: vec!["amd64".into()],
        signed_by: Some(format!("{d}/keyrings/docker.asc")),
    };
    fs::create_dir_all(m.dir.path().join("sources.list.d")).unwrap();
    fs::write(
        m.dir.path().join("sources.list.d/docker.list"),
        d822.render_list(),
    )
    .unwrap();
    assert_eq!(a[1].status().unwrap(), Status::Satisfied);

    // A binary key cannot go into an `.asc` keyring.
    let mut net = FakeNet::default();
    net.keys.insert(
        "https://download.docker.com/linux/ubuntu/gpg".into(),
        vec![0x99, 0x02, 0x0d],
    );
    let m2 = machine(&["apt-get", "dpkg-query", "sudo", "gpg"], net);
    let a = plan_on(&f, &m2, facts(Os::Linux, "apt")).unwrap();
    let e = f.apply_err(a[0].as_ref());
    assert!(e.contains("name the keyring `.gpg` instead"), "{e}");
}

#[test]
fn a_vendor_list_for_the_same_repository_is_a_conflict_unless_replaced() {
    let m = machine(&["apt-get", "dpkg-query", "sudo"], FakeNet::default());
    let dir = m.dir.path().join("sources.list.d");
    fs::create_dir_all(&dir).unwrap();
    // What Docker's own instructions leave behind.
    fs::write(
        dir.join("docker.list"),
        "deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu noble stable\n",
    )
    .unwrap();
    let yaml = |extra: &str| {
        format!(
            "steps:\n  - package_repo:\n      name: docker\n      apt:\n        url: https://download.docker.com/linux/ubuntu/\n        suites: noble\n        components: [stable]\n        file: {}/docker.sources\n{extra}",
            dir.display()
        )
    };
    let f = Fixture::new(&yaml(""));
    let a = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Conflict);
    let d = a[0].describe();
    assert!(
        d.contains(&format!(
            "conflict: {}/docker.list also lists https://download.docker.com/linux/ubuntu/; list it under `replaces:`",
            dir.display()
        )),
        "{d}"
    );
    let e = f.apply_err(a[0].as_ref());
    assert!(e.contains("under `replaces:`"), "{e}");
    assert!(m.runner.lines().is_empty(), "nothing written");

    // With `replaces:`, the old file goes first, then the new one is written.
    let f = Fixture::new(&yaml(&format!(
        "        replaces: [{}/docker.list]\n",
        dir.display()
    )));
    let a = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    assert!(
        a[0].describe()
            .contains(&format!("removes {}/docker.list", dir.display())),
        "{}",
        a[0].describe()
    );
    apply(&f, a[0].as_ref());
    let calls = m.runner.lines_of(Mode::Stream);
    assert_eq!(
        calls[0],
        format!("sudo -n rm -f {}/docker.list", dir.display())
    );
    assert!(
        calls[1].ends_with(&format!(" {}/docker.sources", dir.display())),
        "{calls:?}"
    );

    // `replaces` only names other sources files next to the managed one.
    let f = Fixture::new(&yaml("        replaces: [/etc/apt/sources.list]\n"));
    let e = format!(
        "{:#}",
        plan_on(&f, &m, facts(Os::Linux, "apt")).err().unwrap()
    );
    assert!(e.contains("may only name other"), "{e}");
}

#[test]
fn a_repository_file_already_there_is_saved_before_dot_writes_over_it() {
    let m = machine(&["dnf", "rpm", "sudo"], FakeNet::default());
    let f = Fixture::new(&format!(
        "steps:\n  - package_repo:\n      name: gh\n      dnf: {{ baseurl: https://cli.github.com/packages/rpm, file: {}/gh.repo }}\n",
        m.dir.path().display()
    ));
    let path = m.dir.path().join("gh.repo");
    fs::write(&path, "[gh]\nbaseurl=https://mine.example.com\n").unwrap();
    let mut fedora = facts(Os::Linux, "dnf");
    fedora.family = Some("rhel".into());
    let a = plan_on(&f, &m, fedora).unwrap();
    assert_eq!(a[0].status().unwrap(), Status::Pending);
    let store = dot_core::StateStore::at(m.dir.path().join("dot-state"));
    let ctx =
        RunCtx::new(&f.sink(), dot_core::StepId { bundle: 0, step: 0 }).with_state(store.clone());
    assert_eq!(a[0].apply(&ctx).unwrap(), Outcome::Changed);
    let backups = ctx.take_backups();
    let [dot_core::state::Restore::RootBackup { path: saved, mode }] = backups.as_slice() else {
        panic!("{backups:?}");
    };
    assert_eq!(
        fs::read_to_string(saved).unwrap(),
        "[gh]\nbaseurl=https://mine.example.com\n"
    );
    assert_eq!(*mode & 0o777, 0o644);
    assert!(m.runner.lines().last().unwrap().ends_with("/gh.repo"));
}

#[test]
fn the_apt_architecture_is_dpkgs_unless_the_arch_is_simulated() {
    let m = machine(
        &["apt-get", "dpkg-query", "dpkg", "sudo", "gpg"],
        FakeNet::default(),
    );
    // An amd64 userland on an arm64 kernel: dpkg knows, the binary's target does not.
    m.runner.on("dpkg --print-architecture", 0, "armhf\n");
    let f = docker(&m, "");
    fs::create_dir_all(m.dir.path().join("sources")).unwrap();
    let with_arch = |arch: &str| {
        fs::write(
            m.dir.path().join("sources/docker.sources"),
            sources(&m, "noble").replace("Architectures: amd64", &format!("Architectures: {arch}")),
        )
        .unwrap();
    };
    let source_status = |facts: Facts| plan_on(&f, &m, facts).unwrap()[1].status().unwrap();
    let mut native = facts(Os::Linux, "apt");
    native.arch = std::env::consts::ARCH;
    with_arch("armhf");
    assert_eq!(source_status(native.clone()), Status::Satisfied);
    // `DOT_FACTS=arch=...` names another CPU than this binary's: the simulated one wins.
    let mut simulated = facts(Os::Linux, "apt");
    simulated.arch = if std::env::consts::ARCH == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    };
    assert_eq!(source_status(simulated.clone()), Status::Pending);
    with_arch(if simulated.arch == "x86_64" {
        "amd64"
    } else {
        "arm64"
    });
    assert_eq!(source_status(simulated), Status::Satisfied);
    // No dpkg: the fact's Debian name.
    m.runner.remove_program("dpkg");
    assert_eq!(source_status(native), Status::Pending);
}

#[test]
fn a_keyring_with_two_pinned_primary_keys_is_installed() {
    // cli.github.com's keyring holds two primary keys while GitHub rotates: both pinned.
    let mut net = FakeNet::default();
    net.keys.insert(
        "https://download.docker.com/linux/ubuntu/gpg".into(),
        vec![0x99, 0x01],
    );
    let m = machine(&["apt-get", "dpkg-query", "sudo", "gpg"], net);
    m.runner.on(
        "gpg --batch --no-tty --homedir",
        0,
        concat!(
            "pub:-:4096:1:5612B36462313325:1:::-:::scESA:\n",
            "fpr:::::::::7F38BBB59D064DBCB3D84D725612B36462313325:\n",
            "pub:-:4096:1:23F3D4EA75716059:1:::-:::scESA:\n",
            "fpr:::::::::2C6106201985B60E6C7AC87323F3D4EA75716059:\n",
        ),
    );
    let f = docker(
        &m,
        "        fingerprint:\n          - 7F38BBB59D064DBCB3D84D725612B36462313325\n          - 2C6106201985B60E6C7AC87323F3D4EA75716059\n",
    );
    let actions = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert_eq!(apply(&f, actions[0].as_ref()), Outcome::Changed);
    // With one of them pinned only, the other is not trusted.
    let f = docker(
        &m,
        "        fingerprint: 7F38BBB59D064DBCB3D84D725612B36462313325\n",
    );
    let actions = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    let e = format!("{:#}", f.apply_one(actions[0].as_ref()).unwrap_err());
    assert!(e.contains("holds 2 keys"), "{e}");
}

#[test]
fn a_sysroot_takes_the_repository_files_and_no_sudo() {
    let mut net = FakeNet::default();
    net.keys
        .insert("https://example.com/key.gpg".into(), vec![0x99, 0x01]);
    let mut m = machine(&["apt-get", "dpkg-query", "sudo", "install"], net);
    let root = m.dir.path().join("sysroot");
    m.builtins.sysroot = Some(root.clone());
    let f = Fixture::new(concat!(
        "steps:\n  - package_repo:\n      name: tool\n      apt:\n",
        "        url: https://example.com/apt\n        key: https://example.com/key.gpg\n",
        "        suites: stable\n",
    ));
    let actions = plan_on(&f, &m, facts(Os::Linux, "apt")).unwrap();
    assert!(actions.iter().all(|a| !a.needs_sudo()));
    let keyring = root.join("etc/apt/keyrings/tool.gpg");
    let sources = root.join("etc/apt/sources.list.d/tool.sources");
    assert!(
        actions[0]
            .describe()
            .contains(&keyring.display().to_string())
    );
    f.apply(&actions);
    let calls = m.runner.lines();
    assert!(calls.iter().all(|c| !c.starts_with("sudo")), "{calls:#?}");
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("install -m 0644 ")
                && c.ends_with(&sources.display().to_string())),
        "{calls:#?}"
    );
}
