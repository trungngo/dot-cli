//! `absent`: paths that must not exist. A file, a symlink or junction (never what it points to)
//! or a whole directory at one of them is removed.
//!
//! Deleting is what `clean` is careful about, so the same guard applies, judged on where a path
//! really is (its parents' symlinks followed), at plan time: never the home directory or a
//! directory above it, never the repository, anything inside it or above it, never a filesystem
//! root. And nothing outside the home directory unless the entry says `root: true`; such a path
//! is removed through `sudo` when this user cannot remove it (the run asks for sudo up front).
//! On Windows, where whether a path may be removed cannot be told in advance, a `root: true`
//! path that exists needs an elevated terminal, checked up front like everything else that does.
//!
//! Links are removed through the run's [`LinkStrategy`], like `clean` does; a destination `link`
//! made as a fallback (a copy or junction, recorded in `links.json`) is forgotten once removed.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use dot_core::config::AbsentEntry;
use dot_core::paths::starts_with_fold;
use dot_core::{Action, Outcome, PlanCtx, RunCtx, Status};
use dot_platform::exec::{Cmd, System};
use dot_platform::link::LinkStrategy;

use crate::fileutil::{guard_as, located, on_disk_or_lexical, unwritable};
use crate::linkstate::LinkState;

pub fn plan(
    entries: &[AbsentEntry],
    ctx: &PlanCtx<'_>,
    sys: &System,
    links: &Arc<dyn LinkStrategy>,
    sysroot: Option<&Path>,
) -> Result<Vec<Box<dyn Action>>> {
    let mut actions: Vec<Box<dyn Action>> = Vec::new();
    for e in entries {
        let path = ctx.target(&e.path)?;
        // `DOT_SYSROOT`: a root-owned path is removed under the sysroot.
        let path = match sysroot {
            Some(root) if e.root && !inside(&path, ctx.home) => {
                dot_core::paths::rebase(root, &path)
            }
            _ => path,
        };
        let show = ctx.show(&path);
        guard_as("remove", &path, &show, ctx.home, &ctx.bundle.root)
            .map_err(|err| ctx.config_error(format!("absent: {err:#}")))?;
        if !e.root && !inside(&path, ctx.home) {
            return Err(ctx.config_error(format!(
                "absent: {show} is outside the home directory; add `root: true` to the entry to \
                 remove it anyway"
            )));
        }
        let pointing_into = e
            .pointing_into
            .as_deref()
            .map(|p| ctx.source(p))
            .transpose()?;
        actions.push(Box::new(AbsentAction {
            path,
            show,
            root: e.root,
            link_only: e.link_only,
            pointing_into,
            sys: sys.clone(),
            links: links.clone(),
            state: LinkState::in_dir(ctx.state_dir),
        }));
    }
    Ok(actions)
}

/// Whether `path` (where it is on disk: parents' symlinks followed, names as the filesystem
/// spells them) is inside `home`. Case is folded only on Windows: a macOS volume may be
/// case-sensitive, and this check lets a path through rather than stopping one.
fn inside(path: &Path, home: &Path) -> bool {
    let Some(at) = located(path) else {
        return false;
    };
    starts_with_fold(&at, &on_disk_or_lexical(home), cfg!(windows))
}

pub struct AbsentAction {
    pub path: PathBuf,
    show: String,
    root: bool,
    /// `only: link`: a real file or directory there is kept.
    link_only: bool,
    /// Only a link whose target is inside this directory.
    pointing_into: Option<PathBuf>,
    sys: System,
    links: Arc<dyn LinkStrategy>,
    state: LinkState,
}

/// What is at a path, for messages. Junctions count as symlinks.
fn what(meta: &fs::Metadata, path: &Path) -> String {
    let ft = meta.file_type();
    if ft.is_symlink() {
        match fs::read_link(path) {
            Ok(t) => format!("symlink -> {}", t.display()),
            Err(_) => "symlink".into(),
        }
    } else if ft.is_dir() {
        match fs::read_dir(path) {
            Ok(entries) => {
                let n = entries.count();
                format!("directory, {n} entr{}", if n == 1 { "y" } else { "ies" })
            }
            Err(_) => "directory".into(),
        }
    } else {
        "file".into()
    }
}

/// Whether removing a path needs privileges this process lacks: on Windows, any `root: true`
/// path (whether it may be removed cannot be told in advance); elsewhere one whose directory
/// this user may not change.
fn needs_privilege(root_entry: bool, elevated: bool, windows: bool, unwritable: bool) -> bool {
    root_entry && !elevated && (windows || unwritable)
}

impl AbsentAction {
    fn meta(&self) -> io::Result<Option<fs::Metadata>> {
        match fs::symlink_metadata(&self.path) {
            Ok(m) => Ok(Some(m)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Removing needs root (or, on Windows, an elevated terminal).
    fn privileged(&self) -> bool {
        needs_privilege(
            self.root,
            self.sys.root,
            cfg!(windows),
            unwritable(&self.path).is_some(),
        )
    }

    fn remove(&self, meta: &fs::Metadata) -> io::Result<()> {
        let ft = meta.file_type();
        if ft.is_symlink() {
            self.links.remove(&self.path)
        } else if ft.is_dir() {
            fs::remove_dir_all(&self.path)
        } else {
            fs::remove_file(&self.path)
        }
    }

    /// Why what is at the path is kept under `only: link` / `pointing_into` (`None`: it is
    /// removed).
    fn kept(&self, meta: &fs::Metadata) -> Option<String> {
        if !self.link_only {
            return None;
        }
        if !meta.file_type().is_symlink() {
            return Some(format!(
                "{} is a {}, not a link: kept (`only: link`)",
                self.show,
                if meta.is_dir() { "directory" } else { "file" }
            ));
        }
        let dir = self.pointing_into.as_ref()?;
        let target = fs::read_link(&self.path).ok()?;
        let target = if target.is_absolute() {
            target
        } else {
            self.path.parent().unwrap_or(Path::new("/")).join(target)
        };
        let target = dot_core::paths::normalize(&target);
        let dir = dot_core::paths::normalize(dir);
        (!starts_with_fold(&target, &dir, cfg!(any(windows, target_os = "macos")))).then(|| {
            format!(
                "{} links to {}, not into {}: kept",
                self.show,
                target.display(),
                dir.display()
            )
        })
    }

    /// Drops `link`'s record of a fallback made here, once it is gone.
    fn forget(&self) -> Result<()> {
        if self.state.get(&self.path).is_some() {
            self.state.forget(&self.path)?;
        }
        Ok(())
    }
}

impl Action for AbsentAction {
    fn describe(&self) -> String {
        match self.meta() {
            Ok(Some(m)) if self.kept(&m).is_some() => self.kept(&m).unwrap_or_default(),
            Ok(Some(m)) => {
                let mut d = format!("remove {} ({})", self.show, what(&m, &self.path));
                if self.privileged() {
                    d.push_str(if cfg!(windows) {
                        " [elevated terminal]"
                    } else {
                        " [as root]"
                    });
                }
                d
            }
            Ok(None) => format!("{} is absent", self.show),
            Err(e) => format!("remove {} (cannot inspect: {e})", self.show),
        }
    }

    fn status(&self) -> Result<Status> {
        Ok(
            match self
                .meta()
                .with_context(|| format!("inspecting {}", self.show))?
            {
                Some(m) if self.kept(&m).is_some() => Status::Satisfied,
                Some(_) => Status::Pending,
                None => Status::Satisfied,
            },
        )
    }

    fn apply(&self, ctx: &RunCtx) -> Result<Outcome> {
        let Some(meta) = self
            .meta()
            .with_context(|| format!("inspecting {}", self.show))?
        else {
            self.forget()?;
            return Ok(Outcome::Ok);
        };
        if self.kept(&meta).is_some() {
            return Ok(Outcome::Ok);
        }
        // Windows never gets here unelevated (the run stops up front), and elevated removes
        // directly.
        if self.privileged() && !cfg!(windows) {
            let cmd = self.sys.privileged(
                Cmd::new("rm")
                    .args(["-rf", "--"])
                    .arg(self.path.display().to_string()),
            );
            crate::syscmd::run(&self.sys, &cmd, ctx)?;
            self.forget()?;
            return Ok(Outcome::Changed);
        }
        match self.remove(&meta) {
            Ok(()) => {
                self.forget()?;
                Ok(Outcome::Changed)
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && !self.root => bail!(
                "removing {}: {e} (add `root: true` to remove it {})",
                self.show,
                if cfg!(windows) {
                    "from an elevated terminal"
                } else {
                    "through sudo"
                }
            ),
            Err(e) => Err(e).with_context(|| format!("removing {}", self.show)),
        }
    }

    fn needs_sudo(&self) -> bool {
        self.privileged()
    }
}

#[cfg(test)]
mod privilege_tests {
    use super::needs_privilege;

    #[test]
    fn who_needs_privileges() {
        // (root entry, elevated/root, windows, unwritable) -> needs
        let cases = [
            ((false, false, false, true), false),
            ((true, false, false, false), false),
            ((true, false, false, true), true),
            ((true, true, false, true), false),
            ((true, false, true, false), true),
            ((true, true, true, false), false),
            ((false, false, true, false), false),
        ];
        for ((root, elevated, windows, unwritable), want) in cases {
            assert_eq!(
                needs_privilege(root, elevated, windows, unwritable),
                want,
                "{root} {elevated} {windows} {unwritable}"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use dot_platform::exec::fake::FakeExec;

    use super::*;
    use crate::testutil::Fixture;
    use crate::testutil_sys::fake_builtins;

    fn plan_err(f: &Fixture, step: usize) -> String {
        match f.plan(step) {
            Ok(_) => panic!("expected an error"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn only_link_retires_a_link_and_keeps_anything_real() {
        let f = Fixture::new(concat!(
            "steps:\n",
            "  - absent:\n",
            "      ~/.oh-my-zsh: { only: link }\n",
            "      ~/.pyenv: { only: link }\n",
            "      ~/.glab.env: { pointing_into: ~/old-dotfiles }\n",
            "      ~/.other.env: { pointing_into: ~/old-dotfiles }\n",
        ));
        let h = f.home();
        fs::create_dir_all(h.join("old-dotfiles/vendor/omz")).unwrap();
        fs::write(h.join("old-dotfiles/glab.env"), "x").unwrap();
        fs::write(h.join("mine.env"), "x").unwrap();
        symlink(h.join("old-dotfiles/vendor/omz"), h.join(".oh-my-zsh")).unwrap();
        fs::create_dir_all(h.join(".pyenv/versions")).unwrap();
        symlink("old-dotfiles/glab.env", h.join(".glab.env")).unwrap();
        symlink(h.join("mine.env"), h.join(".other.env")).unwrap();
        let actions = f.plan(0).unwrap();
        let st: Vec<Status> = actions.iter().map(|a| a.status().unwrap()).collect();
        assert_eq!(
            st,
            [
                Status::Pending,
                Status::Satisfied,
                Status::Pending,
                Status::Satisfied
            ]
        );
        assert_eq!(
            actions[1].describe(),
            "~/.pyenv is a directory, not a link: kept (`only: link`)"
        );
        assert!(
            actions[3].describe().contains("not into") && actions[3].describe().ends_with(": kept"),
            "{}",
            actions[3].describe()
        );
        f.apply(&actions);
        assert!(fs::symlink_metadata(h.join(".oh-my-zsh")).is_err());
        assert!(
            h.join("old-dotfiles/vendor/omz").is_dir(),
            "the target stays"
        );
        assert!(h.join(".pyenv/versions").is_dir());
        assert!(fs::symlink_metadata(h.join(".glab.env")).is_err());
        assert!(h.join(".other.env").is_symlink());
    }

    #[test]
    fn removes_files_links_and_directories_then_is_satisfied() {
        let f =
            Fixture::new("steps:\n  - absent: [~/.old-file, ~/.old-dir, ~/.old-link, ~/never]\n");
        let h = f.home();
        fs::write(h.join(".old-file"), "x").unwrap();
        fs::create_dir_all(h.join(".old-dir/sub")).unwrap();
        fs::write(h.join(".old-dir/sub/a"), "x").unwrap();
        fs::write(h.join("target"), "keep").unwrap();
        symlink(h.join("target"), h.join(".old-link")).unwrap();
        let actions = f.plan(0).unwrap();
        let d: Vec<String> = actions.iter().map(|a| a.describe()).collect();
        assert_eq!(d[0], "remove ~/.old-file (file)");
        assert_eq!(d[1], "remove ~/.old-dir (directory, 1 entry)");
        assert!(d[2].starts_with("remove ~/.old-link (symlink -> "), "{d:?}");
        assert_eq!(d[3], "~/never is absent");
        let st: Vec<Status> = actions.iter().map(|a| a.status().unwrap()).collect();
        assert_eq!(
            st,
            [
                Status::Pending,
                Status::Pending,
                Status::Pending,
                Status::Satisfied
            ]
        );
        assert_eq!(
            f.apply(&actions),
            [
                Outcome::Changed,
                Outcome::Changed,
                Outcome::Changed,
                Outcome::Ok
            ]
        );
        assert!(fs::symlink_metadata(h.join(".old-link")).is_err());
        assert_eq!(
            fs::read_to_string(h.join("target")).unwrap(),
            "keep",
            "not followed"
        );
        assert!(!h.join(".old-dir").exists());
        assert!(
            f.plan(0)
                .unwrap()
                .iter()
                .all(|a| a.status().unwrap() == Status::Satisfied)
        );
    }

    #[test]
    fn refuses_home_the_repo_and_the_outside() {
        let f = Fixture::repo_in_home(
            "steps:\n  - absent: ['~']\n  - absent: [~/dotfiles]\n  - absent: [~/dotfiles/bundles/demo/x]\n  - absent: [/]\n  - absent: [/tmp/dot-absent-test-outside]\n",
        );
        assert!(
            plan_err(&f, 0).contains("home directory"),
            "{}",
            plan_err(&f, 0)
        );
        assert!(
            plan_err(&f, 1).contains("repository"),
            "{}",
            plan_err(&f, 1)
        );
        assert!(
            plan_err(&f, 2).contains("inside the repository"),
            "{}",
            plan_err(&f, 2)
        );
        assert!(
            plan_err(&f, 3).contains("filesystem root"),
            "{}",
            plan_err(&f, 3)
        );
        let e = plan_err(&f, 4);
        assert!(e.contains("outside the home directory"), "{e}");
        assert!(e.contains("root: true"), "{e}");
        assert!(e.contains("bundle.yml:6:"), "located: {e}");
    }

    #[test]
    fn another_spelling_of_the_repo_or_home_is_refused_where_case_does_not_matter() {
        let f = Fixture::repo_in_home("steps:\n  - absent: [~/DOTFILES]\n");
        if !crate::testutil::case_insensitive_fs(f.home()) {
            return;
        }
        let e = plan_err(&f, 0);
        assert!(e.contains("contains the repository"), "{e}");
        // The home directory spelled in upper case, from its parent, even with `root: true`.
        let home = f.home().canonicalize().unwrap();
        let upper = home
            .parent()
            .unwrap()
            .join(home.file_name().unwrap().to_string_lossy().to_uppercase());
        let g = f.with_bundle(&format!(
            "steps:\n  - absent: {{ \"{}\": {{ root: true }} }}\n",
            upper.display()
        ));
        let e = plan_err(&g, 0);
        assert!(e.contains("home directory"), "{e}");
        // A path under home in another case is still under home (no `root: true` needed).
        fs::create_dir_all(home.join(".cache/old")).unwrap();
        let g = f.with_bundle(&format!(
            "steps:\n  - absent: [\"{}\"]\n",
            upper.join(".CACHE/old").display()
        ));
        assert!(g.plan(0).is_ok(), "{:?}", g.plan(0).err());
        assert!(f.repo().exists());
    }

    #[test]
    fn a_link_into_the_repo_does_not_smuggle_a_removal() {
        let f = Fixture::repo_in_home("steps:\n  - absent: [~/.config/nvim/init.lua]\n");
        fs::create_dir_all(f.repo().join("nvim")).unwrap();
        fs::write(f.repo().join("nvim/init.lua"), "src").unwrap();
        fs::create_dir_all(f.home().join(".config")).unwrap();
        symlink(f.repo().join("nvim"), f.home().join(".config/nvim")).unwrap();
        assert!(plan_err(&f, 0).contains("inside the repository"));
        assert_eq!(
            fs::read_to_string(f.repo().join("nvim/init.lua")).unwrap(),
            "src"
        );
    }

    #[test]
    fn root_entries_go_through_sudo_when_not_writable() {
        let outside = tempfile::tempdir().unwrap();
        let locked = outside.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("docker.list"), "deb x").unwrap();
        let f = Fixture::new(&format!(
            "steps:\n  - absent: {{ {}: {{ root: true }} }}\n",
            locked.join("docker.list").display()
        ));
        let (b, fake): (_, std::sync::Arc<FakeExec>) = fake_builtins();
        let plan = |f: &Fixture| {
            let s = &f.bundle.steps[0];
            let refs = dot_core::SecretRefs::default();
            let ctx = f.context();
            let pctx = PlanCtx::new(&ctx, &f.bundle, s, &refs);
            dot_core::Planner::plan(&b, &s.directive, &pctx).unwrap()
        };
        // Writable: removed directly.
        let actions = plan(&f);
        assert!(!actions[0].needs_sudo());
        fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let actions = plan(&f);
        let is_root = unsafe { libc::geteuid() } == 0;
        if !is_root {
            assert!(actions[0].needs_sudo());
            assert!(
                actions[0].describe().ends_with("[as root]"),
                "{}",
                actions[0].describe()
            );
            assert_eq!(f.apply(&actions), [Outcome::Changed]);
            assert_eq!(
                fake.calls(),
                [format!(
                    "sudo -n rm -rf -- {}",
                    locked.join("docker.list").display()
                )]
            );
        }
        fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_fallback_copy_is_forgotten_once_removed_and_known_folders_resolve() {
        use crate::linkstate::{Fallback, LinkState, sha256_file};

        let f = Fixture::new("steps:\n  - absent: [\"{{ config_dir }}/old/settings.json\"]\n");
        let ctx = f.context();
        // The fixture is Linux with no XDG variables: `config_dir` is `~/.config`.
        let dest = f.home().join(".config/old/settings.json");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, "{}").unwrap();
        let state = LinkState::in_dir(&ctx.state_dir);
        state
            .record(Fallback {
                dest: dest.clone(),
                src: f.repo().join("settings.json"),
                kind: "copy".into(),
                sha256: Some(sha256_file(&dest).unwrap()),
            })
            .unwrap();
        let actions = f.plan_with(0, &ctx).unwrap();
        assert_eq!(actions[0].status().unwrap(), Status::Pending);
        assert_eq!(f.apply(&actions), [Outcome::Changed]);
        assert!(!dest.exists());
        assert!(state.get(&dest).is_none(), "the record is dropped");
    }
}
