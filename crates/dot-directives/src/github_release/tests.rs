//! `github_release` against an in-memory GitHub: no network, and everything written goes to the
//! fixture's temp home.

use std::collections::HashMap;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use dot_core::event::Recorder;
use dot_core::facts::{Os, Role};
use dot_core::{Event, EventSink, Facts, Options, SecretCache, SecretRefs, engine};
use dot_platform::http::{HttpClient, Request, Response};
use sha2::{Digest, Sha256};

use super::archive::tests::{tar_gz, zip_bytes};
use super::*;
use crate::Builtins;
use crate::testutil::Fixture;

const API: &str = "https://api.test";
const DL: &str = "https://dl.test";

type Route = (u16, Vec<(String, String)>, Vec<u8>);

/// An in-memory HTTP client: canned responses by URL, every request recorded.
#[derive(Default)]
struct FakeHttp {
    routes: Mutex<HashMap<String, Route>>,
    log: Mutex<Vec<Request>>,
    /// Requests without an `Authorization` header to the API get a rate-limit error.
    limit_anonymous: Mutex<bool>,
    offline: Mutex<bool>,
}

impl FakeHttp {
    fn route(&self, url: &str, status: u16, body: impl Into<Vec<u8>>) {
        self.routes
            .lock()
            .unwrap()
            .insert(url.to_string(), (status, Vec::new(), body.into()));
    }

    fn requests(&self) -> Vec<Request> {
        self.log.lock().unwrap().clone()
    }

    fn urls(&self) -> Vec<String> {
        self.requests().into_iter().map(|r| r.url).collect()
    }
}

impl HttpClient for FakeHttp {
    fn get(&self, req: &Request) -> anyhow::Result<Response> {
        self.log.lock().unwrap().push(req.clone());
        if *self.offline.lock().unwrap() {
            anyhow::bail!("GET {}: connection refused", req.url);
        }
        if *self.limit_anonymous.lock().unwrap()
            && req.url.starts_with(API)
            && req.get_header("authorization").is_none()
        {
            return Ok(Response {
                status: 403,
                headers: vec![("x-ratelimit-remaining".into(), "0".into())],
                content_length: None,
                body: Box::new(Cursor::new(
                    b"{\"message\":\"API rate limit exceeded\"}".to_vec(),
                )),
            });
        }
        let (status, headers, body) = self
            .routes
            .lock()
            .unwrap()
            .get(&req.url)
            .cloned()
            .unwrap_or((404, Vec::new(), b"{\"message\":\"Not Found\"}".to_vec()));
        Ok(Response {
            status,
            headers,
            content_length: Some(body.len() as u64),
            body: Box::new(Cursor::new(body)),
        })
    }
}

fn sha256(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A fake GitHub holding releases of `o/tool`.
struct Hub {
    http: Arc<FakeHttp>,
}

impl Hub {
    fn new() -> Self {
        Self {
            http: Arc::new(FakeHttp::default()),
        }
    }

    /// Publishes `tag` with `assets` (name, bytes) as the latest release; `digests` adds
    /// GitHub's `digest` field.
    fn publish(&self, tag: &str, assets: &[(&str, Vec<u8>)], digests: bool) {
        let list: Vec<serde_json::Value> = assets
            .iter()
            .map(|(name, bytes)| {
                let url = format!("{DL}/{tag}/{name}");
                self.http.route(&url, 200, bytes.clone());
                let mut v = serde_json::json!({
                    "name": name,
                    "browser_download_url": url,
                    "size": bytes.len(),
                });
                if digests {
                    v["digest"] = format!("sha256:{}", sha256(bytes)).into();
                }
                v
            })
            .collect();
        let release = serde_json::json!({ "tag_name": tag, "assets": list });
        self.http.route(
            &format!("{API}/repos/o/tool/releases/latest"),
            200,
            release.to_string(),
        );
    }

    /// A fresh planner (empty release cache) talking to this hub.
    fn builtins(&self, f: &Fixture) -> Builtins {
        let mut gh = Github::new(self.http.clone(), API);
        gh.state_dir = Some(f.home().join("state"));
        Builtins {
            github: gh,
            ..Builtins::default()
        }
    }
}

/// Publishes a normal release: a linux and a macOS tarball plus `checksums.txt`.
fn standard(hub: &Hub, tag: &str, content: &str) -> Vec<u8> {
    let linux = tar_gz(&[
        ("tool/README.md", b"docs"),
        ("tool/tool", content.as_bytes()),
    ]);
    let mac = tar_gz(&[("tool/tool", b"mac")]);
    let sums = format!(
        "{}  tool-{tag}-linux-amd64.tar.gz\n{}  tool-{tag}-darwin-arm64.tar.gz\n",
        sha256(&linux),
        sha256(&mac)
    );
    hub.publish(
        tag,
        &[
            (&format!("tool-{tag}-darwin-arm64.tar.gz"), mac),
            (&format!("tool-{tag}-linux-amd64.tar.gz"), linux.clone()),
            ("checksums.txt", sums.into_bytes()),
        ],
        false,
    );
    linux
}

fn plan_with(
    f: &Fixture,
    b: &Builtins,
    ctx: &dot_core::Context,
) -> anyhow::Result<Vec<Box<dyn Action>>> {
    let step = &f.bundle.steps[0];
    let secrets = SecretRefs::default();
    let pctx = PlanCtx::new(ctx, &f.bundle, step, &secrets);
    dot_core::Planner::plan(b, &step.directive, &pctx)
}

fn plan(f: &Fixture, b: &Builtins) -> Vec<Box<dyn Action>> {
    plan_with(f, b, &f.context()).unwrap_or_else(|e| panic!("{e:#}"))
}

fn bin(f: &Fixture, name: &str) -> std::path::PathBuf {
    f.home().join(".local/bin").join(name)
}

fn outputs(f: &Fixture) -> Vec<String> {
    f.recorder
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Output { line, .. } => Some(line),
            _ => None,
        })
        .collect()
}

#[test]
fn installs_the_latest_release_and_is_then_satisfied() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    let linux = standard(&hub, "v1.0.0", "tool 1.0");
    let b = hub.builtins(&f);
    let actions = plan(&f, &b);
    assert_eq!(
        actions[0].describe(),
        "install tool v1.0.0 from o/tool -> ~/.local/bin (tool-v1.0.0-linux-amd64.tar.gz)"
    );
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert_eq!(f.apply(&actions), [Outcome::Changed]);

    let installed = bin(&f, "tool");
    assert_eq!(std::fs::read_to_string(&installed).unwrap(), "tool 1.0");
    let mode = std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755);
    // Nothing else is left in the install directory.
    let names: Vec<String> = std::fs::read_dir(f.home().join(".local/bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["tool"]);
    assert!(
        outputs(&f)
            .iter()
            .any(|l| l.contains("sha256 verified (checksums.txt)")),
        "{:?}",
        outputs(&f)
    );
    let progress: Vec<(u64, Option<u64>)> = f
        .recorder
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Progress {
                done,
                total,
                unit: Unit::Bytes,
                ..
            } => Some((done, total)),
            _ => None,
        })
        .collect();
    let size = linux.len() as u64;
    assert_eq!(progress.first(), Some(&(0, Some(size))));
    assert_eq!(progress.last(), Some(&(size, Some(size))));

    // The same release is satisfied, from the state file alone (the lookup is cached).
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    let again = plan(&f, &b);
    assert_eq!(again[0].status().unwrap(), Status::Satisfied);
    let api_calls = hub
        .http
        .urls()
        .iter()
        .filter(|u| u.starts_with(API))
        .count();
    assert_eq!(api_calls, 1, "{:?}", hub.http.urls());
    let state = StateFile::in_dir(&f.home().join("state"))
        .get("o/tool", &f.home().join(".local/bin"))
        .unwrap();
    assert_eq!(state.tag, "v1.0.0");
    assert_eq!(state.bins, ["tool"]);

    // A deleted executable is noticed.
    std::fs::remove_file(&installed).unwrap();
    assert_eq!(again[0].status().unwrap(), Status::Pending);
    assert!(again[0].describe().contains("(missing from ~/.local/bin)"));
}

#[test]
fn a_tag_pins_the_release_and_sha256_the_asset() {
    let hub = Hub::new();
    let old = standard(&hub, "v1.0.0", "old");
    let latest = format!("{API}/repos/o/tool/releases/latest");
    let body = hub.http.routes.lock().unwrap()[&latest].2.clone();
    hub.http.route(
        &format!("{API}/repos/o/tool/releases/tags/v1.0.0"),
        200,
        body,
    );
    standard(&hub, "v1.1.0", "new");

    let yaml = |sha: &str| {
        format!("steps:\n  - github_release: {{ repo: o/tool, tag: v1.0.0, sha256: \"{sha}\" }}\n")
    };
    let f = Fixture::new(&yaml(&sha256(&old).to_uppercase()));
    let b = hub.builtins(&f);
    let actions = plan(&f, &b);
    assert_eq!(
        actions[0].describe(),
        "install tool v1.0.0 from o/tool -> ~/.local/bin (tool-v1.0.0-linux-amd64.tar.gz)"
    );
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert_eq!(std::fs::read_to_string(bin(&f, "tool")).unwrap(), "old");
    assert!(
        outputs(&f)
            .iter()
            .any(|l| l.contains("sha256 verified (the step's sha256)")),
        "{:?}",
        outputs(&f)
    );
    // A newer latest release changes nothing.
    assert_eq!(plan(&f, &b)[0].status().unwrap(), Status::Satisfied);

    // The pinned digest wins over the release's own checksums.
    let f = Fixture::new(&yaml(&"0".repeat(64)));
    let actions = plan(&f, &hub.builtins(&f));
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("does not match its sha256"), "{e}");
    assert!(!bin(&f, "tool").exists());

    // A tag the repository does not have.
    let f = Fixture::new("steps:\n  - github_release: { repo: o/tool, tag: v9 }\n");
    let actions = plan(&f, &hub.builtins(&f));
    let d = actions[0].describe();
    assert!(
        d.contains("release v9 unknown: o/tool has no release tagged v9"),
        "{d}"
    );
    assert_eq!(actions[0].status().unwrap(), Status::Unknown);
}

#[test]
fn a_newer_release_is_an_update() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    standard(&hub, "v1.0.0", "old");
    f.apply(&plan(&f, &hub.builtins(&f)));
    standard(&hub, "v1.1.0", "new");
    let b = hub.builtins(&f);
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert_eq!(
        actions[0].describe(),
        "update tool v1.0.0 -> v1.1.0 from o/tool in ~/.local/bin"
    );
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert_eq!(std::fs::read_to_string(bin(&f, "tool")).unwrap(), "new");
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
}

#[test]
fn checksum_mismatch_installs_nothing() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    let good = tar_gz(&[("tool", b"real")]);
    let evil = tar_gz(&[("tool", b"evil")]);
    hub.publish(
        "v1",
        &[
            ("tool-linux-x86_64.tar.gz", evil),
            (
                "tool-linux-x86_64.tar.gz.sha256",
                format!("{}\n", sha256(&good)).into_bytes(),
            ),
        ],
        false,
    );
    let actions = plan(&f, &hub.builtins(&f));
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("sha256 mismatch"), "{e}");
    assert!(!bin(&f, "tool").exists());
    assert!(!f.home().join("state").exists());
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    // The temporary download directory is gone too.
    let left: Vec<_> = std::fs::read_dir(f.home().join(".local/bin"))
        .unwrap()
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn github_digest_is_checked() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    hub.publish("v2", &[("tool_linux_amd64", b"raw binary".to_vec())], true);
    let actions = plan(&f, &hub.builtins(&f));
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert_eq!(
        std::fs::read_to_string(bin(&f, "tool")).unwrap(),
        "raw binary"
    );
    assert!(
        outputs(&f)
            .iter()
            .any(|l| l.contains("GitHub's asset digest")),
        "{:?}",
        outputs(&f)
    );
}

#[test]
fn unverified_assets_say_so() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    hub.publish("v2", &[("tool-linux-amd64", b"bin".to_vec())], false);
    f.apply(&plan(&f, &hub.builtins(&f)));
    assert!(
        outputs(&f).iter().any(|l| l.contains("not verified")),
        "{:?}",
        outputs(&f)
    );
}

#[test]
fn asset_map_bins_zip_and_custom_directory() {
    let f = Fixture::new(
        r#"steps:
  - github_release:
      repo: o/tool
      asset:
        macos: "*-mac.zip"
        linux: "*-{{ flavor }}.zip"
      bin: [tool, helper]
      to: ~/opt/bin
"#,
    );
    let hub = Hub::new();
    let z = zip_bytes(&[("a/tool", b"t"), ("a/b/helper", b"h")]);
    hub.publish(
        "v3",
        &[
            ("tool-static.zip", z.clone()),
            ("tool-dynamic.zip", b"x".to_vec()),
        ],
        false,
    );
    let mut ctx = f.context();
    ctx.vars.set("flavor", "static");
    let b = hub.builtins(&f);
    let actions = plan_with(&f, &b, &ctx).unwrap();
    assert_eq!(
        actions[0].describe(),
        "install tool, helper v3 from o/tool -> ~/opt/bin (tool-static.zip)"
    );
    f.apply(&actions);
    assert_eq!(
        std::fs::read_to_string(f.home().join("opt/bin/tool")).unwrap(),
        "t"
    );
    assert_eq!(
        std::fs::read_to_string(f.home().join("opt/bin/helper")).unwrap(),
        "h"
    );

    // A platform without a pattern is a located config error.
    let mac = dot_core::Context::new(
        Facts::new(Os::Windows, "x86_64", Role::Desktop),
        f.home().into(),
    );
    let e = format!("{:#}", plan_with(&f, &b, &mac).err().unwrap());
    assert!(e.contains("no `asset:` pattern for windows/x86_64"), "{e}");
    assert!(e.contains("bundle.yml:2:5"), "{e}");
}

#[test]
fn windows_executables_get_exe() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    hub.publish(
        "v1",
        &[("tool-windows-x64.zip", zip_bytes(&[("tool.exe", b"win")]))],
        false,
    );
    let ctx = dot_core::Context::new(
        Facts::new(Os::Windows, "x86_64", Role::Desktop),
        f.home().into(),
    );
    let actions = plan_with(&f, &hub.builtins(&f), &ctx).unwrap();
    f.apply(&actions);
    assert_eq!(std::fs::read_to_string(bin(&f, "tool.exe")).unwrap(), "win");
}

#[test]
fn no_matching_asset_is_a_plan_error() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    hub.publish("v1", &[("tool-darwin-arm64.tar.gz", vec![1])], false);
    let e = format!(
        "{:#}",
        plan_with(&f, &hub.builtins(&f), &f.context())
            .err()
            .unwrap()
    );
    assert!(e.contains("o/tool v1: no asset for linux/x86_64"), "{e}");
    assert!(e.contains("tool-darwin-arm64.tar.gz"), "{e}");
}

#[test]
fn missing_executable_in_the_archive_fails_cleanly() {
    let f = Fixture::new("steps:\n  - github_release: { repo: o/tool, bin: [nope] }\n");
    let hub = Hub::new();
    standard(&hub, "v1", "x");
    let actions = plan(&f, &hub.builtins(&f));
    let e = f.apply_err(actions[0].as_ref());
    assert!(
        e.contains("`nope` not found in tool-v1-linux-amd64.tar.gz"),
        "{e}"
    );
    assert!(e.contains("tool/tool"), "{e}");
    assert!(!f.home().join("state").exists());
}

#[test]
fn offline_is_unknown_at_plan_time_and_fails_when_applied() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    *hub.http.offline.lock().unwrap() = true;
    let actions = plan(&f, &hub.builtins(&f));
    assert_eq!(actions[0].status().unwrap(), Status::Unknown);
    let d = actions[0].describe();
    assert!(d.contains("latest release unknown"), "{d}");
    assert!(d.contains("connection refused"), "{d}");
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("cannot look up the latest release"), "{e}");

    // A repository without releases.
    *hub.http.offline.lock().unwrap() = false;
    let actions = plan(&f, &hub.builtins(&f));
    assert!(
        actions[0]
            .describe()
            .contains("no published release found for o/tool")
    );
}

#[test]
fn rate_limits_explain_how_to_authenticate() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    standard(&hub, "v1", "x");
    *hub.http.limit_anonymous.lock().unwrap() = true;
    let actions = plan(&f, &hub.builtins(&f));
    let d = actions[0].describe();
    assert!(d.contains("rate limit exceeded"), "{d}");
    assert!(d.contains("set GITHUB_TOKEN"), "{d}");

    // A token from the environment is sent to the API, never to the download host.
    let mut b = hub.builtins(&f);
    b.github.env_token = Some(dot_core::SecretBytes::new("env-t0ken"));
    let actions = plan(&f, &b);
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    f.apply(&actions);
    for r in hub.http.requests() {
        let auth = r.get_header("authorization");
        if r.url.starts_with(DL) {
            assert_eq!(auth, None, "{r:?}");
        } else if auth.is_some() {
            assert_eq!(auth, Some("Bearer env-t0ken"));
        }
    }
    assert!(!format!("{:?}", b.github).contains("env-t0ken"));
}

#[test]
fn a_token_secret_is_used_once_resolved_and_never_shown() {
    let f = Fixture::new("steps:\n  - github_release: { repo: o/tool, token: gh }\n");
    let hub = Hub::new();
    standard(&hub, "v1", "x");
    *hub.http.limit_anonymous.lock().unwrap() = true;
    let secrets = SecretCache::default();
    secrets.insert("gh", "s3cret-token");
    let mut ctx = f.context();
    ctx.secrets = Some(Arc::new(secrets));
    let rec = Recorder::default();
    let sink = EventSink::new().with(Box::new(rec.clone()));
    let b = hub.builtins(&f);
    let summary = engine::run(
        std::slice::from_ref(&f.bundle),
        &b,
        &ctx,
        &sink,
        Options::default(),
    );
    assert!(summary.success(), "{summary:?} {:?}", rec.events());
    assert_eq!(summary.changed, 1);
    let events = rec.events();
    // Planning (no secrets yet) could not look the release up; applying could.
    let Some(Event::PlanReady(view)) = events.iter().find(|e| matches!(e, Event::PlanReady(_)))
    else {
        panic!()
    };
    assert_eq!(view.secrets, ["gh"]);
    assert!(format!("{view:?}").contains("rate limit"), "{view:?}");
    assert!(bin(&f, "tool").exists());
    assert!(
        hub.http
            .requests()
            .iter()
            .any(|r| r.get_header("authorization") == Some("Bearer s3cret-token"))
    );
    assert!(!format!("{events:?}").contains("s3cret-token"));
}

#[test]
fn the_repository_token_secret_is_used_only_without_an_environment_token() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    standard(&hub, "v1", "x");
    *hub.http.limit_anonymous.lock().unwrap() = true;
    let secrets = SecretCache::default();
    secrets.insert(TOKEN_SECRET, "repo-t0ken");
    let mut ctx = f.context();
    ctx.secrets = Some(Arc::new(secrets));
    let mut b = hub.builtins(&f);
    b.github.token_secret = Some(TOKEN_SECRET.into());

    // An environment token wins: the secret is not even asked for.
    let mut with_env = hub.builtins(&f);
    with_env.github.token_secret = Some(TOKEN_SECRET.into());
    with_env.github.env_token = Some(dot_core::SecretBytes::new("env-t0ken"));
    let plan = engine::plan(std::slice::from_ref(&f.bundle), &with_env, &ctx);
    assert!(plan.secrets.is_empty(), "{:?}", plan.secrets);

    let rec = Recorder::default();
    let sink = EventSink::new().with(Box::new(rec.clone()));
    let summary = engine::run(
        std::slice::from_ref(&f.bundle),
        &b,
        &ctx,
        &sink,
        Options::default(),
    );
    assert!(summary.success(), "{summary:?} {:?}", rec.events());
    let Some(Event::PlanReady(view)) = rec
        .events()
        .into_iter()
        .find(|e| matches!(e, Event::PlanReady(_)))
    else {
        panic!()
    };
    assert_eq!(view.secrets, [TOKEN_SECRET]);
    assert!(bin(&f, "tool").exists());
    assert!(
        hub.http
            .requests()
            .iter()
            .any(|r| r.get_header("authorization") == Some("Bearer repo-t0ken"))
    );
}

#[test]
fn dry_run_downloads_nothing() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    standard(&hub, "v1", "x");
    let summary = engine::run(
        std::slice::from_ref(&f.bundle),
        &hub.builtins(&f),
        &f.context(),
        &EventSink::new(),
        Options {
            dry_run: true,
            ..Default::default()
        },
    );
    assert_eq!((summary.changed, summary.failed), (1, 0), "{summary:?}");
    assert!(!f.home().join(".local").exists());
    assert!(!f.home().join("state").exists());
    assert!(
        hub.http.urls().iter().all(|u| u.starts_with(API)),
        "{:?}",
        hub.http.urls()
    );
}

#[test]
fn bad_rendered_values_are_located_errors() {
    let f = Fixture::new("steps:\n  - github_release: { repo: '{{ r }}' }\n");
    let hub = Hub::new();
    let mut ctx = f.context();
    ctx.vars.set("r", "not-a-repo");
    let e = format!(
        "{:#}",
        plan_with(&f, &hub.builtins(&f), &ctx).err().unwrap()
    );
    assert!(e.contains("bundle.yml:2:5"), "{e}");
    assert!(e.contains("expected `owner/name`"), "{e}");
    let f = Fixture::new("steps:\n  - github_release: { repo: o/tool, asset: '[' }\n");
    let e = format!(
        "{:#}",
        plan_with(&f, &hub.builtins(&f), &f.context())
            .err()
            .unwrap()
    );
    assert!(e.contains("asset `[`"), "{e}");
}

#[test]
fn offline_after_an_install_names_the_installed_version() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    standard(&hub, "v1.2.3", "tool");
    f.apply(&plan(&f, &hub.builtins(&f)));
    *hub.http.offline.lock().unwrap() = true;
    let actions = plan(&f, &hub.builtins(&f));
    // Kept, as `download` keeps its file: the bundle does not fail offline.
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    let d = actions[0].describe();
    assert!(
        d.starts_with(
            "tool v1.2.3 installed from o/tool in ~/.local/bin; latest release unknown ("
        ) && d.ends_with("): keeping it"),
        "{d}"
    );
    // The executable is gone: nothing to keep, so unknown (and applying fails).
    std::fs::remove_file(f.home().join(".local/bin/tool")).unwrap();
    let actions = plan(&f, &hub.builtins(&f));
    assert_eq!(actions[0].status().unwrap(), Status::Unknown);
}

#[test]
fn a_changed_asset_choice_is_applied() {
    let hub = Hub::new();
    let gnu = tar_gz(&[("tool", b"gnu")]);
    let musl = tar_gz(&[("tool", b"musl")]);
    hub.publish(
        "v1",
        &[
            ("tool-linux-amd64.tar.gz", gnu),
            ("tool-linux-amd64-musl.tar.gz", musl),
        ],
        true,
    );
    let f = Fixture::new(
        "steps:\n  - github_release: { repo: o/tool, asset: '*-linux-amd64.tar.gz' }\n",
    );
    f.apply(&plan(&f, &hub.builtins(&f)));
    assert_eq!(std::fs::read_to_string(bin(&f, "tool")).unwrap(), "gnu");

    let f2 = Fixture::new("steps:\n  - github_release: { repo: o/tool, asset: '*-musl.tar.gz' }\n");
    // Same home and state: the second fixture's config, the first one's machine.
    let mut b = hub.builtins(&f);
    b.github.state_dir = Some(f.home().join("state"));
    let mut ctx = f2.context();
    ctx.home = f.home().to_path_buf();
    let actions = plan_with(&f2, &b, &ctx).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert_eq!(
        actions[0].describe(),
        "switch tool v1 from o/tool in ~/.local/bin to another asset: tool-linux-amd64.tar.gz \
         -> tool-linux-amd64-musl.tar.gz"
    );
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert_eq!(std::fs::read_to_string(bin(&f, "tool")).unwrap(), "musl");
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
}

#[test]
fn an_executable_dot_did_not_install_is_a_conflict_unless_replaced() {
    let f = Fixture::new("steps:\n  - github_release: o/tool\n");
    let hub = Hub::new();
    standard(&hub, "v1.0.0", "downloaded");
    let mine = bin(&f, "tool");
    std::fs::create_dir_all(mine.parent().unwrap()).unwrap();
    std::fs::write(&mine, "my own precious tool").unwrap();

    let actions = plan(&f, &hub.builtins(&f));
    assert_eq!(actions[0].status().unwrap(), Status::Conflict);
    let d = actions[0].describe();
    assert!(
        d.contains(
            "conflict: ~/.local/bin/tool exists and was not installed by dot; set `replace: true`"
        ),
        "{d}"
    );
    let e = f.apply_err(actions[0].as_ref());
    assert!(e.contains("not installed by dot"), "{e}");
    assert_eq!(
        std::fs::read_to_string(&mine).unwrap(),
        "my own precious tool"
    );

    let f2 = Fixture::new("steps:\n  - github_release: { repo: o/tool, replace: true }\n");
    let mut ctx = f2.context();
    ctx.home = f.home().to_path_buf();
    let mut b = hub.builtins(&f);
    b.github.state_dir = Some(f.home().join("state"));
    let actions = plan_with(&f2, &b, &ctx).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert!(
        actions[0]
            .describe()
            .contains("replaces ~/.local/bin/tool, not installed by dot (kept as .dot-backup)"),
        "{}",
        actions[0].describe()
    );
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert_eq!(std::fs::read_to_string(&mine).unwrap(), "downloaded");
    assert_eq!(
        std::fs::read_to_string(bin(&f, "tool.dot-backup")).unwrap(),
        "my own precious tool"
    );
    // Now it is ours: no conflict, even without `replace`.
    let actions = plan(&f, &hub.builtins(&f));
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
}

#[test]
fn a_package_asset_is_installed_through_the_package_manager() {
    use dot_platform::exec::Output;
    use dot_platform::exec::fake::FakeExec;

    let f = Fixture::new("steps:\n  - github_release: { repo: o/tool, package: tool }\n");
    let hub = Hub::new();
    hub.publish(
        "v1.2.0",
        &[
            ("tool_1.2.0_arm64.deb", b"arm".to_vec()),
            ("tool_1.2.0_amd64.deb", b"deb-amd64".to_vec()),
            ("tool-1.2.0-1.x86_64.rpm", b"rpm".to_vec()),
            ("tool-1.2.0-linux-amd64.tar.gz", b"tgz".to_vec()),
        ],
        true,
    );
    let fake = FakeExec::new();
    let b = Builtins {
        system: dot_platform::exec::System::fake(fake.clone()),
        ..hub.builtins(&f)
    };
    let mut ctx = f.context();
    ctx.facts.pkg = Some("apt".into());
    fake.on("dpkg-query -W", Output::exit(1, ""));
    let actions = plan_with(&f, &b, &ctx).unwrap();
    assert_eq!(
        actions[0].describe(),
        "package tool from the latest release of o/tool (apt) (install v1.2.0 (tool_1.2.0_amd64.deb))"
    );
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert!(actions[0].needs_sudo());
    fake.clear_calls();
    f.apply(&actions);
    let install = fake.calls().pop().unwrap();
    assert!(
        install.starts_with("sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y /")
            && install.ends_with("/tool.deb"),
        "{install}"
    );
    assert!(
        outputs(&f).iter().any(|l| l.contains("sha256 verified")),
        "{:?}",
        outputs(&f)
    );
    // Installed at the release's version (by dot, or not): satisfied.
    fake.on("dpkg-query -W", Output::ok("install ok installed\t1.2.0-1"));
    let b2 = Builtins {
        system: dot_platform::exec::System::fake(fake.clone()),
        ..hub.builtins(&f)
    };
    assert_eq!(
        plan_with(&f, &b2, &ctx).unwrap()[0].status().unwrap(),
        Status::Satisfied
    );
    // Offline or rate limited: the installed package is kept, not a failed bundle.
    *hub.http.offline.lock().unwrap() = true;
    let offline = Builtins {
        system: dot_platform::exec::System::fake(fake.clone()),
        ..hub.builtins(&f)
    };
    let actions = plan_with(&f, &offline, &ctx).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert!(
        actions[0]
            .describe()
            .contains("1.2.0-1 installed; latest release unknown ("),
        "{}",
        actions[0].describe()
    );
    *hub.http.offline.lock().unwrap() = false;
    // A newer release: an update.
    hub.publish(
        "v1.3.0",
        &[("tool_1.3.0_amd64.deb", b"deb-1.3".to_vec())],
        true,
    );
    let b3 = Builtins {
        system: dot_platform::exec::System::fake(fake.clone()),
        ..hub.builtins(&f)
    };
    let actions = plan_with(&f, &b3, &ctx).unwrap();
    assert_eq!(actions[0].status().unwrap(), Status::Pending);
    assert!(actions[0].describe().contains("update 1.2.0-1 -> v1.3.0"));
    // Managed like any package: `pkg:apt:tool`, with its update as a diff.
    assert_eq!(actions[0].resources()[0].key(), "pkg:apt:tool");
    assert_eq!(
        actions[0].diff().unwrap(),
        Some(dot_core::Diff::Packages {
            manager: "apt".into(),
            missing: Vec::new(),
            outdated: vec!["tool 1.2.0-1 -> v1.3.0".into()],
        })
    );
    // dnf takes the rpm.
    ctx.facts.pkg = Some("dnf".into());
    hub.publish(
        "v1.2.0",
        &[
            ("tool-1.2.0-1.aarch64.rpm", b"a".to_vec()),
            ("tool-1.2.0-1.x86_64.rpm", b"x".to_vec()),
        ],
        true,
    );
    let b4 = Builtins {
        system: dot_platform::exec::System::fake(fake.clone()),
        ..hub.builtins(&f)
    };
    fake.on("rpm -q", Output::exit(1, "package tool is not installed"));
    let actions = plan_with(&f, &b4, &ctx).unwrap();
    assert!(
        actions[0].describe().contains("tool-1.2.0-1.x86_64.rpm"),
        "{}",
        actions[0].describe()
    );
    // Options for executables do not go with `package`.
    let file = f.root().join("bad.yml");
    std::fs::write(
        &file,
        "steps:\n  - github_release: { repo: o/tool, package: tool, bin: [x] }\n",
    )
    .unwrap();
    let e = dot_core::config::load_bundle(&file)
        .unwrap_err()
        .to_string();
    assert!(e.contains("do not apply"), "{e}");
}

#[test]
fn a_gitlab_release_is_installed_the_same_way() {
    // glab: GitLab releases only, versioned asset names, a checksums file among the links.
    let f = Fixture::new(
        "steps:\n  - github_release:\n      repo: gitlab-org/cli\n      source: gitlab\n      asset: 'glab_*_linux_amd64.tar.gz'\n      bin: glab\n",
    );
    let hub = Hub::new();
    let tarball = tar_gz(&[("bin/glab", b"glab 1.116"), ("LICENSE", b"mit")]);
    let dl = "https://gitlab.com/gitlab-org/cli/-/releases/v1.116.0/downloads";
    hub.http.route(
        &format!("{dl}/glab_1.116.0_linux_amd64.tar.gz"),
        200,
        tarball.clone(),
    );
    hub.http.route(
        &format!("{dl}/checksums.txt"),
        200,
        format!("{}  glab_1.116.0_linux_amd64.tar.gz\n", sha256(&tarball)),
    );
    let release = serde_json::json!({
        "tag_name": "v1.116.0",
        "assets": { "links": [
            { "name": "glab_1.116.0_linux_amd64.tar.gz", "url": "https://x/other",
              "direct_asset_url": format!("{dl}/glab_1.116.0_linux_amd64.tar.gz") },
            { "name": "glab_1.116.0_linux_amd64.deb", "url": format!("{dl}/glab.deb") },
            { "name": "checksums.txt", "url": format!("{dl}/checksums.txt") }
        ] }
    });
    let api = "https://gitlab.test/api/v4";
    hub.http.route(
        &format!("{api}/projects/gitlab-org%2Fcli/releases/permalink/latest"),
        200,
        release.to_string(),
    );
    let mut b = hub.builtins(&f);
    b.github.gitlab_api = Some(api.into());
    b.github.gitlab_token = Some(dot_core::SecretBytes::new("glpat-secret"));
    let actions = plan(&f, &b);
    assert_eq!(
        actions[0].describe(),
        "install glab v1.116.0 from gitlab.com/gitlab-org/cli -> ~/.local/bin \
         (glab_1.116.0_linux_amd64.tar.gz)"
    );
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert_eq!(
        std::fs::read_to_string(bin(&f, "glab")).unwrap(),
        "glab 1.116"
    );
    assert!(
        outputs(&f)
            .iter()
            .any(|l| l.contains("sha256 verified (checksums.txt)")),
        "{:?}",
        outputs(&f)
    );
    // The token went to the API only, as GitLab's header.
    for r in hub.http.requests() {
        let token = r.get_header("private-token");
        assert_eq!(token.is_some(), r.url.starts_with(api), "{}", r.url);
    }
    assert_eq!(actions[0].status().unwrap(), Status::Satisfied);
    assert_eq!(
        actions[0].resources()[0].key(),
        "release:gitlab.com/gitlab-org/cli in ~/.local/bin"
    );
}

#[test]
fn an_executable_is_installed_under_the_name_given() {
    // Godot: `Godot_v4.7.2-stable_linux.x86_64` in a zip, installed as `godot`.
    let f = Fixture::new(
        "steps:\n  - github_release:\n      repo: o/tool\n      asset: '*_linux.x86_64.zip'\n      bin: { godot: 'Godot_v*' }\n",
    );
    let hub = Hub::new();
    hub.publish(
        "4.7.2-stable",
        &[(
            "Godot_v4.7.2-stable_linux.x86_64.zip",
            zip_bytes(&[("Godot_v4.7.2-stable_linux.x86_64", b"godot")]),
        )],
        true,
    );
    let b = hub.builtins(&f);
    let actions = plan(&f, &b);
    assert_eq!(f.apply(&actions), [Outcome::Changed]);
    assert_eq!(std::fs::read_to_string(bin(&f, "godot")).unwrap(), "godot");
    assert!(!bin(&f, "Godot_v4.7.2-stable_linux.x86_64").exists());
}
