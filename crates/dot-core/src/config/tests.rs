use super::*;
use crate::config::{LinkDefaults, ShellDefaults};
use crate::facts::Fact;

const FILE: &str = "/nonexistent-dot-test/bundles/demo/bundle.yml";

/// `FILE` as an absolute path on this platform.
fn file() -> std::path::PathBuf {
    crate::paths::tests::abs(FILE)
}

fn parse(yaml: &str) -> Result<Bundle, ConfigError> {
    parse_bundle(yaml, &file())
}

/// An absolute destination on this platform, for YAML.
fn abs_target(p: &str) -> String {
    crate::paths::tests::abs(p)
        .display()
        .to_string()
        .replace('\\', "/")
}

fn err(yaml: &str) -> ConfigError {
    match parse(yaml) {
        Ok(b) => panic!("expected an error, parsed {b:#?}"),
        Err(e) => e,
    }
}

#[test]
fn parses_a_full_bundle() {
    let yaml = r#"
name: demo
description: A demo bundle
requires: [common]
after: [zsh]
provides: [node, npm]
requires_if:
  - when: { os: macos }
    requires: [homebrew]
when: { role: [desktop, server] }
check: command -v fnm
steps:
  - link:
      ~/.npmrc: npmrc
      ~/.config/x:
        src: x
        create: false
        relink: false
        on_conflict: overwrite
  - create: [~/.cache/demo, ABS]
  - create:
      ~/.ssh: { mode: "0700" }
      ~/.local/bin:
  - shell: echo hi
    label: Say hi
  - shell:
      cmd: make install
      cwd: sub
      env: { A: "1", B: two }
      interactive: true
      unless: command -v thing
    when: { os: linux, arch: [x86_64, aarch64] }
  - clean: [~/]
  - clean:
      ~/.config: { recursive: true }
"#
    .replace("ABS", &abs_target("/tmp/abs"));
    let b = parse(&yaml).unwrap();
    assert_eq!(b.name, "demo");
    assert_eq!(b.description.as_deref(), Some("A demo bundle"));
    assert_eq!(b.dir, file().parent().unwrap());
    assert_eq!(b.root, b.dir);
    assert_eq!(b.manifest.requires, ["common"]);
    assert_eq!(b.manifest.after, ["zsh"]);
    assert_eq!(b.manifest.provides, ["node", "npm"]);
    assert_eq!(b.manifest.requires_if.len(), 1);
    assert_eq!(b.manifest.check.as_deref(), Some("command -v fnm"));
    let when = b.manifest.when.as_ref().unwrap();
    assert_eq!(when.0[&Fact::Role], ["desktop", "server"]);
    assert_eq!(b.steps.len(), 7);

    let Directive::Link(links) = &b.steps[0].directive else {
        panic!()
    };
    assert_eq!(
        links[0],
        LinkEntry {
            dest: "~/.npmrc".into(),
            src: "npmrc".into(),
            create: None,
            relink: None,
            on_conflict: None,
        }
    );
    assert_eq!(
        (links[1].create, links[1].relink),
        (Some(false), Some(false))
    );
    assert_eq!(links[1].on_conflict, Some(OnConflict::Overwrite));
    let builtin = links[0].resolve(&Defaults::default());
    assert!(builtin.create && builtin.relink);
    assert_eq!(builtin.on_conflict, OnConflict::Backup);

    let Directive::Create(c) = &b.steps[1].directive else {
        panic!()
    };
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].mode, None);
    let Directive::Create(c) = &b.steps[2].directive else {
        panic!()
    };
    assert_eq!(c[0].mode, Some(ModeSpec::Literal(Mode(0o700))));
    assert_eq!(c[1].path, "~/.local/bin");

    assert_eq!(b.steps[3].display_label(), "Say hi");
    let Directive::Shell(s) = &b.steps[4].directive else {
        panic!()
    };
    assert_eq!(s.cmd, "make install");
    assert_eq!(s.cwd.as_deref(), Some("sub"));
    assert_eq!(
        s.env,
        [("A".into(), "1".into()), ("B".into(), "two".into())]
    );
    assert_eq!(s.interactive, Some(true));
    assert_eq!(s.unless.as_deref(), Some("command -v thing"));
    assert_eq!(b.steps[4].when.as_ref().unwrap().0.len(), 2);

    let Directive::Clean(c) = &b.steps[6].directive else {
        panic!()
    };
    assert!(c[0].recursive);
    let Directive::Clean(c) = &b.steps[5].directive else {
        panic!()
    };
    assert!(!c[0].recursive);
}

#[test]
fn name_defaults_to_directory() {
    let b = parse("steps: []").unwrap();
    assert_eq!(b.name, "demo");
}

#[test]
fn default_labels() {
    let b = parse(
        "steps:\n  - link: { ~/.a: a }\n  - shell: \"echo one\\necho two\"\n  - create: [~/a, ~/b, ~/c, ~/d]\n",
    )
    .unwrap();
    assert_eq!(b.steps[0].display_label(), "link ~/.a");
    assert_eq!(b.steps[1].display_label(), "shell: echo one ...");
    assert_eq!(b.steps[2].display_label(), "create ~/a, ~/b, ... (4 total)");
}

#[test]
fn yaml_1_2_scalars() {
    // `no` is a string in YAML 1.2, not a boolean.
    let b = parse("steps:\n  - shell: no\n    label: yes\n").unwrap();
    let Directive::Shell(s) = &b.steps[0].directive else {
        panic!()
    };
    assert_eq!(s.cmd, "no");
    assert_eq!(b.steps[0].label.as_deref(), Some("yes"));
}

#[test]
fn unknown_top_level_key_has_line() {
    let e = err("name: x\nstpes: []\n");
    assert_eq!(e.line, Some(2), "{e}");
    assert!(e.message.contains("unknown field `stpes`"), "{e}");
    assert!(
        e.to_string()
            .starts_with(&format!("{}:2:", file().display())),
        "{e}"
    );
}

#[test]
fn unknown_step_key_has_line() {
    let e = err("steps:\n  - shell: echo\n  - shell: echo\n    lable: typo\n");
    assert_eq!(e.line, Some(4), "{e}");
    assert!(e.message.contains("unknown field `lable`"), "{e}");
}

#[test]
fn unknown_directive_is_an_unknown_key() {
    let e = err("steps:\n  - packge: { brew: [fnm] }\n");
    assert_eq!(e.line, Some(2), "{e}");
    assert!(e.message.contains("unknown field `packge`"), "{e}");
}

#[test]
fn unknown_key_inside_long_form_has_line() {
    let e = err("steps:\n  - link:\n      ~/.x:\n        srcc: x\n");
    assert_eq!(e.line, Some(4), "{e}");
    assert!(e.message.contains("unknown field `srcc`"), "{e}");
    let e = err("steps:\n  - shell:\n      cmd: x\n      interative: true\n");
    assert_eq!(e.line, Some(4), "{e}");
    let e = err("steps:\n  - create:\n      ~/.x: { mod: \"0700\" }\n");
    assert_eq!(e.line, Some(3), "{e}");
    let e = err("steps:\n  - clean:\n      ~/: { recurse: true }\n");
    assert_eq!(e.line, Some(3), "{e}");
}

#[test]
fn step_with_no_directive() {
    let e = err("steps:\n  - shell: a\n  - label: nothing here\n");
    assert!(e.message.contains("steps[1]"), "{e}");
    assert!(e.message.contains("exactly one directive"), "{e}");
    assert!(e.message.contains("found none"), "{e}");
    assert_eq!(e.line, Some(3), "{e}");
}

#[test]
fn step_with_two_directives() {
    let e = err("steps:\n  - shell: a\n    link: { ~/.a: a }\n");
    assert!(e.message.contains("steps[0]"), "{e}");
    assert!(e.message.contains("found 2: link, shell"), "{e}");
    assert_eq!(e.line, Some(2), "{e}");
}

#[test]
fn modes() {
    for ok in ["\"0700\"", "\"644\"", "'0o600'", "\"1777\"", "0700"] {
        let b = parse(&format!(
            "steps:\n  - create:\n      ~/.x: {{ mode: {ok} }}\n"
        ))
        .unwrap_or_else(|e| panic!("{ok}: {e}"));
        let Directive::Create(c) = &b.steps[0].directive else {
            panic!()
        };
        assert!(c[0].mode.is_some(), "{ok}");
    }
    for bad in ["\"0800\"", "\"70\"", "\"rwx\"", "\"077777\"", "700", "true"] {
        let e = err(&format!(
            "steps:\n  - create:\n      ~/.x: {{ mode: {bad} }}\n"
        ));
        assert_eq!(e.line, Some(3), "{bad}: {e}");
    }
    let e = err("steps:\n  - create:\n      ~/.x: { mode: 700 }\n");
    assert!(e.message.contains("quoted octal string"), "{e}");
    assert!(e.message.contains("did you mean \"0700\"?"), "{e}");
    assert_eq!(Mode::parse("0755").unwrap().to_string(), "0755");
}

#[test]
fn unquoted_octal_and_hex_modes_get_a_correct_hint() {
    // YAML 1.2 reads `0o700` and `0x1C0` as the integer 448; the hint must not be "0448".
    for spelled in ["0o700", "0x1C0"] {
        let e = err(&format!(
            "steps:\n  - create:\n      ~/.x: {{ mode: {spelled} }}\n"
        ));
        assert!(e.message.contains("integer 448"), "{spelled}: {e}");
        assert!(
            e.message.contains("did you mean \"0700\"?"),
            "{spelled}: {e}"
        );
        assert!(!e.message.contains("0448"), "{spelled}: {e}");
    }
    // Too large to be a mode at all: no guess.
    let e = err("steps:\n  - create:\n      ~/.x: { mode: 99999 }\n");
    assert!(!e.message.contains("did you mean"), "{e}");
}

#[test]
fn unknown_fact_name() {
    let e = err("when: { oss: linux }\nsteps: []\n");
    assert_eq!(e.line, Some(1), "{e}");
    assert!(e.message.contains("unknown variant `oss`"), "{e}");
    let e = err("steps:\n  - shell: x\n    when: { hostname: a }\n");
    assert_eq!(e.line, Some(3), "{e}");
}

#[test]
fn negated_fact_values_parse_quoted() {
    let b = parse("steps:\n  - shell: x\n    when: { env: ['!container', '!wsl'], os: '!windows' }\n  - shell: y\n    when: { env: none }\n")
        .unwrap();
    assert_eq!(b.steps.len(), 2);
    let e = err("steps:\n  - shell: x\n    when: { env: '!docker' }\n");
    assert!(e.message.contains("unknown value `docker`"), "{e}");
}

#[test]
fn unknown_closed_fact_value() {
    let e = err("steps:\n  - shell: x\n    when: { os: mac }\n");
    assert!(
        e.message.contains("unknown value `mac` for fact `os`"),
        "{e}"
    );
    assert_eq!(e.line, Some(3), "{e}");
    // Open-ended facts accept any value.
    parse("steps:\n  - shell: x\n    when: { distro: fedora, arch: riscv64 }\n").unwrap();
}

#[test]
fn bad_on_conflict() {
    let e = err("steps:\n  - link:\n      ~/.x: { src: x, on_conflict: merge }\n");
    assert!(e.message.contains("unknown variant `merge`"), "{e}");
    assert_eq!(e.line, Some(3), "{e}");
}

#[test]
fn relative_destinations_are_rejected() {
    let e = err("steps:\n  - link:\n      .npmrc: npmrc\n");
    assert!(
        e.message.contains("`.npmrc` must be an absolute path"),
        "{e}"
    );
    assert_eq!(e.line, Some(2), "{e}");
    let e = err("steps:\n  - clean: [relative]\n");
    assert!(e.message.contains("clean"), "{e}");
}

#[test]
fn destinations_for_any_os_parse_on_every_os() {
    // One repository serves every machine: a Linux step's `/usr/local/go` must parse on
    // Windows, and a Windows step's `C:\Tools` on Linux (`when:` decides where they run; the
    // path is checked against this machine when such a step is planned). What the JSON schema
    // accepts, the parser does.
    let yaml = r#"
steps:
  - create: ['/opt/x', 'C:\Tools', 'D:/x', '\\srv\share\x', '~', '~/y']
  - template: { '/etc/x.conf': x.j2, 'C:\ProgramData\x.conf': x.j2 }
  - path: [/opt/late/bin, 'C:\Tools\bin']
  - download: { url: 'https://x/y.tgz', dest: /usr/local/go, extract: auto }
  - git: { repo: 'https://x/y', dest: 'C:\src\y' }
  - block: { file: /etc/hosts, content: x }
  - absent: ['C:\Temp\old', /tmp/old]
"#;
    parse(yaml).unwrap();
    let re = regex::Regex::new(crate::config::raw::TARGET_PATTERN).unwrap();
    for p in ["/opt/x", r"C:\Tools", "D:/x", r"\\srv\share\x", "~", "~/y"] {
        assert!(re.is_match(p), "{p}");
    }
    for p in ["rel", r"\rooted", "C:rel", "~user/x"] {
        let e = err(&format!("steps:\n  - create: ['{p}']\n"));
        assert!(e.message.contains("must be an absolute path"), "{p}: {e}");
    }
}

#[test]
fn duplicate_link_destination() {
    let e = err("steps:\n  - link:\n      ~/.a: a\n      ~/.a: b\n");
    assert!(e.message.contains("duplicate"), "{e}");
    assert!(e.line.is_some(), "{e}");
}

#[test]
fn wrong_types() {
    let e = err("steps: nope\n");
    assert_eq!(e.line, Some(1), "{e}");
    let e = err("steps:\n  - shell: [a, b]\n");
    assert!(e.line.is_some(), "{e}");
    let e = err("steps:\n  - create: ~/.x\n");
    assert!(e.line.is_some(), "{e}");
}

#[test]
fn invalid_yaml_has_line() {
    let e = err("steps:\n  - shell: a\n   bad: [\n");
    assert!(e.line.is_some(), "{e}");
}

#[test]
fn load_reads_files_and_finds_root() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    std::fs::write(repo.join("dot.yml"), "").unwrap();
    let dir = repo.join("bundles/zsh");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("bundle.yml"),
        "steps:\n  - link: { ~/.zshrc: zshrc }\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.join(".git/bundles")).unwrap();
    std::fs::write(repo.join(".git/bundles/bundle.yml"), "junk").unwrap();

    let b = load_bundle(&dir.join("bundle.yml")).unwrap();
    assert_eq!(b.name, "zsh");
    assert_eq!(b.root, repo);
    let lint_errors = lint(&b);
    assert_eq!(lint_errors.len(), 1);
    assert!(lint_errors[0].message.contains("`zshrc`"));
    std::fs::write(dir.join("zshrc"), "").unwrap();
    assert!(lint(&b).is_empty());

    let found = find_bundle_files(repo).unwrap();
    assert_eq!(found, [dir.join("bundle.yml")]);

    let e = load_bundle(&repo.join("missing.yml")).unwrap_err();
    assert!(e.message.contains("cannot read"), "{e}");
}

#[test]
fn steps_record_their_position() {
    let b = parse(
        "name: x\nsteps:\n  - shell: a\n    label: b\n  - link:\n      ~/.a: a\n  -   create: [~/x]\n  - {shell: z}\n",
    )
    .unwrap();
    let pos: Vec<Option<(usize, usize)>> = b
        .steps
        .iter()
        .map(|s| s.pos.map(|p| (p.line, p.column)))
        .collect();
    assert_eq!(
        pos,
        [Some((3, 5)), Some((5, 5)), Some((7, 7)), Some((8, 5))]
    );
    let e = ConfigError::at_step(&b, &b.steps[1], "boom");
    assert_eq!(e.line, Some(5));
    assert!(e.to_string().ends_with(":5:5: steps[1]: boom"), "{e}");
}

#[test]
fn templated_values_are_accepted_at_parse_time() {
    // Checked after rendering, at plan time.
    let b = parse(
        r#"
when: { os: "{{ target_os }}" }
steps:
  - link:
      "{{ config_dir }}/nvim": nvim
  - create:
      "{{ home }}/x": { mode: "{{ key_mode }}" }
  - clean: ["{{ home }}"]
  - shell: echo {{ greeting }}
    when: { role: "{{ role }}" }
"#,
    )
    .unwrap();
    let Directive::Create(c) = &b.steps[1].directive else {
        panic!()
    };
    assert_eq!(c[0].mode, Some(ModeSpec::Template("{{ key_mode }}".into())));
    // Literal values are still checked while parsing.
    let e = err("steps:\n  - create:\n      ~/x: { mode: \"0999\" }\n");
    assert!(e.message.contains("invalid file mode"), "{e}");
    let e = err("when: { os: mac }\nsteps: []\n");
    assert!(e.message.contains("unknown value `mac`"), "{e}");
}

#[test]
fn defaults_layer_under_explicit_values() {
    let b = parse(
        "steps:\n  - link:\n      ~/.a: a\n      ~/.b: { src: b, relink: true }\n  - shell: x\n",
    )
    .unwrap();
    let Directive::Link(links) = &b.steps[0].directive else {
        panic!()
    };
    let repo = Defaults {
        link: LinkDefaults {
            relink: Some(false),
            on_conflict: Some(OnConflict::Fail),
            ..LinkDefaults::default()
        },
        shell: ShellDefaults {
            interactive: Some(true),
            interpreter: Some(crate::config::ShellInterpreter::Bash),
            prelude: None,
        },
    };
    let inner = Defaults {
        link: LinkDefaults {
            on_conflict: Some(OnConflict::Overwrite),
            ..LinkDefaults::default()
        },
        ..Defaults::default()
    };
    let d = inner.or(&repo);
    // Unset in the entry: inherited.
    let a = links[0].resolve(&d);
    assert!(!a.relink && a.create);
    assert_eq!(a.on_conflict, OnConflict::Overwrite, "innermost layer wins");
    // Set explicitly (even to the built-in value): kept.
    assert!(links[1].resolve(&d).relink);
    let Directive::Shell(s) = &b.steps[1].directive else {
        panic!()
    };
    assert!(s.interactive(&d));
    assert!(!s.interactive(&Defaults::default()));
    use crate::config::ShellInterpreter;
    assert_eq!(s.interpreter(&d), Some(ShellInterpreter::Bash));
    assert_eq!(s.interpreter(&Defaults::default()), None);
}

#[test]
fn shell_interpreters() {
    use crate::config::ShellInterpreter;
    let b = parse(concat!(
        "defaults: { shell: { interpreter: pwsh } }\n",
        "steps:\n  - shell: { cmd: Get-ChildItem, interpreter: powershell }\n  - shell: ls\n",
    ))
    .unwrap();
    let Directive::Shell(s) = &b.steps[0].directive else {
        panic!()
    };
    assert_eq!(s.interpreter, Some(ShellInterpreter::Powershell));
    let d = b.manifest.defaults.clone();
    let Directive::Shell(s2) = &b.steps[1].directive else {
        panic!()
    };
    assert_eq!(s2.interpreter(&d), Some(ShellInterpreter::Pwsh));
    let e = err("steps:\n  - shell: { cmd: x, interpreter: fish }\n");
    assert!(e.message.contains("unknown variant `fish`"), "{e}");
    assert!(e.message.contains("pwsh"), "{e}");
}

#[test]
fn schema_matches_the_parser() {
    let schema = serde_json::to_value(json_schema()).unwrap();
    let step = &schema["$defs"]["Step"];
    // Exactly one directive: a oneOf of `required` alternatives, and no nullable directive.
    let one_of: Vec<String> = step["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["required"][0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(one_of, raw::DIRECTIVES);
    assert_eq!(step["additionalProperties"], serde_json::json!(false));
    for d in raw::DIRECTIVES {
        // The directive's own value must not admit null (`link: ~` is "no directive").
        let prop = &step["properties"][d];
        let null = serde_json::json!({ "type": "null" });
        let alts = prop["anyOf"].as_array().cloned().unwrap_or_default();
        assert!(!alts.contains(&null), "{d} is nullable: {prop}");
        assert!(prop["type"] != "null", "{d} is nullable: {prop}");
        let types = prop["type"].as_array().cloned().unwrap_or_default();
        assert!(!types.contains(&"null".into()), "{d} is nullable: {prop}");
    }
    // Target maps constrain their keys.
    assert_eq!(
        step["properties"]["link"]["propertyNames"]["pattern"],
        raw::TARGET_PATTERN
    );
    for d in ["create", "clean"] {
        let alts = step["properties"][d]["anyOf"].as_array().unwrap();
        assert_eq!(alts[0]["items"]["pattern"], raw::TARGET_PATTERN, "{d}");
        assert_eq!(
            alts[1]["propertyNames"]["pattern"],
            raw::TARGET_PATTERN,
            "{d}"
        );
    }
    assert!(schema.to_string().contains(shapes::MODE_PATTERN));
}

/// The schema's patterns, checked by hand against the parser (there is no regex engine among the
/// dependencies): every mode the pattern admits, the parser admits, and vice versa.
#[test]
fn mode_pattern_agrees_with_parser() {
    fn pattern_matches(s: &str) -> bool {
        // ^(0o)?0?[0-7]{3,4}$
        let d = s.strip_prefix("0o").unwrap_or(s);
        let octal = |t: &str| t.bytes().all(|b| (b'0'..=b'7').contains(&b));
        let plain = (3..=4).contains(&d.len()) && octal(d);
        let zero = d.len() == 5 && d.starts_with('0') && octal(d);
        plain || zero
    }
    for s in [
        "0700", "644", "0o600", "1777", "07777", "77777", "17777", "0800", "70", "rwx", "077777",
        "0o0777", "0o17777", "00777", "",
    ] {
        assert_eq!(pattern_matches(s), Mode::parse(s).is_ok(), "{s}");
    }
}

#[test]
fn schema_is_generated() {
    let schema = serde_json::to_value(json_schema()).unwrap();
    let text = schema.to_string();
    for key in [
        "steps",
        "link",
        "create",
        "shell",
        "clean",
        "on_conflict",
        "requires",
    ] {
        assert!(text.contains(key), "schema lacks {key}");
    }
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
}

/// Every directive is registered end to end: it parses, it is in the schema, and a step that
/// pairs it with another directive is rejected naming both (so the parser counts it).
#[test]
fn every_directive_parses_is_in_the_schema_and_counts_as_one() {
    let minimal = [
        ("link", "{ ~/.a: a }"),
        ("create", "[~/x]"),
        ("clean", "[~]"),
        ("package", "[fd]"),
        ("github_release", "a/b"),
        ("git", "{ repo: 'https://x/y', dest: ~/y }"),
        ("download", "{ url: 'https://x/y', dest: ~/y }"),
        ("template", "{ ~/x: a.j2 }"),
        ("copy", "{ ~/y: b }"),
        ("secret_file", "{ ~/z: s }"),
        ("block", "{ file: ~/.rc, content: x }"),
        ("path", "[~/bin]"),
        ("env", "{ A: b }"),
        (
            "macos_defaults",
            "{ domain: com.apple.dock, key: autohide, type: bool, value: true }",
        ),
        ("service", "x"),
        ("package_repo", "{ name: t, brew: a/b }"),
        ("user", "{ shell: zsh }"),
        ("absent", "[~/.old]"),
    ];
    let mut covered: Vec<&str> = minimal.iter().map(|(d, _)| *d).collect();
    covered.push("shell");
    covered.sort_unstable();
    let mut all = raw::DIRECTIVES.to_vec();
    all.sort_unstable();
    assert_eq!(covered, all, "a directive is missing from this test");

    let schema = serde_json::to_value(json_schema()).unwrap();
    let props = &schema["$defs"]["Step"]["properties"];
    for d in raw::DIRECTIVES {
        assert!(props[d].is_object(), "the schema lacks `{d}`");
    }
    for (d, value) in minimal {
        let b =
            parse(&format!("steps:\n  - {d}: {value}\n")).unwrap_or_else(|e| panic!("{d}: {e}"));
        assert_eq!(b.steps[0].directive.kind(), d);
        let e = err(&format!("steps:\n  - shell: 'true'\n    {d}: {value}\n"));
        assert_eq!(e.line, Some(2), "{d}: {e}");
        assert!(e.message.contains("found 2"), "{d}: {e}");
        assert!(e.message.contains(d), "{d}: {e}");
        let e = err(&format!("steps:\n  - {d}: {value}\n    typo: 1\n"));
        assert!(e.message.contains("unknown field `typo`"), "{d}: {e}");
    }
}

#[test]
fn a_step_level_unless_on_shell_is_the_commands_own() {
    let f = std::path::Path::new("/b/bundle.yml");
    let b = parse_bundle(
        "steps:\n  - shell: rm -f ~/.pyenv\n    unless: test ! -L ~/.pyenv\n",
        f,
    )
    .unwrap();
    let Directive::Shell(s) = &b.steps[0].directive else {
        panic!()
    };
    assert_eq!(s.unless.as_deref(), Some("test ! -L ~/.pyenv"));
    assert_eq!(b.steps[0].guard, None);
    let b = parse_bundle(
        "steps:\n  - package: { cask: [docker-desktop] }\n    unless: command -v docker\n",
        f,
    )
    .unwrap();
    assert_eq!(
        b.steps[0].guard,
        Some(Guard::Unless("command -v docker".into()))
    );
    for bad in [
        "steps:\n  - shell: { cmd: x, unless: a }\n    unless: b\n",
        "steps:\n  - create: [~/x]\n    unless: a\n    only_if: b\n",
    ] {
        assert!(parse_bundle(bad, f).is_err(), "{bad}");
    }
}
