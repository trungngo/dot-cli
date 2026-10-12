//! The `lastpass` provider against a fake `lpass`, never the real one.
//!
//! This test binary is its own fake: each test writes a small `lpass` shell script into a temp
//! directory that re-runs this executable with `DOT_FAKE_LPASS_DB` set, and `main` then acts as
//! `lpass` instead of running tests. The fake keeps its vault in a JSON file, emulates the
//! subcommands the provider uses (`status`, `login`, `sync`, `show` with `--json`, `--field`,
//! `--attach` and the plain listing, `add` / `edit --non-interactive`) with the CLI's documented
//! messages and exit codes, and logs every invocation (argv, and whether anything came on stdin)
//! so tests can check batching and that values never reach the command line.
//!
//! Unix-only: the fake is launched through a `#!/bin/sh` script.

#[cfg(not(unix))]
fn main() {}

#[cfg(unix)]
fn main() {
    if std::env::var_os("DOT_FAKE_LPASS_DB").is_some() {
        std::process::exit(fake::run());
    }
    tests::run_all();
}

#[cfg(unix)]
mod fake {
    use std::io::{Read, Write};
    use std::path::PathBuf;

    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct Db {
        pub logged_in: bool,
        /// The only username `login` accepts.
        pub username: String,
        pub items: Vec<Item>,
        pub next_id: u64,
        /// `show` with several names works (else it fails, like a CLI that takes one name).
        pub multi_show: bool,
        /// `show --json` prints custom fields (older CLIs do not).
        pub json_fields: bool,
        pub syncs: u32,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct Item {
        pub id: String,
        pub group: String,
        pub name: String,
        pub username: String,
        pub password: String,
        pub url: String,
        pub note: String,
        pub fields: Vec<(String, String)>,
        /// `(attachment id, filename, bytes)`
        pub attachments: Vec<(String, String, Vec<u8>)>,
    }

    impl Item {
        pub fn fullname(&self) -> String {
            if self.group.is_empty() {
                self.name.clone()
            } else {
                format!("{}/{}", self.group, self.name)
            }
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Call {
        pub argv: Vec<String>,
        pub stdin: bool,
    }

    fn db_path() -> PathBuf {
        PathBuf::from(std::env::var_os("DOT_FAKE_LPASS_DB").unwrap())
    }

    pub fn log_path(db: &std::path::Path) -> PathBuf {
        db.with_extension("log")
    }

    fn load() -> Db {
        serde_json::from_slice(&std::fs::read(db_path()).unwrap()).unwrap()
    }

    fn save(db: &Db) {
        std::fs::write(db_path(), serde_json::to_vec_pretty(db).unwrap()).unwrap();
    }

    fn die(msg: &str) -> i32 {
        eprintln!("Error: {msg}");
        1
    }

    fn not_logged_in() -> i32 {
        die("Could not find decryption key. Perhaps you need to login with `lpass login`.")
    }

    fn matches<'a>(db: &'a Db, name: &str) -> Vec<&'a Item> {
        if let Some(i) = db.items.iter().find(|i| i.id == name) {
            return vec![i];
        }
        db.items
            .iter()
            .filter(|i| i.fullname() == name || i.name == name)
            .collect()
    }

    pub fn run() -> i32 {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let reads_stdin = matches!(argv.first().map(String::as_str), Some("add" | "edit"));
        let mut input = Vec::new();
        if reads_stdin {
            std::io::stdin().read_to_end(&mut input).unwrap();
        }
        let call = Call {
            argv: argv.clone(),
            stdin: !input.is_empty(),
        };
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path(&db_path()))
            .unwrap();
        writeln!(log, "{}", serde_json::to_string(&call).unwrap()).unwrap();

        let mut db = load();
        let (cmd, rest) = argv.split_first().expect("a subcommand");
        let (flags, names): (Vec<&String>, Vec<&String>) =
            rest.iter().partition(|a| a.starts_with('-'));
        let flag = |f: &str| flags.iter().any(|a| *a == f);
        let flag_value = |f: &str| {
            flags
                .iter()
                .find_map(|a| a.strip_prefix(&format!("{f}=")).map(str::to_string))
        };
        match cmd.as_str() {
            "status" => {
                if db.logged_in {
                    if !flag("-q") {
                        println!("Logged in as {}.", db.username);
                    }
                    0
                } else {
                    if !flag("-q") {
                        println!("Not logged in.");
                    }
                    1
                }
            }
            "login" => {
                if names.first().map(|s| s.as_str()) == Some(db.username.as_str()) {
                    db.logged_in = true;
                    save(&db);
                    println!("Success: Logged in as {}.", db.username);
                    0
                } else {
                    die("Failed to enter correct password.")
                }
            }
            "sync" => {
                if !db.logged_in {
                    return not_logged_in();
                }
                db.syncs += 1;
                save(&db);
                0
            }
            "show" => {
                if !db.logged_in {
                    return not_logged_in();
                }
                if names.len() > 1 && !db.multi_show {
                    eprintln!("Usage: lpass show [--sync=auto|now|no] ... {{UNIQUENAME|UNIQUEID}}");
                    return 1;
                }
                let mut found: Vec<&Item> = Vec::new();
                for n in &names {
                    found.extend(matches(&db, n));
                }
                if found.is_empty() {
                    return die("Could not find specified account(s).");
                }
                let mut out = std::io::stdout();
                if let Some(att) = flag_value("--attach") {
                    let [item] = found.as_slice() else {
                        return die("Multiple matches found.");
                    };
                    return match item.attachments.iter().find(|a| a.0 == att) {
                        Some(a) => {
                            out.write_all(&a.2).unwrap();
                            0
                        }
                        None => die("Could not find specified attachment."),
                    };
                }
                if let Some(field) = flag_value("--field") {
                    let [item] = found.as_slice() else {
                        return die("Multiple matches found.");
                    };
                    return match item.fields.iter().find(|f| f.0 == field) {
                        Some(f) => {
                            println!("{}", f.1);
                            0
                        }
                        None => die(&format!("Could not find specified field '{field}'.")),
                    };
                }
                if flag("--json") || flag("-j") {
                    let json: Vec<serde_json::Value> = found
                        .iter()
                        .map(|i| {
                            let mut v = serde_json::json!({
                                "id": i.id,
                                "name": i.name,
                                "fullname": i.fullname(),
                                "username": i.username,
                                "password": i.password,
                                "last_modified_gmt": "1700000000",
                                "last_touch": "0",
                                "group": i.group,
                                "url": i.url,
                                "note": i.note,
                            });
                            if db.json_fields {
                                v["fields"] = i
                                    .fields
                                    .iter()
                                    .map(|(n, val)| {
                                        serde_json::json!({"name": n, "type": "text", "value": val, "checked": false})
                                    })
                                    .collect();
                            }
                            v
                        })
                        .collect();
                    println!("{}", serde_json::to_string_pretty(&json).unwrap());
                    return 0;
                }
                if found.len() > 1 {
                    println!("Multiple matches found.");
                    return 0;
                }
                let i = found[0];
                println!("{} [id: {}]", i.fullname(), i.id);
                println!("Username: {}", i.username);
                println!("Password: {}", i.password);
                for (n, v) in &i.fields {
                    println!("{n}: {v}");
                }
                for (id, file, _) in &i.attachments {
                    println!("{id}: {file}");
                }
                if !i.note.is_empty() {
                    println!("Notes:    {}", i.note);
                }
                0
            }
            "add" | "edit" => {
                if !db.logged_in {
                    return not_logged_in();
                }
                if !flag("--non-interactive") {
                    return die("the fake only supports --non-interactive");
                }
                let [name] = names.as_slice() else {
                    return die("expected one name");
                };
                let value = String::from_utf8(input).unwrap();
                let line = value.strip_suffix('\n').unwrap_or(&value).to_string();
                let existing: Vec<String> =
                    matches(&db, name).iter().map(|i| i.id.clone()).collect();
                let idx = match (cmd.as_str(), existing.as_slice()) {
                    ("edit", [id]) => db.items.iter().position(|i| &i.id == id).unwrap(),
                    ("edit", []) | ("add", _) => {
                        db.next_id += 1;
                        let (group, item) = name.rsplit_once('/').unwrap_or(("", name));
                        db.items.push(Item {
                            id: format!("{}", 9000 + db.next_id),
                            group: group.into(),
                            name: item.into(),
                            ..Item::default()
                        });
                        db.items.len() - 1
                    }
                    _ => return die("Multiple matches found."),
                };
                let item = &mut db.items[idx];
                if flag("--notes") {
                    item.note = value;
                } else if flag("--password") {
                    item.password = line;
                } else if flag("--username") {
                    item.username = line;
                } else if let Some(f) = flag_value("--field") {
                    item.fields.retain(|x| x.0 != f);
                    item.fields.push((f, line));
                } else {
                    return die("no field given");
                }
                save(&db);
                0
            }
            other => die(&format!("unknown command {other}")),
        }
    }
}

#[cfg(unix)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use dot_secrets::contract::{self, Harness, Seed};
    use dot_secrets::mock::MemoryProvider;
    use dot_secrets::{
        Interactive, LastpassConfig, LastpassProvider, NonInteractive, ProvidersConfig, Registry,
        Resolver, SecretProvider, SecretRef, SecretValue, SecretsConfig,
    };
    use tempfile::TempDir;

    use super::fake::{Call, Db, Item};

    type Test = (&'static str, fn());

    pub fn run_all() {
        let tests: &[Test] = &[
            ("contract", contract_lastpass),
            ("contract_without_multi_show", contract_without_multi_show),
            ("one_sync_one_show_per_batch", one_sync_one_show_per_batch),
            ("batch_falls_back_per_item", batch_falls_back_per_item),
            ("fields_and_ids", fields_and_ids),
            ("fields_missing_from_json", fields_missing_from_json),
            ("ambiguous_names_fail", ambiguous_names_fail),
            ("attachments", attachments),
            ("store_uses_stdin", store_uses_stdin),
            ("logged_out_without_terminal", logged_out_without_terminal),
            ("interactive_login", interactive_login),
            ("availability_follows_path", availability_follows_path),
            ("resolver_prompts_and_stores", resolver_prompts_and_stores),
        ];
        let args: Vec<String> = std::env::args().skip(1).collect();
        let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
        if args.iter().any(|a| a == "--list") {
            for (name, _) in tests {
                println!("{name}: test");
            }
            return;
        }
        let mut failed = Vec::new();
        let mut ran = 0;
        for (name, f) in tests {
            if !filters.is_empty() && !filters.iter().any(|flt| name.contains(flt.as_str())) {
                continue;
            }
            ran += 1;
            match catch_unwind(AssertUnwindSafe(f)) {
                Ok(()) => println!("test {name} ... ok"),
                Err(_) => {
                    println!("test {name} ... FAILED");
                    failed.push(*name);
                }
            }
        }
        println!(
            "\ntest result: {}. {} passed; {} failed",
            if failed.is_empty() { "ok" } else { "FAILED" },
            ran - failed.len(),
            failed.len()
        );
        if !failed.is_empty() {
            std::process::exit(101);
        }
    }

    /// A fake vault: a temp dir holding the DB and the `lpass` wrapper script.
    struct Vault {
        _tmp: TempDir,
        dir: PathBuf,
        db: PathBuf,
    }

    impl Vault {
        fn new(db: Db) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().canonicalize().unwrap();
            let bin = dir.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let db_path = dir.join("vault.json");
            std::fs::write(&db_path, serde_json::to_vec(&db).unwrap()).unwrap();
            let exe = std::env::current_exe().unwrap();
            let script = format!(
                "#!/bin/sh\nDOT_FAKE_LPASS_DB='{}' exec '{}' \"$@\"\n",
                db_path.display(),
                exe.display()
            );
            let lpass = bin.join("lpass");
            std::fs::write(&lpass, script).unwrap();
            std::fs::set_permissions(&lpass, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                _tmp: tmp,
                dir,
                db: db_path,
            }
        }

        fn bin(&self) -> PathBuf {
            self.dir.join("bin")
        }

        fn provider(&self, username: Option<&str>) -> LastpassProvider {
            LastpassProvider::new(LastpassConfig {
                username: username.map(str::to_string),
            })
            .with_search_path(Some(self.bin().into_os_string()))
        }

        fn db(&self) -> Db {
            serde_json::from_slice(&std::fs::read(&self.db).unwrap()).unwrap()
        }

        fn calls(&self) -> Vec<Call> {
            let log = super::fake::log_path(&self.db);
            std::fs::read_to_string(log)
                .unwrap_or_default()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }

        fn subcommands(&self) -> Vec<String> {
            self.calls().iter().map(|c| c.argv[0].clone()).collect()
        }
    }

    fn db(items: Vec<Item>) -> Db {
        Db {
            logged_in: true,
            username: "me@example.com".into(),
            items,
            multi_show: true,
            json_fields: true,
            ..Db::default()
        }
    }

    fn item(id: &str, group: &str, name: &str) -> Item {
        Item {
            id: id.into(),
            group: group.into(),
            name: name.into(),
            ..Item::default()
        }
    }

    fn r(s: &str) -> SecretRef {
        SecretRef::parse(s).unwrap()
    }

    fn texts(v: &[Option<SecretValue>]) -> Vec<Option<String>> {
        v.iter()
            .map(|x| x.as_ref().map(|x| x.expose_str().unwrap().to_string()))
            .collect()
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    fn ready(p: &mut LastpassProvider) {
        assert!(p.available().unwrap());
        p.unlock(&mut NonInteractive).unwrap();
    }

    // -----------------------------------------------------------------------------------------
    // The contract

    struct LpHarness {
        multi_show: bool,
        vaults: RefCell<Vec<Vault>>,
        n: Cell<u64>,
    }

    impl Harness for LpHarness {
        fn provider(&self, seed: &Seed) -> Box<dyn SecretProvider> {
            let mut items = Vec::new();
            for (k, v) in &seed.values {
                self.n.set(self.n.get() + 1);
                let mut i = item(&format!("{}", self.n.get()), "Contract", k);
                i.note = v.clone();
                items.push(i);
            }
            if !seed.files.is_empty() {
                let mut i = item("500", "Contract", "Files");
                for (n, (path, bytes)) in seed.files.iter().enumerate() {
                    i.attachments
                        .push((format!("att-500-{n}"), path.clone(), bytes.clone()));
                }
                items.push(i);
            }
            let mut d = db(items);
            d.multi_show = self.multi_show;
            let vault = Vault::new(d);
            let p = vault.provider(None);
            self.vaults.borrow_mut().push(vault);
            Box::new(p)
        }

        fn value_ref(&self, key: &str) -> SecretRef {
            r(&format!("lastpass://Contract/{key}#notes"))
        }

        fn file_ref(&self, path: &str) -> Option<SecretRef> {
            Some(r(&format!("lastpass://Contract/Files#attach={path}")))
        }
    }

    fn contract_lastpass() {
        contract::run(&LpHarness {
            multi_show: true,
            vaults: RefCell::default(),
            n: Cell::new(0),
        });
    }

    fn contract_without_multi_show() {
        contract::run(&LpHarness {
            multi_show: false,
            vaults: RefCell::default(),
            n: Cell::new(0),
        });
    }

    // -----------------------------------------------------------------------------------------
    // Batching

    fn gitlab_vault() -> Vault {
        let mut a = item("11", "Dev", "GitLab");
        a.username = "alice".into();
        a.password = "glpat-secret-1".into();
        a.fields = vec![("Token".into(), "tok-field-2".into())];
        let mut b = item("12", "Ops", "Grafana");
        b.password = "graf-pass-3".into();
        b.note = "line one\nline two".into();
        let mut c = item("13", "Shared-Team/Sub", "Item");
        c.password = "shared-4".into();
        Vault::new(db(vec![a, b, c]))
    }

    fn one_sync_one_show_per_batch() {
        let v = gitlab_vault();
        let mut p = v.provider(None);
        ready(&mut p);
        let refs = [
            r("lastpass://Dev/GitLab#password"),
            r("lastpass://Ops/Grafana#notes"),
            r("lastpass://Dev/Missing#password"),
            r("lastpass://Shared-Team/Sub/Item#password"),
            r("lastpass://Dev/GitLab#username"),
        ];
        let got = p.resolve(&refs).unwrap();
        assert_eq!(
            texts(&got),
            [
                some("glpat-secret-1"),
                some("line one\nline two"),
                None,
                some("shared-4"),
                some("alice")
            ]
        );
        // A second batch in the same run does not sync again.
        p.resolve(&[r("lastpass://Dev/GitLab#field=Token")])
            .unwrap();
        assert_eq!(v.subcommands(), ["status", "sync", "show", "show"]);
        let show = &v.calls()[2].argv;
        assert!(show.contains(&"--json".to_string()) && show.contains(&"--sync=no".to_string()));
        assert_eq!(
            show.iter().filter(|a| !a.starts_with('-')).count(),
            1 + 4,
            "one call names every item once: {show:?}"
        );
    }

    fn batch_falls_back_per_item() {
        let v = gitlab_vault();
        let mut d = v.db();
        d.multi_show = false;
        std::fs::write(&v.db, serde_json::to_vec(&d).unwrap()).unwrap();
        let mut p = v.provider(None);
        ready(&mut p);
        let got = p
            .resolve(&[
                r("lastpass://Dev/GitLab#password"),
                r("lastpass://Nope/Nothing#password"),
                r("lastpass://Ops/Grafana#password"),
            ])
            .unwrap();
        assert_eq!(
            texts(&got),
            [some("glpat-secret-1"), None, some("graf-pass-3")]
        );
        // The failed batch, then one call per item.
        assert_eq!(
            v.subcommands(),
            ["status", "sync", "show", "show", "show", "show"]
        );
        // Every name missing: `lpass show` fails the batch, and each item is missing.
        let got = p
            .resolve(&[r("lastpass://A/B#password"), r("lastpass://C/D#password")])
            .unwrap();
        assert_eq!(texts(&got), [None, None]);
    }

    fn fields_and_ids() {
        let v = gitlab_vault();
        let mut p = v.provider(None);
        ready(&mut p);
        let got = p
            .resolve(&[
                r("lastpass://id/12#password"),
                r("lastpass://Dev/GitLab#field=Token"),
                r("lastpass://Dev/GitLab#field=Nope"),
                r("lastpass://id/99#password"),
                r("lastpass://Ops/Grafana#username"),
            ])
            .unwrap();
        assert_eq!(
            texts(&got),
            [some("graf-pass-3"), some("tok-field-2"), None, None, None],
            "an empty field counts as missing"
        );
    }

    fn fields_missing_from_json() {
        let v = gitlab_vault();
        let mut d = v.db();
        d.json_fields = false;
        std::fs::write(&v.db, serde_json::to_vec(&d).unwrap()).unwrap();
        let mut p = v.provider(None);
        ready(&mut p);
        let got = p
            .resolve(&[
                r("lastpass://Dev/GitLab#field=Token"),
                r("lastpass://Dev/GitLab#field=Nope"),
            ])
            .unwrap();
        assert_eq!(texts(&got), [some("tok-field-2"), None]);
        let fields: Vec<Call> = v
            .calls()
            .into_iter()
            .filter(|c| c.argv.iter().any(|a| a.starts_with("--field=")))
            .collect();
        assert_eq!(fields.len(), 2, "one --field call per field ref");
        assert!(fields[0].argv.contains(&"11".to_string()), "by id");
    }

    fn ambiguous_names_fail() {
        let mut a = item("21", "Dev", "Dup");
        a.password = "dup-pass-one".into();
        let mut b = item("22", "Dev", "Dup");
        b.password = "dup-pass-two".into();
        let v = Vault::new(db(vec![a, b]));
        let mut p = v.provider(None);
        ready(&mut p);
        let e = format!(
            "{:#}",
            p.resolve(&[r("lastpass://Dev/Dup#password")]).unwrap_err()
        );
        assert!(
            e.contains("ids 21, 22") && e.contains("lastpass://id/"),
            "{e}"
        );
        assert!(!e.contains("dup-pass"), "{e}");
        assert_eq!(
            texts(&p.resolve(&[r("lastpass://id/22#password")]).unwrap()),
            [some("dup-pass-two")]
        );
    }

    fn attachments() {
        let mut a = item("31", "Dev", "VPN");
        a.password = "vpn-pass".into();
        a.attachments = vec![
            ("att-31-1".into(), "work.ovpn".into(), vec![0, 1, 2, 0xff]),
            ("att-31-2".into(), "readme.txt".into(), b"hello\n".to_vec()),
        ];
        let v = Vault::new(db(vec![a]));
        let mut p = v.provider(None);
        ready(&mut p);
        let got = p
            .resolve(&[
                r("lastpass://Dev/VPN#attach=work.ovpn"),
                r("lastpass://Dev/VPN#attach=nope.ovpn"),
                r("lastpass://Dev/VPN#password"),
            ])
            .unwrap();
        assert_eq!(got[0].as_ref().unwrap().expose(), [0, 1, 2, 0xff]);
        assert!(got[1].is_none());
        assert_eq!(got[2].as_ref().unwrap().expose_str(), Some("vpn-pass"));
        let attach: Vec<Call> = v
            .calls()
            .into_iter()
            .filter(|c| c.argv.iter().any(|a| a.starts_with("--attach=")))
            .collect();
        assert_eq!(attach.len(), 1);
        assert!(attach[0].argv.contains(&"--attach=att-31-1".to_string()));
        assert!(
            p.store(
                &r("lastpass://Dev/VPN#attach=new.bin"),
                &SecretValue::new(vec![1])
            )
            .is_err()
        );
    }

    fn store_uses_stdin() {
        let v = gitlab_vault();
        let mut p = v.provider(None);
        ready(&mut p);
        let secret = "n3w 'value' $HOME \"q\"";
        p.store(
            &r("lastpass://Dev/GitLab#password"),
            &SecretValue::from(secret.to_string()),
        )
        .unwrap();
        p.store(
            &r("lastpass://New/Thing#field=Api Key"),
            &SecretValue::from("fresh-field".to_string()),
        )
        .unwrap();
        let got = p
            .resolve(&[
                r("lastpass://Dev/GitLab#password"),
                r("lastpass://New/Thing#field=Api Key"),
            ])
            .unwrap();
        assert_eq!(texts(&got), [some(secret), some("fresh-field")]);
        let calls = v.calls();
        let writes: Vec<&Call> = calls
            .iter()
            .filter(|c| matches!(c.argv[0].as_str(), "add" | "edit"))
            .collect();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].argv[0], "edit");
        assert!(writes[0].argv.contains(&"11".to_string()), "edits by id");
        assert_eq!(writes[1].argv[0], "add");
        for w in &writes {
            assert!(w.stdin, "value on stdin");
            assert!(w.argv.contains(&"--non-interactive".to_string()));
        }
        for c in &calls {
            let argv = c.argv.join(" ");
            assert!(
                !argv.contains("n3w") && !argv.contains("fresh-field"),
                "{argv}"
            );
        }
        assert!(
            p.store(
                &r("lastpass://id/404#password"),
                &SecretValue::from("x".to_string())
            )
            .is_err()
        );
    }

    fn logged_out_without_terminal() {
        let v = gitlab_vault();
        let mut d = v.db();
        d.logged_in = false;
        std::fs::write(&v.db, serde_json::to_vec(&d).unwrap()).unwrap();
        let mut p = v.provider(Some("me@example.com"));
        let e = format!("{:#}", p.unlock(&mut NonInteractive).unwrap_err());
        assert!(e.contains("lpass login --trust me@example.com"), "{e}");
        assert!(!v.subcommands().contains(&"login".to_string()));

        // Through the resolver: locked, so the envfile-style fallback is used.
        let lp = v.bin().into_os_string();
        let ef = MemoryProvider::new("ef").with_value("GL", "from-fallback");
        let mut reg = Registry::new();
        reg.register("lastpass", move |_: &ProvidersConfig| {
            Ok(Box::new(
                LastpassProvider::new(LastpassConfig::default()).with_search_path(Some(lp.clone())),
            ) as Box<dyn SecretProvider>)
        });
        let efc = ef.clone();
        reg.register("ef", move |_: &ProvidersConfig| Ok(efc.boxed()));
        let cfg = SecretsConfig::from_yaml(
            "secrets:\n  glab: { ref: \"lastpass://Dev/GitLab#password\", fallback: \"ef:GL\" }\n",
            Path::new("secrets.yml"),
        )
        .unwrap();
        let mut resolver = Resolver::new(reg, cfg);
        let got = resolver.resolve(["glab"], &mut NonInteractive).unwrap();
        assert_eq!(got.get("glab").unwrap().expose_str(), Some("from-fallback"));
    }

    /// Interactive, with a record of how the console was used.
    #[derive(Default)]
    struct Tty {
        events: Vec<String>,
        answers: Vec<String>,
    }

    impl Interactive for Tty {
        fn is_interactive(&self) -> bool {
            true
        }
        fn suspend(&mut self, reason: &str) {
            self.events.push(format!("suspend {reason}"));
        }
        fn resume(&mut self) {
            self.events.push("resume".into());
        }
        fn read_secret(&mut self, prompt: &str) -> anyhow::Result<SecretValue> {
            self.events.push(format!("ask {prompt}"));
            Ok(SecretValue::from(self.answers.remove(0)))
        }
    }

    fn interactive_login() {
        let v = gitlab_vault();
        let mut d = v.db();
        d.logged_in = false;
        std::fs::write(&v.db, serde_json::to_vec(&d).unwrap()).unwrap();

        // No username: nothing to log in with.
        let e = format!(
            "{:#}",
            v.provider(None).unlock(&mut Tty::default()).unwrap_err()
        );
        assert!(e.contains("providers.lastpass.username"), "{e}");

        let mut p = v.provider(Some("wrong@example.com"));
        assert!(p.unlock(&mut Tty::default()).is_err());

        let mut p = v.provider(Some("me@example.com"));
        p.unlock(&mut Tty::default()).unwrap();
        assert!(v.db().logged_in);
        let logins: Vec<Call> = v
            .calls()
            .into_iter()
            .filter(|c| c.argv[0] == "login")
            .collect();
        assert_eq!(
            logins.last().unwrap().argv,
            ["login", "--trust", "me@example.com"]
        );

        // Through the resolver, the console is suspended around the unlock.
        d.logged_in = false;
        std::fs::write(&v.db, serde_json::to_vec(&d).unwrap()).unwrap();
        let lp = v.bin().into_os_string();
        let mut reg = Registry::new();
        reg.register("lastpass", move |_: &ProvidersConfig| {
            Ok(Box::new(
                LastpassProvider::new(LastpassConfig {
                    username: Some("me@example.com".into()),
                })
                .with_search_path(Some(lp.clone())),
            ) as Box<dyn SecretProvider>)
        });
        let cfg = SecretsConfig::from_yaml(
            "secrets:\n  glab: { ref: \"lastpass://Dev/GitLab#password\" }\n",
            Path::new("secrets.yml"),
        )
        .unwrap();
        let mut tty = Tty::default();
        let got = Resolver::new(reg, cfg).resolve(["glab"], &mut tty).unwrap();
        assert_eq!(
            got.get("glab").unwrap().expose_str(),
            Some("glpat-secret-1")
        );
        assert_eq!(
            tty.events,
            ["suspend unlocking secrets provider `lastpass`", "resume"]
        );
    }

    fn availability_follows_path() {
        let v = gitlab_vault();
        assert!(v.provider(None).available().unwrap());
        let empty = v.dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let p = LastpassProvider::new(LastpassConfig::default())
            .with_search_path(Some(empty.into_os_string()));
        assert!(!p.available().unwrap());
    }

    fn resolver_prompts_and_stores() {
        let v = gitlab_vault();
        let lp = v.bin().into_os_string();
        let made = std::sync::Arc::new(Mutex::new(0));
        let m = made.clone();
        let mut reg = Registry::new();
        reg.register("lastpass", move |_: &ProvidersConfig| {
            *m.lock().unwrap() += 1;
            Ok(Box::new(
                LastpassProvider::new(LastpassConfig::default()).with_search_path(Some(lp.clone())),
            ) as Box<dyn SecretProvider>)
        });
        let cfg = SecretsConfig::from_yaml(
            "secrets:\n  new_tok: { ref: \"lastpass://Dev/NewToken#password\" }\n",
            Path::new("secrets.yml"),
        )
        .unwrap();
        let mut tty = Tty {
            answers: vec!["typed-in-value".into()],
            ..Tty::default()
        };
        let mut resolver = Resolver::new(reg, cfg);
        let got = resolver.resolve(["new_tok"], &mut tty).unwrap();
        assert_eq!(
            got.get("new_tok").unwrap().expose_str(),
            Some("typed-in-value")
        );
        assert_eq!(
            got.stored().collect::<Vec<_>>(),
            [("new_tok", "lastpass://Dev/NewToken#password")]
        );
        let items: BTreeMap<String, String> = v
            .db()
            .items
            .iter()
            .map(|i| (i.fullname(), i.password.clone()))
            .collect();
        assert_eq!(items["Dev/NewToken"], "typed-in-value");
        assert_eq!(*made.lock().unwrap(), 1);
        for c in v.calls() {
            assert!(!c.argv.join(" ").contains("typed-in-value"));
        }
    }
}
