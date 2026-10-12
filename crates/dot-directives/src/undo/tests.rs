use std::os::unix::fs::symlink;

use dot_core::{EventSink, Options, engine};
use dot_platform::link::SystemLinks;

use super::*;
use crate::Builtins;
use crate::testutil::Fixture;

fn apply(f: &Fixture) -> dot_core::Summary {
    engine::run(
        std::slice::from_ref(&f.bundle),
        &Builtins::default(),
        &f.context(),
        &EventSink::new(),
        Options::default(),
    )
}

fn remover(f: &Fixture, uninstall: bool) -> Remover {
    Remover {
        links: Arc::new(SystemLinks::new()),
        packages: Packages::default(),
        system: System::default(),
        store: StateStore::at(&f.context().state_dir),
        home: f.home().to_path_buf(),
        uninstall,
    }
}

/// Checks and applies every removal of the bundle, dropping the records the way the CLI does;
/// returns the verdicts.
fn remove(f: &Fixture) -> Vec<(String, Verdict)> {
    let r = remover(f, false);
    let mut out = Vec::new();
    for removal in r.plan(&f.bundle.name) {
        let v = removal.check().unwrap();
        if matches!(v, Verdict::Undo(_)) {
            removal.apply(&mut |_, _| {}).unwrap();
        }
        if v.forgets() {
            r.store
                .update(|s| s.resources.remove(&removal.key))
                .unwrap();
        }
        out.push((removal.key.clone(), v));
    }
    out
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap()
}

#[test]
fn a_bundle_is_undone_and_what_was_there_before_comes_back() {
    let f = Fixture::new(concat!(
        "steps:\n",
        "  - create: [~/made]\n",
        "  - link: { ~/.rc: rc }\n",
        "  - template: { ~/.config/app.conf: app.j2 }\n",
        "  - block: { file: ~/.bashrc, content: \"export X=1\\n\" }\n",
        "  - path: [~/made/bin]\n",
    ));
    f.write_repo("rc", "repo rc\n");
    f.write_repo("app.j2", "from dot\n");
    fs::write(f.home().join(".rc"), "my rc\n").unwrap();
    fs::create_dir_all(f.home().join(".config")).unwrap();
    fs::write(f.home().join(".config/app.conf"), "my conf\n").unwrap();
    fs::write(f.home().join(".bashrc"), "mine\n").unwrap();
    let s = apply(&f);
    assert!(s.success(), "{s:?}");
    assert_eq!(read(&f.home().join(".rc")), "repo rc\n");
    assert_eq!(read(&f.home().join(".rc.dot-backup")), "my rc\n");

    let state = StateStore::at(&f.context().state_dir).load().unwrap();
    let keys: Vec<&str> = state.resources.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "block:~/.bashrc [dot:demo]",
            "dir:~/made",
            "file:~/.config/app.conf",
            "link:~/.rc",
            "path:~/made/bin",
        ]
    );
    assert!(
        state
            .resources
            .values()
            .all(|r| r.owned && r.bundle == "demo")
    );
    assert!(matches!(
        &state.resources["link:~/.rc"].restore,
        Some(Restore::Backup { path }) if path.ends_with(".rc.dot-backup")
    ));
    // The template's earlier file was saved in the state directory.
    let Some(Restore::Backup { path: saved }) = &state.resources["file:~/.config/app.conf"].restore
    else {
        panic!("no backup")
    };
    assert_eq!(read(saved), "my conf\n");

    // A second run changes nothing in the records' ownership or backups.
    assert!(apply(&f).success());
    let again = StateStore::at(&f.context().state_dir).load().unwrap();
    assert_eq!(again.resources, state.resources);

    let verdicts = remove(&f);
    assert!(
        verdicts.iter().all(|(_, v)| matches!(v, Verdict::Undo(_))),
        "{verdicts:#?}"
    );
    // Newest first: the path entry, then the block, ..., the directory last.
    assert_eq!(verdicts.first().unwrap().0, "path:~/made/bin");
    assert_eq!(verdicts.last().unwrap().0, "dir:~/made");
    assert_eq!(read(&f.home().join(".rc")), "my rc\n");
    assert!(!f.home().join(".rc.dot-backup").exists());
    assert_eq!(read(&f.home().join(".config/app.conf")), "my conf\n");
    assert_eq!(read(&f.home().join(".bashrc")), "mine\n");
    assert!(!f.home().join("made").exists());
    let after = StateStore::at(&f.context().state_dir).load().unwrap();
    assert!(after.resources.is_empty(), "{:?}", after.resources);
}

#[test]
fn what_changed_since_is_left_alone_and_what_was_there_is_kept() {
    let f = Fixture::new(concat!(
        "steps:\n",
        "  - link: { ~/.rc: rc, ~/.pre: pre }\n",
        "  - template: { ~/app.conf: app.j2 }\n",
        "  - create: [~/made]\n",
    ));
    f.write_repo("rc", "rc\n");
    f.write_repo("pre", "pre\n");
    f.write_repo("app.j2", "from dot\n");
    // Linked before dot ever ran (by hand, or dotbot): not dot's.
    symlink(f.repo().join("pre"), f.home().join(".pre")).unwrap();
    assert!(apply(&f).success());
    // Since then: the link points elsewhere, the file was edited, the directory has a file.
    fs::remove_file(f.home().join(".rc")).unwrap();
    symlink("/elsewhere", f.home().join(".rc")).unwrap();
    fs::write(f.home().join("app.conf"), "edited\n").unwrap();
    fs::write(f.home().join("made/keep"), "").unwrap();

    let verdicts = remove(&f);
    let v = |k: &str| verdicts.iter().find(|(key, _)| key == k).unwrap().1.clone();
    assert!(matches!(v("link:~/.rc"), Verdict::Conflict(m) if m.contains("no longer the link")));
    assert!(matches!(v("file:~/app.conf"), Verdict::Conflict(m) if m.contains("changed since")));
    assert!(
        matches!(v("dir:~/made"), Verdict::Keep { why, forget: true } if why.contains("not empty"))
    );
    assert!(
        matches!(v("link:~/.pre"), Verdict::Keep { why, .. } if why.contains("already in place"))
    );
    assert_eq!(read(&f.home().join("app.conf")), "edited\n");
    assert!(f.home().join(".pre").exists());
    // The conflicting records stay, for a later attempt.
    let left: Vec<String> = StateStore::at(&f.context().state_dir)
        .load()
        .unwrap()
        .resources
        .into_keys()
        .collect();
    assert_eq!(left, ["file:~/app.conf", "link:~/.rc"]);
}

#[test]
fn packages_stay_unless_asked_and_gone_things_are_just_forgotten() {
    let f = Fixture::new("steps:\n  - link: { ~/.rc: rc }\n");
    f.write_repo("rc", "rc\n");
    assert!(apply(&f).success());
    let store = StateStore::at(&f.context().state_dir);
    store
        .update(|s| {
            s.record(
                "pkg:apt:fd-find",
                "demo",
                "pkg",
                Some(Undo::Uninstall {
                    manager: "apt".into(),
                    name: "fd-find".into(),
                }),
                true,
                None,
            )
        })
        .unwrap();
    fs::remove_file(f.home().join(".rc")).unwrap();
    let verdicts = remove(&f);
    assert!(
        matches!(&verdicts[0].1, Verdict::Keep { forget: false, why } if why.contains("--packages"))
    );
    assert!(matches!(&verdicts[1].1, Verdict::Gone(_)));
    let left: Vec<String> = store.load().unwrap().resources.into_keys().collect();
    assert_eq!(left, ["pkg:apt:fd-find"]);
}

#[test]
fn a_root_owned_file_is_saved_before_the_first_write_and_put_back_as_root() {
    use std::os::unix::fs::PermissionsExt;

    let f = Fixture::new("steps:\n  - template: { ~/etc/app.conf: { src: app.j2, root: true } }\n");
    f.write_repo("app.j2", "from dot\n");
    let dest = f.home().join("etc/app.conf");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(&dest, "original\n").unwrap();
    fs::set_permissions(&dest, fs::Permissions::from_mode(0o640)).unwrap();
    let (mut b, fake) = crate::testutil_sys::fake_builtins();
    // SAFETY: getuid cannot fail. Tests cannot make root's files: this user's stand in.
    b.root_uid = unsafe { libc::getuid() };
    let s = engine::run(
        std::slice::from_ref(&f.bundle),
        &b,
        &f.context(),
        &EventSink::new(),
        Options::default(),
    );
    assert!(s.success(), "{s:?}");
    assert!(
        fake.calls()
            .iter()
            .any(|c| c.starts_with("sudo -n install -m 0640 -o root")),
        "{:?}",
        fake.calls()
    );
    // Recorded as a root-owned file, with the original saved to be put back as root.
    let store = StateStore::at(&f.context().state_dir);
    let rec = store.load().unwrap().resources["file:~/etc/app.conf"].clone();
    assert!(rec.owned);
    assert!(matches!(
        &rec.undo,
        Some(Undo::RemoveFiles { root: true, .. })
    ));
    let Some(Restore::RootBackup { path: saved, mode }) = rec.restore.clone() else {
        panic!("{rec:?}");
    };
    assert_eq!(mode, 0o640);
    assert_eq!(read(&saved), "original\n");

    // What the fake `install` would have written (the record holds its hash).
    fs::write(&dest, "from dot\n").unwrap();
    fake.clear_calls();
    let r = Remover {
        system: b.system.clone(),
        ..remover(&f, false)
    };
    let removal = r.plan(&f.bundle.name).pop().unwrap();
    assert!(removal.needs_sudo(), "putting it back takes root");
    let v = removal.check().unwrap();
    assert!(
        matches!(&v, Verdict::Undo(m) if m.contains("put back ~/etc/app.conf (as root) from")),
        "{v:?}"
    );
    removal.apply(&mut |_, _| {}).unwrap();
    let calls = fake.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("sudo -n install -m 0640 -o root")
                && c.ends_with(&format!(" {}", dest.display()))),
        "{calls:?}"
    );
    assert!(!saved.exists(), "the saved copy is used up");
}

#[test]
fn root_blocks_and_directories_are_undone_through_sudo() {
    let blk = Undo::RemoveBlock {
        path: "/etc/x".into(),
        begin: "# >>> dot:b:m >>>".into(),
        end: "# <<< dot:b:m <<<".into(),
        root: true,
        created_file: false,
        created_dirs: Vec::new(),
    };
    let json = serde_json::to_string(&blk).unwrap();
    assert!(json.contains(r#""root":true"#), "{json}");
    assert_eq!(serde_json::from_str::<Undo>(&json).unwrap(), blk);
    // Records written before the flag existed read as not root-owned.
    let old: Undo = serde_json::from_str(r#"{"op":"remove_dir","path":"/x"}"#).unwrap();
    assert_eq!(
        old,
        Undo::RemoveDir {
            path: "/x".into(),
            root: false
        }
    );

    // A root block: taken out of the file through `sudo -n install`, the rest kept.
    let f = Fixture::new("steps:\n  - link: { ~/.rc: rc }\n");
    let file = f.home().join("etc.conf");
    fs::write(&file, "keep\n# >>> dot:b:m >>>\nx=1\n# <<< dot:b:m <<<\n").unwrap();
    let store = StateStore::at(&f.context().state_dir);
    store
        .update(|s| {
            s.record(
                "block:~/etc.conf [dot:b:m]",
                &f.bundle.name,
                "block",
                Some(Undo::RemoveBlock {
                    path: file.clone(),
                    begin: "# >>> dot:b:m >>>".into(),
                    end: "# <<< dot:b:m <<<".into(),
                    root: true,
                    created_file: false,
                    created_dirs: Vec::new(),
                }),
                true,
                None,
            )
        })
        .unwrap();
    let (b, fake) = crate::testutil_sys::fake_builtins();
    let r = Remover {
        system: b.system.clone(),
        ..remover(&f, false)
    };
    let removal = r.plan(&f.bundle.name).pop().unwrap();
    assert!(removal.needs_sudo());
    assert!(matches!(removal.check().unwrap(), Verdict::Undo(m) if m.contains("remove block")));
    removal.apply(&mut |_, _| {}).unwrap();
    let calls = fake.calls();
    assert!(
        calls.iter().any(|c| c.starts_with("sudo -n install -m ")
            && c.ends_with(&format!(" {}", file.display()))),
        "{calls:?}"
    );
}

#[test]
fn an_unreadable_root_file_is_compared_through_sudo_before_it_is_removed() {
    use std::os::unix::fs::PermissionsExt;

    if unsafe { libc::geteuid() } == 0 {
        return; // root reads everything
    }
    let f = Fixture::new(
        "steps:\n  - template: { ~/sudoers.d/dot: { src: s.j2, root: true, mode: \"0440\" } }\n",
    );
    f.write_repo("s.j2", "me ALL=(ALL) NOPASSWD: ALL\n");
    let (mut b, fake) = crate::testutil_sys::fake_builtins();
    // SAFETY: getuid cannot fail. Tests cannot make root's files: this user's stand in.
    b.root_uid = unsafe { libc::getuid() };
    let s = engine::run(
        std::slice::from_ref(&f.bundle),
        &b,
        &f.context(),
        &EventSink::new(),
        Options::default(),
    );
    assert!(s.success(), "{s:?}");
    // What the fake `install` would have put there, unreadable by this user (like 0440 root).
    let dest = f.home().join("sudoers.d/dot");
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(&dest, "me ALL=(ALL) NOPASSWD: ALL\n").unwrap();
    fs::set_permissions(&dest, fs::Permissions::from_mode(0o000)).unwrap();
    // The hash recorded is that of what dot wrote, not of a file it could not read back.
    let store = StateStore::at(&f.context().state_dir);
    let rec = store.load().unwrap().resources["file:~/sudoers.d/dot"].clone();
    let Some(Undo::RemoveFiles { files, root: true }) = &rec.undo else {
        panic!("{rec:?}");
    };
    assert_eq!(
        files[0].sha256.as_deref(),
        Some(crate::fileutil::sha256_hex(b"me ALL=(ALL) NOPASSWD: ALL\n").as_str())
    );
    let r = Remover {
        system: b.system.clone(),
        ..remover(&f, false)
    };
    // An admin edited it since: read through sudo, it differs, and it is left alone.
    fake.on(
        "sudo -n cat",
        dot_platform::exec::Output::ok("me ALL=(ALL) ALL\nadmin ALL=(ALL) ALL\n"),
    );
    let removal = r.plan(&f.bundle.name).pop().unwrap();
    let v = removal.check().unwrap();
    assert!(
        matches!(&v, Verdict::Conflict(m) if m.contains("changed since")),
        "{v:?}"
    );
    assert!(removal.apply(&mut |_, _| {}).is_err());
    assert!(dest.exists());
    // Unreadable even through sudo (not primed): compared when removing, never removed blind.
    fake.on("sudo -n cat", dot_platform::exec::Output::exit(1, ""));
    let v = removal.check().unwrap();
    assert!(
        matches!(&v, Verdict::Undo(m) if m.contains("compared as root")),
        "{v:?}"
    );
    let e = format!("{:#}", removal.apply(&mut |_, _| {}).unwrap_err());
    assert!(e.contains("even through sudo"), "{e}");
    assert!(dest.exists());
    // Unchanged: removed.
    fake.on(
        "sudo -n cat",
        dot_platform::exec::Output::ok("me ALL=(ALL) NOPASSWD: ALL\n"),
    );
    assert!(matches!(removal.check().unwrap(), Verdict::Undo(_)));
    removal.apply(&mut |_, _| {}).unwrap();
    assert!(!dest.exists());

    // A root-owned record without a hash cannot be told unchanged: left alone.
    fs::write(&dest, "x\n").unwrap();
    store
        .update(|s| {
            s.record(
                "file:~/sudoers.d/dot",
                &f.bundle.name,
                "file",
                Some(Undo::RemoveFiles {
                    files: vec![Written {
                        path: dest.clone(),
                        sha256: None,
                    }],
                    root: true,
                }),
                true,
                None,
            )
        })
        .unwrap();
    let removal = r.plan(&f.bundle.name).pop().unwrap();
    let v = removal.check().unwrap();
    assert!(
        matches!(&v, Verdict::Conflict(m) if m.contains("no checksum")),
        "{v:?}"
    );
}

#[test]
fn a_directory_that_was_there_is_not_removed_when_create_only_fixed_its_mode() {
    use std::os::unix::fs::PermissionsExt;

    let f =
        Fixture::new("steps:\n  - create: { ~/.ssh: { mode: \"0700\" } }\n  - create: [~/made]\n");
    let ssh = f.home().join(".ssh");
    fs::create_dir(&ssh).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let s = apply(&f);
    assert!(s.success(), "{s:?}");
    assert_eq!(s.changed, 2, "{s:?}");
    assert_eq!(
        fs::metadata(&ssh).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let state = StateStore::at(&f.context().state_dir).load().unwrap();
    assert!(!state.resources["dir:~/.ssh"].owned);
    assert!(state.resources["dir:~/made"].owned);
    let verdicts = remove(&f);
    assert!(ssh.is_dir(), "the user's directory stays: {verdicts:?}");
    assert!(!f.home().join("made").exists(), "{verdicts:?}");
    assert!(
        verdicts
            .iter()
            .any(|(k, v)| k == "dir:~/.ssh" && matches!(v, Verdict::Keep { .. })),
        "{verdicts:?}"
    );
}

#[test]
fn an_empty_file_create_made_is_removed_only_while_empty() {
    let f = Fixture::new(
        "steps:\n  - create: { ~/.local.zshrc: { file: true }, ~/.local.vimrc: { file: true } }\n",
    );
    let s = apply(&f);
    assert!(s.success(), "{s:?}");
    fs::write(f.home().join(".local.vimrc"), "set nu\n").unwrap();
    let verdicts = remove(&f);
    assert!(!f.home().join(".local.zshrc").exists(), "{verdicts:?}");
    assert_eq!(read(&f.home().join(".local.vimrc")), "set nu\n");
    assert!(
        verdicts
            .iter()
            .any(|(k, v)| k == "file:~/.local.vimrc" && matches!(v, Verdict::Conflict(_))),
        "{verdicts:?}"
    );
}

#[test]
fn a_file_and_directories_dot_created_for_a_block_go_with_it() {
    use dot_core::Planner;
    // `dot` creates ~/.config/tool/rc (and ~/.config/tool) to hold the block.
    let f = Fixture::new(concat!(
        "steps:\n",
        "  - block: { file: ~/.config/tool/rc, content: \"x=1\" }\n",
    ));
    fs::create_dir_all(f.home().join(".config")).unwrap();
    let ctx = f.context();
    let store = StateStore::at(&ctx.state_dir);
    let apply_and_record = || {
        let refs = dot_core::SecretRefs::default();
        let s = &f.bundle.steps[0];
        let pctx = dot_core::PlanCtx::new(&ctx, &f.bundle, s, &refs);
        let actions = crate::Builtins::default()
            .plan(&s.directive, &pctx)
            .unwrap();
        let outcome = f.apply_one(actions[0].as_ref()).unwrap();
        store
            .record_applied(
                &f.bundle.name,
                &actions[0].resources(),
                outcome == dot_core::Outcome::Changed,
                &[],
            )
            .unwrap();
    };
    apply_and_record();
    // A second, converged run records the block again: what the first one knew is kept.
    apply_and_record();
    let rec = store.read().resources.into_values().next().unwrap();
    match rec.undo {
        Some(Undo::RemoveBlock {
            created_file,
            created_dirs,
            ..
        }) => {
            assert!(created_file);
            assert_eq!(created_dirs, [f.home().join(".config/tool")]);
        }
        other => panic!("{other:?}"),
    }
    let removal = remover(&f, false).plan(&f.bundle.name).pop().unwrap();
    let v = removal.check().unwrap();
    assert!(
        matches!(&v, Verdict::Undo(m) if m.contains("dot created it for block")),
        "{v:?}"
    );
    removal.apply(&mut |_, _| {}).unwrap();
    assert!(
        !f.home().join(".config/tool").exists(),
        "file and directory gone"
    );
    assert!(f.home().join(".config").exists(), "what was there stays");

    // Something else was added to the file: only the block goes.
    apply_and_record();
    let rc = f.home().join(".config/tool/rc");
    let mut text = fs::read_to_string(&rc).unwrap();
    text.push_str("mine=1\n");
    fs::write(&rc, text).unwrap();
    remover(&f, false)
        .plan(&f.bundle.name)
        .pop()
        .unwrap()
        .apply(&mut |_, _| {})
        .unwrap();
    assert_eq!(fs::read_to_string(&rc).unwrap(), "mine=1\n");
}
