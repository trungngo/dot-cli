//! `dot apply` on a real pseudo-terminal: the "Apply?" question and then the hidden prompt for a
//! missing secret are both answered through the terminal. Regression test for a deadlock where
//! the confirmation prompter held the stdin lock for the whole run, so the secret prompt (which
//! reads through `Stdin::read_line`) blocked forever. HOME, the state dir and PATH point into a
//! temp dir; nothing else is touched.
#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn openpty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: plain out-pointers; null name, termios and window size mean "defaults".
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both descriptors were just opened and are owned by nothing else.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// Everything the child wrote to the terminal so far.
struct Screen(Arc<Mutex<Vec<u8>>>);

impl Screen {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// Waits until the screen shows `what` (after `from` bytes), failing after a timeout.
    fn wait_for(&self, what: &str, from: usize) -> usize {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let t = self.text();
            if let Some(i) = t.get(from..).and_then(|s| s.find(what)) {
                return from + i + what.len();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what:?}; screen:\n{t}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn write(p: &Path, text: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn apply_answering_on_a_pty(extra: &[&str]) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    write(&root.join("repo/dot.yml"), "vars: {}\n");
    write(
        &root.join("repo/secrets.yml"),
        "secrets:\n  api_token: { ref: \"envfile:API_TOKEN\" }\n",
    );
    write(
        &root.join("repo/bundles/app/bundle.yml"),
        "steps:\n  - secret_file:\n      ~/.app/token: api_token\n",
    );
    let env_file = home.join(".config/dot/secrets.env");
    write(&env_file, "OTHER=1\n");
    fs::set_permissions(&env_file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir_all(root.join("state")).unwrap();

    let (master, slave) = openpty();
    let io = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
    let mut args = vec!["apply", "app"];
    args.extend_from_slice(extra);
    let mut child = Command::new(env!("CARGO_BIN_EXE_dot"))
        .args(&args)
        .current_dir(root.join("repo"))
        .env("HOME", &home)
        .env_remove("DOT_STATE_DIR")
        .env_remove("XDG_CONFIG_HOME")
        .env("XDG_STATE_HOME", root.join("state"))
        .env("PATH", "/usr/bin:/bin")
        .env("DOT_ROLE", "server")
        .env("TERM", "xterm")
        .env("LC_ALL", "C")
        .env_remove("NO_COLOR")
        .env_remove("DOT_REPO")
        .env_remove("DOT_PROFILE")
        .env("GIT_CEILING_DIRECTORIES", &root)
        .stdin(io(&slave))
        .stdout(io(&slave))
        .stderr(io(&slave))
        .spawn()
        .unwrap();
    // Only the child holds the terminal's slave side now, so reads end when it exits.
    drop(slave);

    let buf = Arc::new(Mutex::new(Vec::new()));
    let mut reader = fs::File::from(master.try_clone().unwrap());
    let sink = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });
    let screen = Screen(buf);
    let mut keys = fs::File::from(master);

    let at = screen.wait_for("Apply?", 0);
    keys.write_all(b"y\n").unwrap();
    // Typed only once the prompt is up: hidden input discards what was typed ahead.
    screen.wait_for("Value (hidden)", at);
    keys.write_all(b"typed-secret-value\n").unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "dot apply {extra:?} hangs after the secret was typed; screen:\n{}",
                screen.text()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{status}; screen:\n{}", screen.text());
    assert_eq!(
        fs::read_to_string(home.join(".app/token")).unwrap(),
        "typed-secret-value"
    );
    assert!(
        !screen.text().contains("typed-secret-value"),
        "the secret was echoed or printed:\n{}",
        screen.text()
    );
}

#[test]
fn prompts_for_a_missing_secret_after_confirming_tty_renderer() {
    apply_answering_on_a_pty(&[]);
}

#[test]
fn prompts_for_a_missing_secret_after_confirming_plain() {
    apply_answering_on_a_pty(&["--plain"]);
}
