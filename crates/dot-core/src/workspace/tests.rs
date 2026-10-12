use std::fs;

use serde_json::json;

use super::*;
use crate::facts::Os;

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Spelled as on disk but without Windows' verbatim `\\?\` prefix (what users see).
        let root = crate::paths::simplify(&tmp.path().canonicalize().unwrap());
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("repo")).unwrap();
        Self { _tmp: tmp, root }
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn inputs(&self) -> Inputs {
        Inputs {
            home: self.root.join("home"),
            cwd: self.root.join("repo"),
            ..Inputs::default()
        }
    }
}

fn linux(role: Option<Role>) -> Facts {
    Facts::new(Os::Linux, "x86_64", role.unwrap_or(Role::Server))
}

const DOT_YML: &str = r#"
vars:
  v_repo: repo
  v_profile: repo
  v_bundle: repo
  v_dotenv: repo
  v_env: repo
  v_cli: repo
profiles:
  server:
    match: { os: linux, role: server }
    bundles: [app]
    vars: { v_profile: profile, v_bundle: profile }
  desktop:
    match: { role: desktop }
"#;

#[test]
fn every_layer_in_order_with_sources() {
    let fx = Fx::new();
    fx.write("repo/dot.yml", DOT_YML);
    fx.write(
        "repo/bundles/app/bundle.yml",
        "vars: { v_bundle: bundle, v_dotenv: bundle }\nsteps: []\n",
    );
    fx.write(
        "repo/.env",
        "v_dotenv=dotenv\nv_dotenv_profile=dotenv\nv_local=dotenv\n",
    );
    fx.write("repo/.env.server", "v_dotenv_profile=dotenv.server\n");
    fx.write("repo/.env.local", "v_local=.env.local\nv_user=.env.local\n");
    fx.write(
        "home/.config/dot/local.env",
        "v_user=local.env\nDOT_ROLE=server\n",
    );
    fx.write("extra.env", "v_file=file\n");
    let mut inputs = fx.inputs();
    inputs.env = BTreeMap::from([
        ("v_env".to_string(), "environment".to_string()),
        ("v_cli".to_string(), "environment".to_string()),
    ]);
    inputs.vars = vec![("v_cli".into(), "cli".into())];
    inputs.env_files = vec![fx.root.join("extra.env")];
    let ws = Workspace::open(&inputs, linux).unwrap();
    assert_eq!(ws.profile.as_ref().unwrap().0, "server");
    let app = ws.repo.bundles["app"].clone();
    let vars = ws.vars.with_bundle(&app);
    let got = |n: &str| {
        let v = vars.get(n).unwrap_or_else(|| panic!("{n} missing"));
        (v.value.clone(), v.source.name.clone())
    };
    assert_eq!(got("v_repo"), (json!("repo"), "dot.yml".into()));
    assert_eq!(
        got("v_profile"),
        (json!("profile"), "profile server".into())
    );
    assert_eq!(got("v_bundle"), (json!("bundle"), "bundle app".into()));
    assert_eq!(got("v_dotenv"), (json!("dotenv"), ".env".into()));
    assert_eq!(
        got("v_dotenv_profile"),
        (json!("dotenv.server"), ".env.server".into())
    );
    assert_eq!(got("v_local"), (json!(".env.local"), ".env.local".into()));
    // Named as the docs write it, on every OS.
    assert_eq!(
        got("v_user"),
        (json!("local.env"), "~/.config/dot/local.env".into())
    );
    assert_eq!(got("v_env"), (json!("environment"), "environment".into()));
    assert_eq!(got("v_cli"), (json!("cli"), "--var".into()));
    assert!(got("v_file").1.starts_with("--env-file"));
    assert_eq!(got("profile"), (json!("server"), "fact".into()));
    assert!(
        vars.get("DOT_ROLE").is_none(),
        "control keys are not variables"
    );
    // A Linux machine's folders are Linux paths (`<home>/.config`), whatever this host is.
    let config_dir = format!("{}/.config", fx.root.join("home").display());
    assert_eq!(got("config_dir").0, json!(config_dir));
}

#[test]
fn role_override_and_profile_pin_come_from_local_env() {
    let fx = Fx::new();
    fx.write("repo/dot.yml", DOT_YML);
    fx.write("repo/bundles/app/bundle.yml", "steps: []\n");
    fx.write("home/.config/dot/local.env", "DOT_ROLE=desktop\n");
    let ws = Workspace::open(&fx.inputs(), linux).unwrap();
    assert_eq!(ws.facts.role, Role::Desktop);
    assert_eq!(ws.profile_name(), Some("desktop"));

    fx.write(
        "home/.config/dot/local.env",
        "DOT_ROLE=desktop\nDOT_PROFILE=server\n",
    );
    let ws = Workspace::open(&fx.inputs(), linux).unwrap();
    let (name, how) = ws.profile.clone().unwrap();
    assert_eq!(name, "server");
    assert!(how.starts_with("pinned in"), "{how}");

    // --profile beats the pin.
    let mut inputs = fx.inputs();
    inputs.profile = Some("desktop".into());
    assert_eq!(
        Workspace::open(&inputs, linux).unwrap().profile_name(),
        Some("desktop")
    );

    fx.write("home/.config/dot/local.env", "DOT_ROLE=laptop\n");
    let e = Workspace::open(&fx.inputs(), linux)
        .unwrap_err()
        .to_string();
    assert!(e.contains("unknown role `laptop`"), "{e}");
}

#[test]
fn repo_found_through_local_env() {
    let fx = Fx::new();
    fx.write("repo/dot.yml", "");
    let mut inputs = fx.inputs();
    inputs.cwd = fx.root.join("home");
    let e = Workspace::open(&inputs, linux).unwrap_err().to_string();
    assert!(e.contains("no dotfiles repository found"), "{e}");
    // Quoted, as install.sh and install.ps1 write it: dotenv reads `\` in an unquoted value
    // as an escape, and a Windows path is full of them.
    fx.write(
        "home/.config/dot/local.env",
        &format!("DOT_REPO='{}'\n", fx.root.join("repo").display()),
    );
    let ws = Workspace::open(&inputs, linux).unwrap();
    assert_eq!(ws.located.how, "DOT_REPO in local.env");
    // XDG_CONFIG_HOME moves the local config.
    inputs.env.insert(
        "XDG_CONFIG_HOME".into(),
        fx.root.join("elsewhere").display().to_string(),
    );
    assert!(Workspace::open(&inputs, linux).is_err());
}

#[test]
fn selection_reasons_and_missing_required_vars() {
    let fx = Fx::new();
    fx.write(
        "repo/dot.yml",
        "vars:\n  email: { required: true }\n  port: { type: int }\nprofiles:\n  server:\n    match: {}\n    bundles: [app]\n",
    );
    fx.write(
        "repo/bundles/common/bundle.yml",
        "implicit: true\nsteps: []\n",
    );
    fx.write(
        "repo/bundles/app/bundle.yml",
        "requires: [lib]\nvars:\n  token: { required: true }\nsteps: []\n",
    );
    fx.write("repo/bundles/lib/bundle.yml", "steps: []\n");
    fx.write(
        "repo/bundles/unused/bundle.yml",
        "vars:\n  never: { required: true }\nsteps: []\n",
    );
    let mut inputs = fx.inputs();
    inputs.vars = vec![("port".into(), "x".into())];
    let ws = Workspace::open(&inputs, linux).unwrap();
    let res = ws.resolve(&[], false).unwrap();
    assert_eq!(res.names(), ["common", "lib", "app"]);
    assert_eq!(
        res.get("app").unwrap().reasons,
        [Reason::Selected("profile server".into())]
    );
    assert_eq!(res.get("common").unwrap().reasons, [Reason::Implicit]);
    let e = ws.check_vars(&res).unwrap_err();
    assert!(
        e.contains("missing required variables: email, token"),
        "{e}"
    );
    assert!(!e.contains("never"), "only bundles in the run count: {e}");
    assert!(e.contains("expected int, got `x`"), "{e}");

    let res = ws.resolve(&["lib".into()], true).unwrap();
    assert_eq!(res.names(), ["lib"]);
    assert_eq!(res.get("lib").unwrap().reasons, [Reason::Requested]);
}

#[test]
fn sensitive_values_are_collected_for_redaction() {
    let fx = Fx::new();
    fx.write(
        "repo/dot.yml",
        "vars:\n  pw: { sensitive: true, default: hunter2 }\n  plain: x\n",
    );
    fx.write("repo/.env", "GITLAB_TOKEN=glpat-123\n");
    let ws = Workspace::open(&fx.inputs(), linux).unwrap();
    let res = ws.resolve(&[], false).unwrap();
    assert_eq!(ws.sensitive_values(&res), ["glpat-123", "hunter2"]);
}

#[test]
fn a_repo_var_may_use_a_var_only_the_bundle_defines() {
    let fx = Fx::new();
    fx.write(
        "repo/dot.yml",
        "vars: { url: \"http://localhost:{{ port }}\" }\nbundles: [b]\n",
    );
    fx.write(
        "repo/bundles/b/bundle.yml",
        "vars: { port: 8080 }\nsteps: []\n",
    );
    let ws = Workspace::open(&fx.inputs(), linux).unwrap();
    let res = ws.resolve(&[], false).unwrap();
    ws.check_vars(&res).unwrap();
    let b = ws.repo.bundles.get("b").unwrap();
    assert_eq!(
        ws.vars.with_bundle(b).render("{{ url }}").unwrap(),
        "http://localhost:8080"
    );
}

#[test]
fn env_example_keys_are_required_and_secret_ones_sensitive() {
    let fx = Fx::new();
    fx.write("repo/dot.yml", "");
    fx.write("repo/.env.example", "EMAIL=\nVPN_PSK=  # secret\n");
    fx.write("repo/.env.local", "VPN_PSK=abc123xyz\n");
    let ws = Workspace::open(&fx.inputs(), linux).unwrap();
    let res = ws.resolve(&[], false).unwrap();
    let e = ws.check_vars(&res).unwrap_err();
    assert!(e.contains("missing required variables: EMAIL "), "{e}");
    assert!(!e.contains("VPN_PSK"), "{e}");
    assert_eq!(ws.sensitive_values(&res), ["abc123xyz"]);
}

#[test]
fn check_env_files_reports_syntax_and_declaration_problems() {
    let fx = Fx::new();
    fx.write(
        "repo/dot.yml",
        "vars:\n  port: { type: int }\n  db_password: { type: int }\nprofiles:\n  mac: { match: { os: macos } }\n",
    );
    fx.write("repo/.env", "A=1\nBAD LINE\n");
    fx.write("repo/.env.mac", "port=abc\ndb_password=hunter2-SENSITIVE\n");
    fx.write("repo/.env.example", "X=\n");
    let root = fx.root.join("repo");
    let repo = Repo::load(&root).unwrap();
    let errs: Vec<String> = check_env_files(&root, &repo, &linux(None), &fx.root.join("home"))
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        errs.iter()
            .any(|e| e.contains(".env:2:") && e.contains("invalid dotenv syntax")),
        "{errs:#?}"
    );
    assert!(
        errs.iter()
            .any(|e| e.contains("`port` (from .env.mac): expected int, got `abc`")),
        "{errs:#?}"
    );
    assert!(
        errs.iter()
            .any(|e| e.contains("`db_password`") && e.contains("`***`")),
        "{errs:#?}"
    );
    assert!(!errs.iter().any(|e| e.contains("SENSITIVE")), "{errs:#?}");
    assert_eq!(errs.len(), 3, "{errs:#?}");
}

#[test]
fn prompted_variables_are_listed_answered_and_validated() {
    let fx = Fx::new();
    fx.write(
        "repo/dot.yml",
        concat!(
            "vars:\n",
            "  email: { prompt: \"Your e-mail\", persist: local, pattern: '.+@.+' }\n",
            "  gpg_pin: { prompt: \"GPG PIN\", type: int }\n",
            "  have: { prompt: \"Already set\", default: yes }\n",
            "profiles:\n  server:\n    match: {}\n    bundles: [app]\n",
        ),
    );
    fx.write(
        "repo/bundles/app/bundle.yml",
        "vars:\n  api_token: { prompt: \"API token\", required: true }\nsteps: []\n",
    );
    fx.write(
        "repo/bundles/unused/bundle.yml",
        "vars:\n  never: { prompt: \"never asked\" }\nsteps: []\n",
    );
    fx.write("home/.config/dot/local.env", "gpg_pin=\n");
    let mut ws = Workspace::open(&fx.inputs(), linux).unwrap();
    let res = ws.resolve(&[], false).unwrap();
    let prompts = ws.var_prompts(&res);
    let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    // In declaration order: dot.yml's, then the bundle's.
    assert_eq!(names, ["email", "gpg_pin", "api_token"]);
    assert_eq!(
        prompts[2],
        VarPrompt {
            name: "api_token".into(),
            question: "API token".into(),
            sensitive: true,
            persist: false,
        }
    );
    assert!(prompts[0].persist && !prompts[0].sensitive);

    // Non-interactive: every missing name in one message.
    let e = ws
        .check_vars_including(&res, &["email".into(), "gpg_pin".into()])
        .unwrap_err();
    assert!(
        e.contains("missing required variables: api_token, email, gpg_pin"),
        "{e}"
    );

    assert!(
        ws.answer_problems(&res, "email", "me@example.com")
            .is_empty()
    );
    let p = ws.answer_problems(&res, "email", "nope");
    assert!(p[0].contains("does not match pattern"), "{p:?}");
    let p = ws.answer_problems(&res, "gpg_pin", "12x");
    assert!(p[0].contains("expected int"), "{p:?}");

    ws.answer(&[
        ("email".into(), "me@example.com".into()),
        ("gpg_pin".into(), "1234".into()),
        ("api_token".into(), "tok".into()),
    ]);
    assert!(ws.var_prompts(&res).is_empty());
    ws.check_vars(&res).unwrap();
    assert_eq!(ws.vars.get("gpg_pin").unwrap().value, json!(1234));
    assert_eq!(
        ws.vars.get("email").unwrap().source.to_string(),
        "answered at prompt"
    );
}

#[test]
fn known_folders_follow_the_os_and_environment() {
    let fx = Fx::new();
    let mut inputs = fx.inputs();
    let home = inputs.home.display().to_string();
    let value = |vars: &Vars, n: &str| vars.get(n).map(|v| v.value.clone());
    // Windows looks variables up case-insensitively.
    inputs.env = [
        (
            "AppData".to_string(),
            r"C:\Users\me\AppData\Roaming".to_string(),
        ),
        ("PROGRAMFILES".to_string(), r"D:\Apps".to_string()),
    ]
    .into();
    let win = Facts::new(Os::Windows, "x86_64", Role::Desktop);
    let vars = inputs.base_vars(&win);
    assert_eq!(value(&vars, "xdg_config_home"), Some(json!(null)));
    let rendered = vars
        .render("{{ config_dir }}/Code/User/settings.json")
        .unwrap();
    if cfg!(windows) {
        assert_eq!(
            value(&vars, "config_dir"),
            Some(json!(r"C:\Users\me\AppData\Roaming"))
        );
        assert_eq!(value(&vars, "programfiles"), Some(json!(r"D:\Apps")));
        assert_eq!(
            crate::folders::normalize(Os::Windows, &rendered),
            r"C:\Users\me\AppData\Roaming\Code\User\settings.json"
        );
    } else {
        // Windows simulated on this host (`DOT_FACTS=os=windows`): laid out under home, as
        // real paths here; the host's variables do not describe Windows.
        assert_eq!(
            value(&vars, "config_dir"),
            Some(json!(format!("{home}/AppData/Roaming")))
        );
        assert_eq!(
            value(&vars, "programfiles"),
            Some(json!(format!("{home}/Program Files")))
        );
        assert_eq!(
            rendered,
            format!("{home}/AppData/Roaming/Code/User/settings.json")
        );
    }

    inputs.env = [("XDG_DATA_HOME".to_string(), "/data".to_string())].into();
    let vars = inputs.base_vars(&linux(None));
    assert_eq!(value(&vars, "data_dir"), Some(json!("/data")));
    assert_eq!(value(&vars, "xdg_data_home"), Some(json!("/data")));
    assert_eq!(
        value(&vars, "cache_dir"),
        Some(json!(format!("{home}/.cache")))
    );
    assert_eq!(value(&vars, "appdata"), Some(json!(null)));
    assert_eq!(value(&vars, "windows_home"), Some(json!(null)));

    let mut wsl = linux(None);
    wsl.env = vec!["wsl".into()];
    wsl.windows_home = Some("/mnt/c/Users/me".into());
    let vars = inputs.base_vars(&wsl);
    assert_eq!(
        vars.render("{{ windows_home }}/AppData").unwrap(),
        "/mnt/c/Users/me/AppData"
    );
}

/// What install.sh / install.ps1 append to `local.env`: single-quoted, so a Windows path's
/// backslashes stay literal.
#[test]
fn installer_written_dot_repo_is_read_literally() {
    let fx = Fx::new();
    fx.write(
        "home/.config/dot/local.env",
        "DOT_REPO='C:\\Users\\me\\.dot'\n",
    );
    let control = fx.inputs().local_control().unwrap();
    assert_eq!(control["DOT_REPO"], r"C:\Users\me\.dot");
}

#[test]
fn prompts_come_in_declaration_order() {
    let fx = Fx::new();
    fx.write(
        "repo/dot.yml",
        "vars:\n  zulu: { prompt: \"Z\" }\n  alpha: { prompt: \"A\" }\n",
    );
    fx.write(
        "repo/bundles/app/bundle.yml",
        "vars:\n  name: { prompt: \"Name\" }\n  email: { prompt: \"Email\" }\nsteps: []\n",
    );
    let ws = Workspace::open(&fx.inputs(), linux).unwrap();
    let res = ws.resolve(&[], false).unwrap();
    let names: Vec<String> = ws.var_prompts(&res).into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["zulu", "alpha", "name", "email"]);
}
