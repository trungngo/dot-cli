//! `dot adopt <path> [--bundle b]`: take an existing file (or directory) under the home
//! directory into the repository and link it back.
//!
//! The file moves to `<bundle dir>/<name>` (its name without a leading dot), a `link` step is
//! appended to the bundle's `bundle.yml` as text (comments and formatting kept; the file is
//! never re-serialised) or a new bundle is created, and the original path becomes a link to the
//! repository copy. Everything is checked before anything moves; a failure half-way puts the
//! file and `bundle.yml` back. The link is recorded in the state store as `dot`'s, with the
//! file's copy as what `dot remove` puts back.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use dot_core::config;
use dot_core::paths::{display, normalize};
use dot_core::repo::{Repo, find_repo};
use dot_core::state::Restore;
use dot_core::{Resource, StateStore, Undo};
use dot_directives::fileutil::within;
use dot_directives::linkstate::{Fallback, LinkState, sha256_file};
use dot_platform::link::{LinkStrategy, SystemLinks};

use crate::AdoptArgs;
use crate::commands::{config_exit, inputs};
use crate::render::logfile::default_state_dir;

/// A YAML plain scalar when that reads back as the same string, else single-quoted.
pub fn yaml_scalar(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let not_a_string =
        matches!(lower.as_str(), "true" | "false" | "null" | "~") || s.parse::<f64>().is_ok();
    let plain = !s.is_empty()
        && !not_a_string
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./~+-@".contains(c))
        && (!s.starts_with(['-', '@', '~']) || s.starts_with("~/"));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// `text` (a `bundle.yml`) with a step linking `dest` to `src` added at the end of its
/// `steps:` list, as text: every other line, comment and blank stays as it is. A block list is
/// extended with the indentation of its items; `steps: []` becomes a block list; a file
/// without `steps:` gets one. A non-empty flow list (`steps: [...]`) is refused: it would have
/// to be rewritten.
pub fn append_link_step(text: &str, dest: &str, src: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let item = |indent: usize| {
        let pad = " ".repeat(indent);
        format!(
            "{pad}- link:\n{pad}    {}: {}\n",
            yaml_scalar(dest),
            yaml_scalar(src)
        )
    };
    let is_top = |l: &str| !l.starts_with([' ', '\t']) && !l.trim().is_empty();
    let steps_at = lines.iter().position(|l| {
        let t = l.trim_end();
        is_top(l) && (t == "steps:" || t.starts_with("steps:") && t[6..].starts_with([' ', '#']))
    });
    let mut out = String::with_capacity(text.len() + 64);
    let Some(at) = steps_at else {
        out.push_str(text);
        if !text.is_empty() && !text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("steps:\n");
        out.push_str(&item(2));
        return Ok(out);
    };
    let rest = lines[at].trim_end()["steps:".len()..].trim();
    let value = rest.split(" #").next().unwrap_or("").trim();
    if value.starts_with("[]") {
        for l in &lines[..at] {
            out.push_str(l);
        }
        out.push_str("steps:\n");
        out.push_str(&item(2));
        for l in &lines[at + 1..] {
            out.push_str(l);
        }
        return Ok(out);
    }
    if !value.is_empty() && !value.starts_with('#') {
        return Err(format!(
            "`steps:` is written as `{value}` (a flow list): add the step by hand"
        ));
    }
    // The block: every line after `steps:` up to the next top-level key (a `- ` item at column
    // 0 still belongs to it).
    let mut end = at + 1;
    let mut indent: Option<usize> = None;
    let mut last = at;
    while end < lines.len() {
        let l = lines[end];
        let t = l.trim_start();
        if is_top(l) && !t.starts_with('-') && !t.starts_with('#') {
            break;
        }
        if !t.trim().is_empty() && !(is_top(l) && t.starts_with('#')) {
            if indent.is_none() && t.starts_with('-') {
                indent = Some(l.len() - t.len());
            }
            last = end;
        }
        end += 1;
    }
    let indent = indent.unwrap_or(2);
    for l in &lines[..=last] {
        out.push_str(l);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&item(indent));
    for l in &lines[last + 1..] {
        out.push_str(l);
    }
    Ok(out)
}

/// A bundle name from a file name: `.zshrc` -> `zshrc`, `init.lua` -> `init`.
fn bundle_name_for(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    let name = name.trim_start_matches('.');
    let stem = name.split('.').next().unwrap_or(name);
    let clean: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    (!clean.is_empty()).then_some(clean)
}

/// `~/...` with forward slashes, for `bundle.yml`.
fn home_relative(path: &Path, home: &Path) -> Option<String> {
    let rel = path.strip_prefix(home).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(format!("~/{}", parts.join("/")))
}

pub fn adopt(a: AdoptArgs) -> Result<ExitCode> {
    match run(&a) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(e) => {
            eprintln!("error: {e:#}");
            Ok(config_exit())
        }
    }
}

fn run(a: &AdoptArgs) -> Result<()> {
    let inputs = inputs(&a.repo)?;
    let home = normalize(&inputs.home);
    let local = inputs.local_control().map_err(|e| anyhow::anyhow!("{e}"))?;
    let located = find_repo(
        inputs.repo.as_deref(),
        inputs.env.get("DOT_REPO").map(String::as_str),
        &inputs.cwd,
        local.get("DOT_REPO").map(String::as_str),
        &inputs.home,
    )
    .map_err(anyhow::Error::msg)?;
    let repo = Repo::load(&located.root).map_err(|errs| {
        let first = errs.first().map(ToString::to_string).unwrap_or_default();
        anyhow::anyhow!("{first} (fix the repository first: `dot check`)")
    })?;
    let root = normalize(&located.root);

    // What to adopt.
    let given = dot_core::paths::expand_tilde(&a.path.to_string_lossy(), &inputs.home);
    let path = normalize(&inputs.cwd.join(given));
    let shown = display(&path, &home);
    let meta = fs::symlink_metadata(&path).with_context(|| format!("cannot adopt {shown}"))?;
    if meta.file_type().is_symlink() {
        bail!("{shown} is a link already");
    }
    if !meta.is_file() && !meta.is_dir() {
        bail!("{shown} is not a regular file or directory");
    }
    if !within(&path, &home) || path == home {
        bail!("{shown} is not inside the home directory");
    }
    if within(&path, &root) || within(&root, &path) {
        bail!("{shown} is inside the repository, or contains it");
    }
    let dest_key = home_relative(&path, &home)
        .with_context(|| format!("{shown} is not inside the home directory"))?;

    // Where it goes.
    let bundle_name = match &a.bundle {
        Some(b) => b.clone(),
        None => bundle_name_for(&path)
            .with_context(|| format!("cannot name a bundle after {shown}: use --bundle"))?,
    };
    let existing = repo.bundles.get(&bundle_name);
    let (bundle_dir, bundle_file) = match existing {
        Some(b) => (b.dir.clone(), b.file.clone()),
        None => {
            let dir = repo.config.bundles_dir.join(&bundle_name);
            (dir.clone(), dir.join("bundle.yml"))
        }
    };
    let file_name = match &a.name {
        Some(n) => n.clone(),
        None => {
            let n = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            n.trim_start_matches('.').to_string()
        }
    };
    if file_name.is_empty() || file_name.contains(['/', '\\']) {
        bail!("`{file_name}` cannot be a file name in the bundle: use --name");
    }
    let target = bundle_dir.join(&file_name);
    if fs::symlink_metadata(&target).is_ok() {
        bail!(
            "{} already exists in bundle {bundle_name}: use --name",
            target.display()
        );
    }
    let old_text = match existing {
        Some(_) => Some(
            fs::read_to_string(&bundle_file)
                .with_context(|| format!("reading {}", bundle_file.display()))?,
        ),
        None => None,
    };
    let new_text = match &old_text {
        Some(t) => append_link_step(t, &dest_key, &file_name)
            .map_err(|e| anyhow::anyhow!("{}: {e}", bundle_file.display()))?,
        None => append_link_step("", &dest_key, &file_name).map_err(anyhow::Error::msg)?,
    };
    let parsed = config::parse_bundle(&new_text, &bundle_file)
        .map_err(|e| anyhow::anyhow!("the edited bundle would not load: {e}"))?;
    let links = SystemLinks::new();
    let kind = links
        .kind_for(meta.is_dir())
        .map_err(|e| anyhow::anyhow!("cannot link {shown} back: {e}"))?;

    let what = if meta.is_dir() { "directory" } else { "file" };
    let bundle_note = if existing.is_some() {
        format!("add a link step to {}", display(&bundle_file, &home))
    } else {
        format!(
            "create bundle {bundle_name} ({})",
            display(&bundle_file, &home)
        )
    };
    if a.dry_run {
        println!("would move {what} {shown} to {}", display(&target, &home));
        println!("would {bundle_note}:");
        let label = display(&bundle_file, &home);
        let old = old_text.as_deref().unwrap_or("");
        print!(
            "{}",
            dot_core::textdiff::unified(old, &new_text, &label, &label, 1)
        );
        println!("would link {shown} -> {}", display(&target, &home));
        return Ok(());
    }

    let state_dir = default_state_dir(&inputs.home).context("no state directory")?;
    let store = StateStore::at(&state_dir);
    let _lock = store.run_lock(&format!("adopt {shown}"))?;

    // Move, write the manifest, link; undo what was done when a later part fails.
    fs::create_dir_all(&bundle_dir)
        .with_context(|| format!("creating {}", bundle_dir.display()))?;
    move_path(&path, &target)?;
    let write = fs::write(&bundle_file, &new_text)
        .with_context(|| format!("writing {}", bundle_file.display()));
    if let Err(e) = write {
        let _ = move_path(&target, &path);
        return Err(e);
    }
    if let Err(e) = links.create(kind, &target, &path) {
        match &old_text {
            Some(t) => {
                let _ = fs::write(&bundle_file, t);
            }
            None => {
                let _ = fs::remove_file(&bundle_file);
            }
        }
        let _ = move_path(&target, &path);
        bail!("linking {shown} -> {}: {e}", target.display());
    }
    if kind.is_fallback() {
        LinkState {
            store: store.clone(),
        }
        .record(Fallback {
            dest: path.clone(),
            src: target.clone(),
            kind: kind.to_string(),
            sha256: sha256_file(&path).ok().filter(|_| !meta.is_dir()),
        })?;
    }
    let resource = Resource::new("link", dest_key.clone()).with_undo(Undo::Unlink {
        path: path.clone(),
        target: target.clone(),
    });
    store.update(|s| {
        s.record(
            &resource.key(),
            &parsed.name,
            resource.kind,
            resource.undo.clone(),
            true,
            Some(Restore::Copy {
                src: target.clone(),
            }),
        )
    })?;
    println!("moved {shown} to {}", display(&target, &home));
    println!(
        "{}",
        bundle_note
            .replacen("add", "added", 1)
            .replacen("create ", "created ", 1)
    );
    println!("linked {shown} -> {}", display(&target, &home));
    Ok(())
}

/// Renames, or copies and removes across filesystems.
fn move_path(from: &Path, to: &Path) -> Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    dot_directives::undo::copy_tree(from, to)?;
    let r = if fs::symlink_metadata(from).is_ok_and(|m| m.is_dir()) {
        fs::remove_dir_all(from)
    } else {
        fs::remove_file(from)
    };
    r.with_context(|| format!("removing {} after copying it", from.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_are_quoted_only_when_needed() {
        assert_eq!(yaml_scalar("~/.zshrc"), "~/.zshrc");
        assert_eq!(yaml_scalar("zshrc"), "zshrc");
        assert_eq!(yaml_scalar("~/My Files/x"), "'~/My Files/x'");
        assert_eq!(yaml_scalar("it's"), "'it''s'");
        assert_eq!(yaml_scalar("~"), "'~'");
        assert_eq!(yaml_scalar("-x"), "'-x'");
        assert_eq!(yaml_scalar("a:b"), "'a:b'");
        assert_eq!(yaml_scalar("true"), "'true'");
        assert_eq!(yaml_scalar("1.5"), "'1.5'");
    }

    #[test]
    fn a_step_is_appended_to_the_block_list_keeping_everything_else() {
        let text = "# my shell\nname: zsh\nsteps:\n  # links first\n  - link:\n      ~/.zshenv: zshenv   # env\n\n  - shell: echo hi\n\n# trailing note\n";
        let out = append_link_step(text, "~/.zshrc", "zshrc").unwrap();
        assert_eq!(
            out,
            "# my shell\nname: zsh\nsteps:\n  # links first\n  - link:\n      ~/.zshenv: zshenv   # env\n\n  - shell: echo hi\n  - link:\n      ~/.zshrc: zshrc\n\n# trailing note\n"
        );
        // Keys after `steps:` stay after it; items at column 0 keep their indentation.
        let text = "steps:\n- shell: a\nwhen: { os: linux }\n";
        assert_eq!(
            append_link_step(text, "~/.x", "x").unwrap(),
            "steps:\n- shell: a\n- link:\n    ~/.x: x\nwhen: { os: linux }\n"
        );
        // No trailing newline.
        assert_eq!(
            append_link_step("steps:\n  - shell: a", "~/.x", "x").unwrap(),
            "steps:\n  - shell: a\n  - link:\n      ~/.x: x\n"
        );
    }

    #[test]
    fn empty_missing_and_flow_lists() {
        assert_eq!(
            append_link_step("name: a\nsteps: []  # none yet\nwhen: {}\n", "~/.x", "x").unwrap(),
            "name: a\nsteps:\n  - link:\n      ~/.x: x\nwhen: {}\n"
        );
        assert_eq!(
            append_link_step("name: a", "~/.x", "x").unwrap(),
            "name: a\nsteps:\n  - link:\n      ~/.x: x\n"
        );
        assert_eq!(
            append_link_step("", "~/.x", "x").unwrap(),
            "steps:\n  - link:\n      ~/.x: x\n"
        );
        let e = append_link_step("steps: [{ shell: a }]\n", "~/.x", "x").unwrap_err();
        assert!(e.contains("flow list"), "{e}");
    }

    #[test]
    fn appended_steps_parse() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("bundles/zsh/bundle.yml");
        let text = "name: zsh\nsteps:\n  - shell: echo\n";
        let out = append_link_step(text, "~/My Files/.rc", "it's").unwrap();
        let b = config::parse_bundle(&out, &file).unwrap();
        assert_eq!(b.steps.len(), 2);
        match &b.steps[1].directive {
            config::Directive::Link(entries) => {
                assert_eq!(entries[0].dest, "~/My Files/.rc");
                assert_eq!(entries[0].src, "it's");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bundle_names_come_from_file_names() {
        assert_eq!(
            bundle_name_for(Path::new("/h/.zshrc")).as_deref(),
            Some("zshrc")
        );
        assert_eq!(
            bundle_name_for(Path::new("/h/.config/nvim/init.lua")).as_deref(),
            Some("init")
        );
        assert_eq!(
            bundle_name_for(Path::new("/h/My Tool")).as_deref(),
            Some("my-tool")
        );
    }
}
