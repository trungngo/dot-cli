use super::*;
use crate::config::{Bundle, ConfigError, Directive, json_schema, parse_bundle};

fn parse(yaml: &str) -> Result<Bundle, ConfigError> {
    let file = crate::paths::tests::abs("/nonexistent-dot-test/bundles/demo/bundle.yml");
    parse_bundle(yaml, &file)
}

fn err(yaml: &str) -> ConfigError {
    match parse(yaml) {
        Ok(b) => panic!("expected an error, parsed {b:#?}"),
        Err(e) => e,
    }
}

fn release(yaml: &str) -> GithubReleaseSpec {
    let b = parse(yaml).unwrap_or_else(|e| panic!("{e}"));
    match &b.steps[0].directive {
        Directive::GithubRelease(g) => g.clone(),
        d => panic!("{d:?}"),
    }
}

fn git(yaml: &str) -> GitSpec {
    let b = parse(yaml).unwrap_or_else(|e| panic!("{e}"));
    match &b.steps[0].directive {
        Directive::Git(g) => g.clone(),
        d => panic!("{d:?}"),
    }
}

#[test]
fn github_release_shorthand_is_the_repository() {
    let g = release("steps:\n  - github_release: junegunn/fzf\n");
    assert_eq!(g.repo, "junegunn/fzf");
    assert_eq!(g.asset, None);
    assert!(g.bin.is_empty());
    assert_eq!(g.to, None);
    assert_eq!(g.extract, Extract::Auto);
    assert!(!g.replace);
    let b = parse("steps:\n  - github_release: junegunn/fzf\n").unwrap();
    assert_eq!(b.steps[0].directive.kind(), "github_release");
    assert_eq!(b.steps[0].display_label(), "github_release junegunn/fzf");
}

#[test]
fn github_release_full_form() {
    let g = release(
        r#"steps:
  - github_release:
      repo: BurntSushi/ripgrep
      asset:
        linux: "*-unknown-linux-musl.tar.gz"
        macos/aarch64: "*-aarch64-apple-darwin.tar.gz"
      bin: [rg]
      to: ~/bin
      extract: tar.gz
      token: github_token
      replace: true
"#,
    );
    assert!(g.replace);
    assert_eq!(names(&g.bin), ["rg"]);
    assert_eq!(g.to.as_deref(), Some("~/bin"));
    assert_eq!(g.extract, Extract::TarGz);
    assert_eq!(g.token.as_deref(), Some("github_token"));
    let asset = g.asset.unwrap();
    assert_eq!(
        asset.for_platform("linux", "aarch64"),
        Ok("*-unknown-linux-musl.tar.gz")
    );
    assert_eq!(
        asset.for_platform("macos", "aarch64"),
        Ok("*-aarch64-apple-darwin.tar.gz")
    );
    let e = asset.for_platform("macos", "x86_64").unwrap_err();
    assert!(e.contains("no `asset:` pattern for macos/x86_64"), "{e}");
    assert!(e.contains("linux, macos/aarch64"), "{e}");

    let g = release("steps:\n  - github_release: { repo: a/b, asset: '*.zip', bin: b }\n");
    assert_eq!(g.asset, Some(AssetSpec::Glob("*.zip".into())));
    assert_eq!(names(&g.bin), ["b"]);
    assert_eq!(g.source, ReleaseSource::Github);
}

fn names(bin: &[BinEntry]) -> Vec<&str> {
    bin.iter().map(|b| b.name.as_str()).collect()
}

#[test]
fn github_release_from_gitlab_and_renamed_executables() {
    let g = release(
        "steps:\n  - github_release:\n      repo: gitlab-org/cli\n      source: gitlab\n      bin: { glab: 'glab*' }\n",
    );
    assert_eq!(g.source, ReleaseSource::Gitlab);
    assert_eq!(g.host, None);
    assert_eq!(
        g.bin,
        [BinEntry {
            name: "glab".into(),
            from: Some("glab*".into())
        }]
    );
    let g = release(
        "steps:\n  - github_release: { repo: a/sub/proj, source: gitlab, host: git.example.com:8443 }\n",
    );
    assert_eq!(g.repo, "a/sub/proj");
    assert_eq!(g.host.as_deref(), Some("git.example.com:8443"));
    for (yaml, want) in [
        (
            "steps:\n  - github_release: { repo: a/sub/proj }\n",
            "expected `owner/name`",
        ),
        (
            "steps:\n  - github_release: { repo: a/b, host: x.com }\n",
            "`host` is for `source: gitlab`",
        ),
        (
            "steps:\n  - github_release: { repo: a, source: gitlab }\n",
            "not a GitLab project",
        ),
        (
            "steps:\n  - github_release: { repo: a/b, source: gitlab, host: 'a b' }\n",
            "not a host name",
        ),
        (
            "steps:\n  - github_release: { repo: a/b, source: bitbucket }\n",
            "unknown variant `bitbucket`",
        ),
        (
            "steps:\n  - github_release: { repo: a/b, bin: { 'x/y': 'z' } }\n",
            "must be a plain file name",
        ),
    ] {
        let e = err(yaml);
        assert!(e.message.contains(want), "{yaml}: {e}");
    }
}

fn download(yaml: &str) -> DownloadSpec {
    let b = parse(yaml).unwrap_or_else(|e| panic!("{e}"));
    match &b.steps[0].directive {
        Directive::Download(d) => d.clone(),
        d => panic!("{d:?}"),
    }
}

#[test]
fn download_from_a_release_or_the_latest_version_and_trees() {
    let d = download(
        "steps:\n  - download:\n      url: 'https://go.dev/dl/{latest}.linux-amd64.tar.gz'\n      latest: https://go.dev/VERSION?m=text\n      dest: /usr/local/go\n      extract: tar.gz\n      strip: 1\n      replace: true\n      root: true\n      bin: [bin/go, bin/gofmt]\n",
    );
    assert_eq!(
        d.from,
        DownloadFrom::Url {
            url: "https://go.dev/dl/{latest}.linux-amd64.tar.gz".into(),
            latest: Some(LatestOpts {
                url: "https://go.dev/VERSION?m=text".into(),
                pattern: None
            })
        }
    );
    assert!(d.replace && d.root);
    assert_eq!(d.strip, 1);
    assert_eq!(names(&d.bin), ["bin/go", "bin/gofmt"]);
    let d = download(
        "steps:\n  - download:\n      release: { repo: gitlab-org/cli, source: gitlab, asset: 'glab_*_linux_amd64.deb' }\n      package: glab\n",
    );
    let DownloadFrom::Release(r) = &d.from else {
        panic!("{d:?}");
    };
    assert_eq!(r.source, ReleaseSource::Gitlab);
    assert_eq!(
        r.asset,
        Some(AssetSpec::Glob("glab_*_linux_amd64.deb".into()))
    );
    let d = download(
        "steps:\n  - download:\n      url: 'https://x/zed-{latest}.tar.gz'\n      latest: { url: 'https://x/v', pattern: 'v([0-9.]+)' }\n      dest: ~/.local/zed.app\n      extract: auto\n      bin: { zed: bin/zed }\n      bin_dir: ~/bin\n",
    )
    .from;
    assert!(matches!(
        d,
        DownloadFrom::Url {
            latest: Some(LatestOpts {
                pattern: Some(_),
                ..
            }),
            ..
        }
    ));

    for (yaml, want) in [
        ("{ dest: ~/y }", "exactly one of `url` and `release`"),
        (
            "{ url: 'https://x/y', release: { repo: a/b }, dest: ~/y }",
            "exactly one of `url` and `release`",
        ),
        ("{ url: 'https://x/{latest}', dest: ~/y }", "latest: <url>"),
        (
            "{ url: 'https://x/y', latest: 'https://x/v', dest: ~/y }",
            "has none",
        ),
        (
            "{ url: 'https://x/{latest}', latest: 'http://x/v', dest: ~/y }",
            "not an https:// URL",
        ),
        (
            "{ url: 'https://x/{latest}', latest: { url: 'https://x/v', pattern: '(' }, dest: ~/y }",
            "pattern `(`",
        ),
        (
            "{ release: { repo: a/b }, latest: 'https://x/v', dest: ~/y }",
            "`latest` is for `url`",
        ),
        (
            "{ url: 'https://x/y', dest: ~/y, replace: true }",
            "are for an unpacked archive",
        ),
        (
            "{ url: 'https://x/y', dest: ~/y, bin: [a] }",
            "are for an unpacked archive",
        ),
        (
            "{ url: 'https://x/y.tgz', dest: ~/y, extract: auto, bin_dir: ~/b }",
            "there is no `bin`",
        ),
        (
            "{ url: 'https://x/y.tgz', dest: ~/y, extract: auto, bin: ['../x'] }",
            "inside the unpacked tree",
        ),
        (
            "{ release: { repo: a/b/c }, dest: ~/y }",
            "expected `owner/name`",
        ),
    ] {
        let e = err(&format!("steps:\n  - download: {yaml}\n"));
        assert!(e.message.contains(want), "{yaml}: {e}");
    }
}

#[test]
fn github_release_errors_are_located() {
    let cases = [
        (
            "steps:\n  - github_release:\n      repo: a/b\n      sha256: 0123\n      tag: v1\n",
            "github_release: sha256 `0123` is not a SHA-256 digest",
            3,
        ),
        (
            "steps:\n  - github_release:\n      repo: a/b\n      sha256: \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n",
            "`sha256` needs `tag`",
            3,
        ),
        (
            "steps:\n  - github_release:\n      repo: a/b\n      tag: \"v1/../x\"\n",
            "tag `v1/../x` may only use",
            3,
        ),
        (
            "steps:\n  - github_release: fzf\n",
            "expected `owner/name`",
            2,
        ),
        (
            "steps:\n  - shell: a\n  - github_release:\n      repo: a/b\n      bni: [x]\n",
            "unknown field `bni`",
            5,
        ),
        (
            "steps:\n  - github_release:\n      repo: a/b\n      to: bin\n",
            "to `bin` must be an absolute path",
            3,
        ),
        (
            "steps:\n  - github_release:\n      repo: a/b\n      bin: [x/y]\n",
            "must be a plain file name",
            3,
        ),
        (
            "steps:\n  - github_release:\n      repo: a/b\n      asset: { darwin: '*' }\n",
            "unknown os `darwin`",
            4,
        ),
        (
            "steps:\n  - github_release:\n      repo: a/b\n      extract: tgz\n",
            "unknown variant `tgz`",
            4,
        ),
        (
            "steps:\n  - github_release:\n      repo: a/b\n      extract: raw\n      bin: [a, b]\n",
            "only one executable",
            3,
        ),
        (
            "steps:\n  - github_release: a/b\n    git: { repo: 'https://x/y', dest: ~/y }\n",
            "found 2: github_release, git",
            2,
        ),
    ];
    for (yaml, want, line) in cases {
        let e = err(yaml);
        assert!(e.message.contains(want), "{yaml}: {e}");
        assert_eq!(e.line, Some(line), "{yaml}: {e}");
    }
}

#[test]
fn templated_values_are_checked_later() {
    let g = release(
        "steps:\n  - github_release: { repo: '{{ owner }}/x', to: '{{ bin_dir }}', bin: ['{{ b }}'] }\n",
    );
    assert_eq!(g.repo, "{{ owner }}/x");
}

#[test]
fn git_parses_with_defaults() {
    let g = git("steps:\n  - git: { repo: 'https://github.com/a/b.git', dest: ~/src/b }\n");
    assert_eq!(g.repo, "https://github.com/a/b.git");
    assert_eq!(g.dest, "~/src/b");
    assert_eq!(g.branch, None);
    assert!(g.update);
    let g = git(
        "steps:\n  - git:\n      repo: file:///srv/b.git\n      dest: ~/b\n      branch: dev\n      update: false\n",
    );
    assert_eq!(g.branch.as_deref(), Some("dev"));
    assert!(!g.update);
    let b = parse("steps:\n  - git: { repo: 'https://x/y', dest: ~/y }\n").unwrap();
    assert_eq!(b.steps[0].display_label(), "git ~/y");
}

#[test]
fn git_errors_are_located() {
    let cases = [
        (
            "steps:\n  - git: { repo: 'git@github.com:a/b.git', dest: ~/b }\n",
            "is not an https:// URL",
        ),
        (
            "steps:\n  - git: { repo: 'ssh://github.com/a/b', dest: ~/b }\n",
            "is not an https:// URL",
        ),
        (
            "steps:\n  - git: { repo: 'https://x/y', dest: b }\n",
            "dest `b` must be an absolute path",
        ),
        (
            "steps:\n  - git: { repo: 'https://oauth2:glpat-123@gitlab.com/a/b', dest: ~/b }\n",
            "has credentials in it",
        ),
        (
            "steps:\n  - git: { repo: 'https://ghp_abc@github.com/a/b', dest: ~/b }\n",
            "has credentials in it",
        ),
        (
            "steps:\n  - git: { repo: 'https://x/y' }\n",
            "missing field `dest`",
        ),
        ("steps:\n  - git: https://x/y\n", "a mapping"),
        (
            "steps:\n  - git: { repo: 'https://x/y', dest: ~/y, branch: '-x' }\n",
            "not a branch name",
        ),
        (
            "steps:\n  - git: { repo: 'https://x/y', dest: ~/y, update: yes }\n",
            "invalid type",
        ),
    ];
    for (yaml, want) in cases {
        let e = err(yaml);
        assert!(e.message.contains(want), "{yaml}: {e}");
        assert_eq!(e.line, Some(2), "{yaml}: {e}");
        assert!(!e.message.contains("glpat-123") && !e.message.contains("ghp_abc"));
    }
    // An `@` in the path is not a credential.
    assert_eq!(
        git("steps:\n  - git: { repo: 'https://x/a@b', dest: ~/b }\n").repo,
        "https://x/a@b"
    );
}

#[test]
fn schema_describes_both_directives() {
    let schema = serde_json::to_value(json_schema()).unwrap();
    let step = &schema["$defs"]["Step"]["properties"];
    let gr = &step["github_release"];
    assert_eq!(gr["anyOf"][0]["pattern"], GITHUB_REPO_PATTERN);
    let text = schema.to_string();
    let json = |s: &str| serde_json::to_string(s).unwrap();
    for needle in [
        &json(GIT_URL_PATTERN),
        &json(PLATFORM_KEY_PATTERN),
        "\"tar.xz\"",
        "\"raw\"",
        "Fast-forward",
        "dot-backup",
    ] {
        assert!(text.contains(needle), "schema lacks {needle}");
    }
    let opts = &schema["$defs"]["GithubReleaseOpts"];
    assert_eq!(opts["additionalProperties"], serde_json::json!(false));
    assert_eq!(opts["required"], serde_json::json!(["repo"]));
    let git = &schema["$defs"]["GitOpts"];
    assert_eq!(git["additionalProperties"], serde_json::json!(false));
    assert_eq!(git["required"], serde_json::json!(["repo", "dest"]));
}

/// The schema patterns agree with the parser on representative values.
#[test]
fn patterns_agree_with_the_parser() {
    let re = |p: &str| regex::Regex::new(p).unwrap();
    let repo = re(GITHUB_REPO_PATTERN);
    for s in [
        "a/b",
        "junegunn/fzf",
        "a.b/c-d_e",
        "a",
        "a/b/c",
        "/b",
        "a/",
        "a b/c",
    ] {
        assert_eq!(repo.is_match(s), check_github_repo(s).is_ok(), "{s}");
    }
    let url = re(GIT_URL_PATTERN);
    for s in [
        "https://x/y",
        "file:///srv/x",
        "http://x",
        "git@x:y",
        "https://",
        "https://u:p@x/y",
        "https://tok@x",
        "https://x/a@b",
        "https://x?q=a@b",
    ] {
        assert_eq!(url.is_match(s), check_git_url(s).is_ok(), "{s}");
    }
    let key = re(PLATFORM_KEY_PATTERN);
    for s in [
        "linux",
        "macos/aarch64",
        "windows/x86_64",
        "darwin",
        "linux/",
        "linux/a-b",
    ] {
        assert_eq!(key.is_match(s), PlatformKey::parse(s).is_ok(), "{s}");
    }
}
