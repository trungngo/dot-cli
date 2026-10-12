//! `dot diff` for the file directives: what is shown, and that secrets never are.

use std::fs;

use dot_core::Diff;

use crate::testutil::Fixture;
use crate::testutil_sys::plan_applying;

fn diff_of(f: &Fixture, step: usize) -> Option<Diff> {
    let actions = f.plan(step).unwrap();
    actions[0].status().unwrap();
    actions[0].diff().unwrap()
}

#[test]
fn a_template_shows_a_unified_diff_and_nothing_for_secrets() {
    let f = Fixture::new(concat!(
        "steps:\n",
        "  - template: { ~/app.conf: app.j2 }\n",
        "  - template: { ~/token.conf: token.j2 }\n",
    ));
    f.write_repo("app.j2", "a\nb={{ os }}\nc\n");
    f.write_repo("token.j2", "token={{ secret(\"tok\") }}\n");
    fs::write(f.home().join("app.conf"), "a\nb=old\nc\n").unwrap();
    fs::write(f.home().join("token.conf"), "token=hunter2\n").unwrap();
    match diff_of(&f, 0) {
        Some(Diff::File { unified, .. }) => assert_eq!(
            unified,
            "--- ~/app.conf\n+++ ~/app.conf (rendered from app.j2)\n@@ -1,3 +1,3 @@\n a\n-b=old\n+b=linux\n c\n"
        ),
        other => panic!("{other:?}"),
    }
    // Planned or resolved, a template with secrets is never compared in a diff.
    let d = diff_of(&f, 1).unwrap().to_string();
    assert!(d.contains("uses secrets tok; content not shown"), "{d}");
    let resolved = plan_applying(&f, 1, &[("tok", "s3cret")]);
    resolved[0].status().unwrap();
    let d = resolved[0].diff().unwrap().unwrap().to_string();
    assert!(!d.contains("hunter2") && !d.contains("s3cret"), "{d}");
    // In place: no diff.
    fs::write(f.home().join("app.conf"), "a\nb=linux\nc\n").unwrap();
    assert_eq!(diff_of(&f, 0), None);
}

#[test]
fn a_block_diff_shows_the_block_only() {
    let f = Fixture::new(
        "steps:\n  - block: { file: ~/.bashrc, marker: tools, content: \"export A=2\\n\" }\n",
    );
    fs::write(
        f.home().join(".bashrc"),
        "export SECRET_TOKEN=abc\n# >>> dot:demo:tools >>>\nexport A=1\n# <<< dot:demo:tools <<<\n",
    )
    .unwrap();
    let d = diff_of(&f, 0).unwrap().to_string();
    assert!(d.contains("-export A=1\n+export A=2\n"), "{d}");
    assert!(!d.contains("SECRET_TOKEN"), "{d}");
}

#[test]
fn a_link_diff_says_what_is_there_and_where_it_should_point() {
    let f = Fixture::new("steps:\n  - link: { ~/.rc: rc }\n");
    f.write_repo("rc", "x");
    fs::write(f.home().join(".rc"), "mine").unwrap();
    match diff_of(&f, 0) {
        Some(Diff::Link { path, from, to }) => {
            assert_eq!(path, "~/.rc");
            assert_eq!(from.as_deref(), Some("a file"));
            assert!(to.starts_with("link to ") && to.ends_with("rc"), "{to}");
        }
        other => panic!("{other:?}"),
    }
}
