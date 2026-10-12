//! `examples/basic`, the repository the README's quickstart points at, must keep working: it
//! passes `dot check` on every OS, plans for each of its profiles (simulated with `DOT_FACTS`),
//! and its secret-free and secret-using bundles apply in a temp home. Nothing touches the real
//! home, and no package manager is on PATH (package steps plan as unknown).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

const EXAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/basic");

struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "config", "state", "logs", "bin"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        Sandbox { _tmp: tmp, root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn dot(&self, args: &[&str], facts: Option<&str>) -> Output {
        self.cmd(args, facts).output().unwrap()
    }

    fn cmd(&self, args: &[&str], facts: Option<&str>) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dot"));
        c.args(args)
            .arg("--repo")
            .arg(EXAMPLE)
            .current_dir(self.home())
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("DOT_STATE_DIR", self.root.join("state"))
            // No package manager, `defaults` or other tool of the machine running the tests.
            .env("PATH", self.root.join("bin"))
            .env("LC_ALL", "C")
            .env("git_name", "Me")
            .env("git_email", "me@example.com")
            .env_remove("DOT_REPO")
            .env_remove("DOT_PROFILE")
            .env_remove("DOT_FACTS")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN");
        if let Some(f) = facts {
            c.env("DOT_FACTS", f);
        }
        c
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn the_example_passes_dot_check() {
    let s = Sandbox::new();
    let out = s.dot(&["check"], None);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("dot.yml, secrets.yml and 6 bundles: ok"),
        "{}",
        text(&out)
    );
}

#[cfg(unix)]
const MACOS: &str = "os=macos,arch=aarch64,pkg=brew,init=launchd,role=desktop";
#[cfg(unix)]
const UBUNTU: &str = "os=linux,arch=x86_64,family=debian,distro=ubuntu,version=24.04,codename=noble,pkg=apt,init=systemd,role=server";

#[cfg(unix)]
#[test]
fn every_profile_of_the_example_plans() {
    for (facts, profile, bundles) in [
        (MACOS, "macos", &["shell", "git", "cli-tools", "macos"][..]),
        (UBUNTU, "ubuntu", &["shell", "git", "cli-tools"][..]),
    ] {
        let s = Sandbox::new();
        // `-v`: the package actions are listed even when this machine already has them.
        let out = s.dot(&["plan", "-v"], Some(facts));
        let t = text(&out);
        assert!(out.status.success(), "{profile}: {t}");
        assert!(t.contains(&format!("profile: {profile}")), "{profile}: {t}");
        for b in bundles {
            assert!(t.contains(&format!("selected by profile {profile}")), "{t}");
            assert!(t.contains(b), "{profile}: {b} missing: {t}");
        }
        assert!(
            t.contains("secrets needed (resolved when applying): example_api_token"),
            "{t}"
        );
        // Package names mapped per manager.
        let fd = if profile == "ubuntu" { "fd-find" } else { "fd" };
        // `install fd, ripgrep`, or `install fd (already installed: ripgrep)` where ripgrep is
        // on this machine already.
        assert!(
            t.contains(&format!("install {fd}")) && t.contains("ripgrep"),
            "{t}"
        );
    }
}

#[cfg(unix)]
#[test]
fn the_examples_links_template_and_secret_apply_in_a_temp_home() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    let s = Sandbox::new();
    // secrets.yml's envfile: ~/.config/dot/secrets.env, owner-only, in the temp home.
    let secrets = s.home().join(".config/dot/secrets.env");
    fs::create_dir_all(secrets.parent().unwrap()).unwrap();
    fs::write(&secrets, "EXAMPLE_API_TOKEN=example-token-000\n").unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o600)).unwrap();

    let logs = s.root.join("logs");
    let out = s.dot(
        &[
            "apply",
            "--yes",
            "--no-deps",
            "--log-dir",
            logs.to_str().unwrap(),
            "shell",
            "api-client",
        ],
        Some(UBUNTU),
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        !t.contains("example-token-000"),
        "the secret was printed: {t}"
    );

    let home = s.home();
    let inputrc = fs::read_link(home.join(".inputrc")).unwrap();
    assert!(
        inputrc.ends_with(Path::new("bundles/shell/inputrc")),
        "{}",
        inputrc.display()
    );
    let bashrc = fs::read_to_string(home.join(".bashrc")).unwrap();
    assert!(
        bashrc.contains(". ~/.config/example-shell/aliases.sh"),
        "{bashrc}"
    );

    let config = home.join(".config/example-api/config.env");
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "# Rendered by dot from bundles/api-client/config.env.j2.\n\
         EXAMPLE_API_URL=https://api.example.com\n\
         EXAMPLE_API_TOKEN=example-token-000\n"
    );
    let mode = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    // Nothing has drifted since.
    let out = s.dot(
        &["status", "--no-deps", "shell", "api-client"],
        Some(UBUNTU),
    );
    assert!(out.status.success(), "{}", text(&out));
}

#[cfg(unix)]
#[test]
fn bundles_that_do_not_use_the_git_variables_run_without_them() {
    let s = Sandbox::new();
    let out = s
        .cmd(&["apply", "--dry-run", "shell"], Some(UBUNTU))
        .env_remove("git_name")
        .env_remove("git_email")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));

    // The git bundle needs them: without a terminal, what to do instead.
    let out = s
        .cmd(&["apply", "--dry-run", "git"], Some(UBUNTU))
        .env_remove("git_name")
        .env_remove("git_email")
        .output()
        .unwrap();
    let t = text(&out);
    assert_eq!(out.status.code(), Some(2), "{t}");
    assert!(
        t.contains(
            "there is no terminal to ask for git_name, git_email here: give them with --var"
        ),
        "{t}"
    );
}

#[test]
fn a_missing_optional_secret_is_not_a_problem() {
    let s = Sandbox::new();
    // The example has no Windows profile: name one, so this runs on every OS.
    let out = s
        .cmd(&["secrets", "check"], None)
        .env("DOT_PROFILE", "ubuntu")
        .output()
        .unwrap();
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        t.contains("not set (optional: the steps using them are skipped): EXAMPLE_API_TOKEN"),
        "{t}"
    );
    assert!(t.contains("secrets check: ok"), "{t}");
}

#[cfg(unix)]
#[test]
fn a_short_simulated_ubuntu_plans_with_apt() {
    let s = Sandbox::new();
    let out = s.dot(&["plan"], Some("os=linux,distro=ubuntu"));
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("profile: ubuntu"), "{t}");
    assert!(t.contains("apt: install fd-find, ripgrep"), "{t}");
    assert!(!t.contains("`pkg` fact is unknown"), "{t}");
}
