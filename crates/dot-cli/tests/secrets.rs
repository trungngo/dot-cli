//! Profiles, bundles, variables and secrets end to end: a temp repository with two profiles, four bundles ordered by `requires`,
//! `after` and a capability, `.env` + `.env.local` variables, and `secrets.yml` mapped to an
//! `envfile` in a temp `$HOME` (`~/.config/dot/secrets.env`). A shell step's template uses
//! `secret()`; the value must reach the step and never the console or the log file. Nothing
//! touches the real home or `~/.config/dot`; git (for the guardrail) sees no global config.
//! Unix-only: the bundles run `sh` steps and the guardrail checks file modes.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

const TOKEN: &str = "tok-5f3a9c-very-secret";
const KEY: &str = "key-77b1e0-also-secret";

const DOT_YML: &str = r#"
vars:
  greeting: hello
profiles:
  desk:
    match: { role: desktop }
    bundles: [tools, app]
  srv:
    match: { role: server }
    bundles: [app]
    vars: { greeting: "hi from {{ profile }}" }
"#;

const SECRETS_YML: &str = r#"
default_provider: envfile
secrets:
  api_token: { ref: "envfile:API_TOKEN" }
  deploy_key: { ref: DEPLOY_KEY }
"#;

struct Env {
    _tmp: TempDir,
    root: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "state", "logs", "repo"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        let e = Self { _tmp: tmp, root };
        e.write("repo/dot.yml", DOT_YML);
        e.write("repo/secrets.yml", SECRETS_YML);
        e.write("repo/.env", "api_host=api.example.com\nwhere=committed\n");
        e.write("repo/.env.local", "where=local\n");
        e.write(
            "repo/.env.example",
            "API_TOKEN=   # secret\nDEPLOY_KEY=  # secret: deploy key\n",
        );
        e.write(
            "repo/bundles/base/bundle.yml",
            "implicit: true\nsteps:\n  - shell: echo base >> ~/order\n",
        );
        e.write(
            "repo/bundles/vim/bundle.yml",
            "provides: [editor]\nafter: [base]\nsteps:\n  - shell: echo vim >> ~/order\n",
        );
        e.write(
            "repo/bundles/app/bundle.yml",
            concat!(
                "requires: [\"cap:editor\"]\n",
                "after: [base]\n",
                "steps:\n",
                "  - shell: |\n",
                "      echo \"app {{ greeting }} {{ api_host }} {{ where }} token={{ secret('api_token') }}\" >> ~/order\n",
                "      echo \"calling {{ api_host }} with {{ secret('api_token') }}\"\n",
                "    label: Call the API\n",
            ),
        );
        e.write(
            "repo/bundles/tools/bundle.yml",
            concat!(
                "after: [app]\n",
                "when: { role: desktop }\n",
                "steps:\n",
                "  - shell: echo \"tools {{ secret('deploy_key') }}\" >> ~/order\n",
            ),
        );
        e
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn secrets_env(&self) -> PathBuf {
        self.home().join(".config/dot/secrets.env")
    }

    /// Writes `~/.config/dot/secrets.env` with `mode`.
    fn secrets(&self, text: &str, mode: u32) {
        let p = self.secrets_env();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn dot(&self, role: &str, args: &[&str]) -> Output {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dot"));
        c.args(args)
            .current_dir(self.root.join("repo"))
            .env("HOME", self.home())
            .env_remove("DOT_STATE_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("DOT_ROLE", role)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .env_remove("DOT_REPO")
            .env_remove("DOT_PROFILE")
            .env_remove("API_TOKEN")
            .env_remove("DEPLOY_KEY");
        c.output().unwrap()
    }

    fn apply(&self, role: &str, extra: &[&str]) -> Output {
        let logs = self.root.join("logs");
        let mut args = vec!["apply", "--yes", "--log-dir", logs.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.dot(role, &args)
    }

    fn order(&self) -> Vec<String> {
        fs::read_to_string(self.home().join("order"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn logs(&self) -> Vec<String> {
        let mut files: Vec<PathBuf> = fs::read_dir(self.root.join("logs"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        files
            .iter()
            .map(|f| fs::read_to_string(f).unwrap())
            .collect()
    }
}

fn both(o: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn assert_no_secret(text: &str) {
    assert!(!text.contains(TOKEN), "token leaked:\n{text}");
    assert!(!text.contains(KEY), "key leaked:\n{text}");
}

const GOOD: &str = "API_TOKEN=tok-5f3a9c-very-secret\nDEPLOY_KEY=key-77b1e0-also-secret\n";

#[test]
fn apply_resolves_secrets_in_order_and_redacts_the_log() {
    let e = Env::new();
    e.secrets(GOOD, 0o600);
    let out = e.apply("desktop", &["--plain", "-vv"]);
    assert!(out.status.success(), "{}", both(&out));

    // requires cap:editor pulls vim in before app; `after` puts base first and tools last.
    let order = e.order();
    assert_eq!(
        order,
        [
            "base".to_string(),
            "vim".to_string(),
            format!("app hello api.example.com local token={TOKEN}"),
            format!("tools {KEY}"),
        ],
        "{}",
        both(&out)
    );

    let console = both(&out);
    assert_no_secret(&console);
    assert!(
        console.contains("calling api.example.com with ***"),
        "{console}"
    );

    let logs = e.logs();
    assert_eq!(logs.len(), 1);
    let log = &logs[0];
    assert_no_secret(log);
    assert!(log.contains("calling api.example.com with ***"), "{log}");
    assert!(
        log.contains("secrets needed: api_token, deploy_key"),
        "{log}"
    );
}

#[test]
fn the_json_renderer_is_redacted_too() {
    let e = Env::new();
    e.secrets(GOOD, 0o600);
    let out = e.apply("desktop", &["--json"]);
    assert!(out.status.success(), "{}", both(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_no_secret(&stdout);
    let events: Vec<serde_json::Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|err| panic!("{err}: {l}")))
        .collect();
    assert_eq!(events.first().unwrap()["event"], "run_started");
    let last = events.last().unwrap();
    assert_eq!(last["event"], "run_finished");
    assert_eq!(last["success"], true);
}

#[test]
fn the_other_profile_selects_other_bundles() {
    let e = Env::new();
    e.secrets(GOOD, 0o600);
    let out = e.apply("server", &["--plain"]);
    assert!(out.status.success(), "{}", both(&out));
    assert_eq!(
        e.order(),
        [
            "base".to_string(),
            "vim".to_string(),
            format!("app hi from srv api.example.com local token={TOKEN}"),
        ]
    );
    assert_no_secret(&both(&out));
}

#[test]
fn a_group_readable_secrets_file_is_refused_before_anything_runs() {
    let e = Env::new();
    e.secrets(GOOD, 0o644);
    let out = e.apply("desktop", &["--plain"]);
    assert!(!out.status.success(), "{}", both(&out));
    let text = both(&out);
    assert!(text.contains("chmod 600"), "{text}");
    assert!(text.contains("0644") || text.contains("644"), "{text}");
    assert!(e.order().is_empty(), "no step ran: {:?}", e.order());
    assert_no_secret(&text);
    assert_no_secret(&e.logs()[0]);

    let check = e.dot("desktop", &["secrets", "check"]);
    assert!(!check.status.success());
    assert!(both(&check).contains("chmod 600"), "{}", both(&check));
}

/// Regression: one file secret others can read (an `.ovpn` copied with umask 022) used to
/// fail the envfile provider's whole batch, so every secret counted as unresolved and the whole
/// run was skipped with "0 failed". Now only the bundle that uses that file fails.
#[test]
fn a_group_readable_file_secret_fails_only_its_bundle() {
    let e = Env::new();
    e.secrets(GOOD, 0o600);
    e.write(
        "repo/dot.yml",
        &DOT_YML.replace("bundles: [tools, app]", "bundles: [tools, app, vpn]"),
    );
    e.write(
        "repo/secrets.yml",
        &format!(
            "{SECRETS_YML}  vpn_profile: {{ ref: \"envfile-file:vpn/p.ovpn\", kind: file }}\n"
        ),
    );
    e.write(
        "repo/bundles/vpn/bundle.yml",
        concat!(
            "after: [base]\n",
            "steps:\n",
            "  - label: VPN profile\n",
            "    secret_file:\n",
            "      ~/vpn/p.ovpn: { secret: vpn_profile }\n",
            "  - shell: echo vpn >> ~/order\n",
        ),
    );
    let ovpn = e.home().join(".config/dot/files/vpn/p.ovpn");
    fs::create_dir_all(ovpn.parent().unwrap()).unwrap();
    fs::write(&ovpn, "client\n").unwrap();
    fs::set_permissions(&ovpn, fs::Permissions::from_mode(0o644)).unwrap();

    let out = e.apply("desktop", &["--plain"]);
    let text = both(&out);
    assert!(!out.status.success(), "{text}");
    // Every other bundle ran, the ones using secrets from secrets.env included.
    let order = e.order();
    assert!(order.iter().any(|l| l == "base"), "{text}");
    assert!(order.iter().any(|l| l == "vim"), "{text}");
    assert!(order.iter().any(|l| l.starts_with("app ")), "{text}");
    assert!(order.iter().any(|l| l.starts_with("tools ")), "{text}");
    assert!(!order.iter().any(|l| l == "vpn"), "{text}");
    assert!(!e.home().join("vpn/p.ovpn").exists());
    // Only the loose file is reported, and its bundle's steps count as failed.
    assert!(text.contains("1 secret could not be resolved"), "{text}");
    assert!(
        text.contains("vpn_profile: `envfile-file:vpn/p.ovpn`"),
        "{text}"
    );
    assert!(text.contains("chmod 600"), "{text}");
    assert!(!text.contains("api_token:"), "{text}");
    assert!(text.contains(" 1 failed"), "{text}");
    assert_no_secret(&text);

    // `secrets check` names the file too.
    let check = e.dot("desktop", &["secrets", "check"]);
    let c = both(&check);
    assert!(!check.status.success(), "{c}");
    assert!(c.contains("p.ovpn") && c.contains("chmod 600"), "{c}");

    // Fixed, the same run applies.
    fs::set_permissions(&ovpn, fs::Permissions::from_mode(0o600)).unwrap();
    let out = e.apply("desktop", &["--plain"]);
    assert!(out.status.success(), "{}", both(&out));
    assert!(e.order().iter().any(|l| l == "vpn"));
}

#[test]
fn plan_never_reads_secrets() {
    let e = Env::new();
    // Unreadable by the guardrail: planning must not care.
    e.secrets(GOOD, 0o644);
    let out = e.dot("desktop", &["plan"]);
    assert!(out.status.success(), "{}", both(&out));
    let text = both(&out);
    assert!(
        text.contains("secrets needed (resolved when applying): api_token, deploy_key"),
        "{text}"
    );
    assert_no_secret(&text);
    let pos = |name: &str| {
        text.find(&format!(". {name}"))
            .unwrap_or_else(|| panic!("{name} missing: {text}"))
    };
    assert!(pos("base") < pos("vim") && pos("vim") < pos("app") && pos("app") < pos("tools"));

    // An undeclared secret is a plan error, still without touching a provider.
    e.write(
        "repo/bundles/vim/bundle.yml",
        "provides: [editor]\nsteps:\n  - shell: echo {{ secret('nope') }}\n",
    );
    let out = e.dot("desktop", &["plan"]);
    assert!(!out.status.success());
    let vim = e.root.join("repo/bundles/vim/bundle.yml");
    assert!(
        both(&out).contains(&format!(
            "secret `nope` is not declared in secrets.yml (used at {}:3)",
            vim.display()
        )),
        "{}",
        both(&out)
    );
}

#[test]
fn check_reports_undeclared_secrets_where_they_are_used() {
    let e = Env::new();
    e.write(
        "repo/bundles/vim/bundle.yml",
        concat!(
            "provides: [editor]\n",
            "steps:\n",
            "  # - shell: echo {{ secret('commented_out') }}\n",
            "  - secret_file: { ~/.k: undeclared_secret }\n",
            "  - template: { ~/.conf: conf.j2 }\n",
        ),
    );
    e.write(
        "repo/bundles/vim/conf.j2",
        "a={{ secret('api_token') }}\n{# {{ secret('in_comment') }} #}\nb={{ secret(\"nope_secret\") }}\n",
    );
    let out = e.dot("desktop", &["check"]);
    let text = both(&out);
    assert!(!out.status.success(), "{text}");
    let bundle = e.root.join("repo/bundles/vim/bundle.yml");
    let conf = e.root.join("repo/bundles/vim/conf.j2");
    assert!(
        text.contains(&format!(
            "{}:4:5: steps[0]: secret_file ~/.k: secret `undeclared_secret` is not declared in \
             secrets.yml",
            bundle.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "{}:3:6: secret(\"nope_secret\"): secret `nope_secret` is not declared",
            conf.display()
        )),
        "{text}"
    );
    for fine in ["api_token", "commented_out", "in_comment"] {
        assert!(!text.contains(&format!("`{fine}`")), "{fine}: {text}");
    }
    assert!(text.contains("2 errors"), "{text}");

    // The apply error says where, too.
    e.secrets(GOOD, 0o600);
    let out = e.apply("desktop", &["--plain"]);
    let text = both(&out);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains(&format!("`nope_secret` is used at {}:3", conf.display())),
        "{text}"
    );
}

#[test]
fn non_interactive_fails_only_the_bundles_whose_secrets_are_missing() {
    let e = Env::new();
    e.secrets("# nothing here\nAPI_TOKEN=\n", 0o600);
    let out = e.apply("desktop", &["--plain"]);
    assert!(!out.status.success(), "{}", both(&out));
    let text = both(&out);
    assert!(text.contains("1 secret could not be resolved"), "{text}");
    assert!(
        text.contains("api_token: `envfile:API_TOKEN` not found"),
        "{text}"
    );
    assert!(
        text.contains("deploy_key: `envfile:DEPLOY_KEY` not found"),
        "{text}"
    );
    assert!(text.contains("non-interactively"), "{text}");
    // The bundles that need no secret ran; `app` and `tools` failed.
    assert_eq!(e.order(), ["base", "vim"], "{text}");

    // An optional secret with no value: the steps that use it are skipped, the bundle is not
    // failed.
    e.write(
        "repo/secrets.yml",
        &SECRETS_YML
            .replace(
                r#"{ ref: "envfile:API_TOKEN" }"#,
                r#"{ ref: "envfile:API_TOKEN", optional: true }"#,
            )
            .replace("{ ref: DEPLOY_KEY }", "{ ref: DEPLOY_KEY, optional: true }"),
    );
    let out = e.apply("desktop", &["--plain", "-v"]);
    let text = both(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("optional secret `api_token` has no value"),
        "{text}"
    );
}

#[test]
fn secrets_check_reports_names_never_values() {
    let e = Env::new();
    e.secrets(
        "API_TOKEN=tok-5f3a9c-very-secret\nSTALE=old-value-123\n",
        0o600,
    );
    let out = e.dot("desktop", &["secrets", "check"]);
    let text = both(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("missing keys: DEPLOY_KEY"), "{text}");
    assert!(text.contains("unused keys: STALE"), "{text}");
    assert!(!text.contains("old-value-123"), "{text}");
    assert_no_secret(&text);

    e.secrets(GOOD, 0o600);
    let out = e.dot("desktop", &["secrets", "check"]);
    assert!(out.status.success(), "{}", both(&out));
    assert!(both(&out).contains("secrets check: ok"), "{}", both(&out));
    assert_no_secret(&both(&out));

    // `dot check` validates secrets.yml without any provider.
    e.write("repo/secrets.yml", "secrets:\n  x: { reff: X }\n");
    let out = e.dot("desktop", &["check"]);
    assert!(!out.status.success());
    let text = both(&out);
    assert!(text.contains("secrets.yml:2"), "{text}");
}

#[test]
fn secret_values_never_reach_a_command_line_and_logs_are_private() {
    let e = Env::new();
    e.secrets(GOOD, 0o600);
    e.write(
        "repo/bundles/vim/bundle.yml",
        "provides: [editor]\nsteps:\n  - shell: ps -o args= -p $$ > ~/argv; test \"{{ secret('api_token') }}\" = \"$(cat ~/.config/dot/secrets.env | sed -n 's/^API_TOKEN=//p')\" && echo same >> ~/argv\n",
    );
    let out = e.apply("server", &["--plain"]);
    assert!(out.status.success(), "{}", both(&out));
    let argv = fs::read_to_string(e.home().join("argv")).unwrap();
    assert!(!argv.contains(TOKEN), "secret in argv: {argv}");
    assert!(argv.contains("DOT_SECRET_api_token"), "{argv}");
    assert!(
        argv.contains("same"),
        "the value reached the command: {argv}"
    );

    let dir = e.root.join("logs");
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    for f in fs::read_dir(&dir).unwrap() {
        assert_eq!(mode(&f.unwrap().path()), 0o600);
    }
}

#[test]
fn committed_secret_values_fail_check_and_apply() {
    let e = Env::new();
    e.secrets(GOOD, 0o600);
    let repo = e.root.join("repo");
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", &e.root)
            .output()
    };
    match git(&["init", "-q"]) {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("git not available; skipping");
            return;
        }
    }
    e.write(
        "repo/.env",
        "api_host=api.example.com\nAPI_TOKEN=committed-value-xyz\n",
    );
    assert!(git(&["add", ".env"]).unwrap().status.success());

    let out = e.dot("server", &["check"]);
    assert!(!out.status.success(), "{}", both(&out));
    let text = both(&out);
    assert!(
        text.contains(".env:2: `API_TOKEN` is a secret but has a committed value"),
        "{text}"
    );
    assert!(!text.contains("committed-value-xyz"), "{text}");

    let out = e.apply("server", &["--plain"]);
    assert_eq!(out.status.code(), Some(2), "{}", both(&out));
    assert!(
        both(&out).contains("`API_TOKEN` is a secret"),
        "{}",
        both(&out)
    );
    assert!(e.order().is_empty(), "nothing ran");
}

#[test]
fn only_steps_with_work_need_their_secrets_and_status_compares_secret_content() {
    let e = Env::new();
    e.write(
        "repo/dot.yml",
        "profiles:\n  desk:\n    match: { role: desktop }\n    bundles: [conf, login]\n",
    );
    e.write(
        "repo/bundles/conf/bundle.yml",
        concat!(
            "steps:\n",
            "  - secret_file: { ~/sf.txt: { secret: api_token } }\n",
            "  - template: { ~/tpl.txt: t.j2 }\n",
        ),
    );
    e.write("repo/bundles/conf/t.j2", "x={{ secret('api_token') }}\n");
    // Logged in already (`unless` succeeds): the token is not needed.
    e.write(
        "repo/bundles/login/bundle.yml",
        concat!(
            "steps:\n",
            "  - shell:\n",
            "      cmd: echo login {{ secret('deploy_key') }} >> ~/order\n",
            "      unless: test -f ~/logged-in\n",
        ),
    );
    fs::write(e.home().join("logged-in"), "").unwrap();
    e.secrets(&format!("API_TOKEN={TOKEN}\n"), 0o600);
    let out = e.apply("desktop", &["--plain"]);
    let text = both(&out);
    assert!(out.status.success(), "deploy_key is not needed: {text}");
    assert_no_secret(&text);
    let again = e.apply("desktop", &["--plain"]);
    assert!(
        both(&again).contains("1 changed, 3 ok"),
        "only base runs again: {}",
        both(&again)
    );

    // A converged machine is in sync: the secret content is compared with what can be
    // resolved without a prompt.
    let st = e.dot("desktop", &["status"]);
    let text = both(&st);
    assert!(st.status.success(), "{text}");
    assert!(text.contains("status: in sync"), "{text}");
    assert_no_secret(&text);
    // Edited by hand: drift, still without showing the value.
    fs::write(e.home().join("tpl.txt"), "x=other\n").unwrap();
    let st = e.dot("desktop", &["status"]);
    let text = both(&st);
    assert!(!st.status.success(), "{text}");
    assert!(text.contains("~/tpl.txt"), "{text}");
    assert_no_secret(&text);
    // Without the value (and nobody to ask): not checked, not drift.
    fs::write(e.home().join("tpl.txt"), format!("x={TOKEN}\n")).unwrap();
    e.secrets("# nothing\n", 0o600);
    let st = e.dot("desktop", &["status"]);
    let text = both(&st);
    assert!(st.status.success(), "{text}");
    assert!(text.contains("not checked"), "{text}");
    assert!(text.contains("in sync (2 steps not checked)"), "{text}");
}
