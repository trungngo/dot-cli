use std::sync::Arc;

use super::*;
use crate::runenv::{EnvVar, PathEntry, PathPosition};

fn store() -> (tempfile::TempDir, StateStore) {
    let tmp = tempfile::tempdir().unwrap();
    let s = StateStore::at(tmp.path().join("state/dot"));
    (tmp, s)
}

#[test]
fn an_empty_directory_is_an_empty_state_and_nothing_is_written_by_reading() {
    let (_tmp, s) = store();
    assert_eq!(s.load().unwrap(), State::new());
    assert!(!s.dir().exists());
}

#[test]
fn version_0_files_are_migrated_once_and_moved_to_legacy() {
    let (_tmp, s) = store();
    let d = s.dir();
    fs::create_dir_all(d.join("services")).unwrap();
    fs::write(
        d.join("links.json"),
        r#"{"links":{"/h/rc":{"dest":"/h/rc","src":"/r/rc","kind":"copy","sha256":"ab"}}}"#,
    )
    .unwrap();
    fs::write(
        d.join("github-releases.json"),
        r#"{"installs":{"a/b /h/bin":{"repo":"a/b","dir":"/h/bin","tag":"v1","asset":"b.tgz","bins":["b"]}}}"#,
    )
    .unwrap();
    fs::write(
        d.join("env.json"),
        r#"{"path":[{"dir":"/h/bin","position":"prepend","bundle":"x"}],"vars":[{"name":"E","value":"1","bundle":"x"}]}"#,
    )
    .unwrap();
    fs::write(d.join("apt-update.stamp"), "1700000000\n").unwrap();
    fs::write(d.join("services/systemd-user-a.service"), "/h/a.service").unwrap();

    let st = s.load().unwrap();
    assert_eq!(st.version, SCHEMA_VERSION);
    let links: serde_json::Value = st.section(sections::LINKS);
    assert_eq!(links["/h/rc"]["kind"], "copy");
    let gh: serde_json::Value = st.section(sections::GITHUB_RELEASES);
    assert_eq!(gh["a/b /h/bin"]["tag"], "v1");
    let apt: serde_json::Value = st.section(sections::APT);
    assert_eq!(apt["refreshed"], 1_700_000_000u64);
    let svc: BTreeMap<String, String> = st.section(sections::SERVICES);
    assert_eq!(svc["systemd-user-a.service"], "/h/a.service");
    assert_eq!(
        st.env.path,
        [PathEntry {
            dir: "/h/bin".into(),
            position: PathPosition::Prepend,
            bundle: "x".into()
        }]
    );
    assert_eq!(
        st.env.vars,
        [EnvVar {
            name: "E".into(),
            value: "1".into(),
            bundle: "x".into()
        }]
    );

    // On disk: state.json, and the old files under legacy/.
    assert!(s.path().exists());
    for old in [
        "links.json",
        "github-releases.json",
        "env.json",
        "apt-update.stamp",
        "services",
    ] {
        assert!(!d.join(old).exists(), "{old} still there");
        assert!(d.join(LEGACY_DIR).join(old).exists(), "{old} not kept");
    }
    // A second read is the same state, from state.json.
    assert_eq!(s.load().unwrap(), st);
    // Owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(s.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn a_legacy_file_is_migrated_by_the_first_update_too() {
    let (_tmp, s) = store();
    fs::create_dir_all(s.dir()).unwrap();
    fs::write(s.dir().join("apt-update.stamp"), "42\n").unwrap();
    s.update(|st| st.set_section("other", &serde_json::json!({"k": 1})))
        .unwrap();
    let st = s.load().unwrap();
    let apt: serde_json::Value = st.section(sections::APT);
    assert_eq!(apt["refreshed"], 42);
    assert!(!s.dir().join("apt-update.stamp").exists());
}

#[test]
fn malformed_sections_and_files_count_as_nothing_recorded() {
    let (_tmp, s) = store();
    s.update(|st| {
        st.sections
            .insert("links".into(), serde_json::json!("not a map"))
    })
    .unwrap();
    let links: BTreeMap<String, String> = s.section("links");
    assert!(links.is_empty());

    fs::write(s.path(), "{ not json").unwrap();
    assert!(matches!(s.load(), Err(StateError::Corrupt { .. })));
    assert_eq!(s.read(), State::default());
    // The next write keeps the broken file aside and starts afresh.
    s.update(|st| st.set_section("k", &serde_json::json!({"a": 1})))
        .unwrap();
    assert!(s.dir().join("state.json.corrupt").exists());
    assert_eq!(s.load().unwrap().section::<serde_json::Value>("k")["a"], 1);
}

#[test]
fn a_newer_schema_is_never_written_to() {
    let (_tmp, s) = store();
    fs::create_dir_all(s.dir()).unwrap();
    fs::write(s.path(), r#"{"version": 99, "future": true}"#).unwrap();
    let e = s.load().unwrap_err().to_string();
    assert!(e.contains("schema version 99"), "{e}");
    let e = format!("{:#}", s.update(|_| ()).unwrap_err());
    assert!(e.contains("schema version 99"), "{e}");
    assert!(fs::read_to_string(s.path()).unwrap().contains("\"future\""));
}

#[test]
fn records_track_ownership_and_restore() {
    let mut st = State::new();
    // Already in place when first met: not owned.
    st.record("link:~/.a", "a", "link", None, false, None);
    assert!(!st.resources["link:~/.a"].owned);
    assert!(st.first_write("link:~/.a"));
    // Changed later: owned from then on, and the restore taken then is kept.
    let backup = Restore::Backup {
        path: "/h/.a.dot-backup".into(),
    };
    st.record("link:~/.a", "b", "link", None, true, Some(backup.clone()));
    st.record("link:~/.a", "b", "link", None, false, None);
    let r = &st.resources["link:~/.a"];
    assert!(r.owned);
    assert_eq!(r.bundle, "b");
    assert_eq!(r.restore, Some(backup));
    assert!(!st.first_write("link:~/.a"));
    st.record("file:~/.c", "b", "file", None, true, None);
    let order: Vec<String> = st.records_of("b").into_iter().map(|(k, _)| k).collect();
    assert_eq!(order, ["file:~/.c", "link:~/.a"]);
    assert!(st.records_of("a").is_empty());
}

#[test]
fn concurrent_updates_never_lose_each_others_changes() {
    let (_tmp, s) = store();
    let s = Arc::new(s);
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let s = StateStore::at(s.dir());
            std::thread::spawn(move || {
                for i in 0..25 {
                    s.update(|st| {
                        let mut n: u64 = st.section("counter");
                        n += 1;
                        st.set_section("counter", &n);
                        st.record(&format!("file:/t{t}/{i}"), "b", "file", None, true, None);
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let st = s.load().unwrap();
    assert_eq!(st.section::<u64>("counter"), 200);
    assert_eq!(st.resources.len(), 200);
    let mut seqs: Vec<u64> = st.resources.values().map(|r| r.seq).collect();
    seqs.sort();
    assert_eq!(seqs, (0..200).collect::<Vec<_>>());
}

#[test]
fn the_run_lock_is_exclusive_and_a_leftover_record_is_stale() {
    let (_tmp, s) = store();
    assert_eq!(s.run_lock_status(), RunLockStatus::Free);
    let held = s.run_lock("apply").unwrap();
    match s.run_lock_status() {
        RunLockStatus::Held(Some(i)) => {
            assert_eq!(i.pid, std::process::id());
            assert_eq!(i.command, "apply");
        }
        other => panic!("{other:?}"),
    }
    // Another holder (a second open of the lock file, as another process would) is refused.
    let e = format!(
        "{:#}",
        StateStore::at(s.dir()).run_lock("remove").unwrap_err()
    );
    assert!(e.contains("another dot run is in progress"), "{e}");
    assert!(e.contains("`dot apply`"), "{e}");
    drop(held);
    assert_eq!(s.run_lock_status(), RunLockStatus::Free);
    // A run that died leaves its record, not the lock.
    fs::write(
        s.dir().join(RUN_INFO_FILE),
        r#"{"pid":12345,"command":"apply","started":1}"#,
    )
    .unwrap();
    assert!(matches!(s.run_lock_status(), RunLockStatus::Stale(i) if i.pid == 12345));
    s.clear_stale_run().unwrap();
    assert_eq!(s.run_lock_status(), RunLockStatus::Free);
    let again = s.run_lock("apply");
    assert!(again.is_ok());
}
