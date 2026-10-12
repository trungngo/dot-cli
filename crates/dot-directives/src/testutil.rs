//! Test fixture: a temp "repo" holding one bundle and a separate temp "home". Nothing here
//! touches the real home directory.

use std::path::{Path, PathBuf};

use anyhow::Result;
use dot_core::event::Recorder;
use dot_core::facts::{Os, Role};
use dot_core::{
    Action, Bundle, EventSink, Facts, Outcome, PlanCtx, Planner, RunCtx, SecretRefs, StepId,
};
use tempfile::TempDir;

use crate::Builtins;

pub struct Fixture {
    _tmp: Vec<TempDir>,
    root: PathBuf,
    home: PathBuf,
    pub bundle: Bundle,
    pub recorder: Recorder,
}

/// Whether the filesystem holding `dir` ignores case (APFS and NTFS by default): tests of the
/// guards against `~/DOTFILES` run only there.
pub fn case_insensitive_fs(dir: &Path) -> bool {
    let probe = tempfile::tempdir_in(dir).unwrap();
    std::fs::create_dir(probe.path().join("Probe")).unwrap();
    probe.path().join("PROBE").exists()
}

pub fn facts() -> Facts {
    Facts::new(Os::Linux, "x86_64", Role::Desktop)
}

impl Fixture {
    /// Writes `yaml` as `<repo>/bundles/demo/bundle.yml` (with a `dot.yml` at the repo root).
    pub fn new(yaml: &str) -> Self {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let (root, home_path) = (repo.path().to_path_buf(), home.path().to_path_buf());
        Self::build(yaml, vec![repo, home], root, home_path)
    }

    /// Like `new`, but the repo lives at `<home>/dotfiles`.
    pub fn repo_in_home(yaml: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("dotfiles");
        let home_path = home.path().to_path_buf();
        Self::build(yaml, vec![home], root, home_path)
    }

    /// Another bundle file in the same repo directory and home. The returned fixture does not
    /// own the temp dirs: `self` must outlive it.
    pub fn with_bundle(&self, yaml: &str) -> Fixture {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let name = format!("bundle-{}.yml", N.fetch_add(1, Ordering::SeqCst));
        Self::build_file(
            yaml,
            Vec::new(),
            self.root.clone(),
            self.home.clone(),
            &name,
        )
    }

    fn build(yaml: &str, tmp: Vec<TempDir>, root: PathBuf, home: PathBuf) -> Self {
        Self::build_file(yaml, tmp, root, home, "bundle.yml")
    }

    fn build_file(yaml: &str, tmp: Vec<TempDir>, root: PathBuf, home: PathBuf, name: &str) -> Self {
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("dot.yml"), "").unwrap();
        let dir = root.join("bundles/demo");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, yaml).unwrap();
        let bundle = dot_core::config::load_bundle(&file).unwrap();
        Self {
            _tmp: tmp,
            root,
            home,
            bundle,
            recorder: Recorder::default(),
        }
    }

    /// The bundle's directory; sources are relative to it.
    pub fn repo(&self) -> std::path::PathBuf {
        self.root.join("bundles/demo")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn write_repo(&self, rel: &str, contents: &str) {
        let p = self.repo().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    pub fn context(&self) -> dot_core::Context {
        dot_core::Context::new(facts(), self.home().to_path_buf())
    }

    pub fn plan(&self, step: usize) -> Result<Vec<Box<dyn Action>>> {
        self.plan_with(step, &self.context())
    }

    pub fn plan_with(&self, step: usize, ctx: &dot_core::Context) -> Result<Vec<Box<dyn Action>>> {
        self.plan_by(step, ctx, &Builtins::default())
    }

    /// Plans with `builtins` (e.g. forced link capabilities).
    pub fn plan_by(
        &self,
        step: usize,
        ctx: &dot_core::Context,
        builtins: &Builtins,
    ) -> Result<Vec<Box<dyn Action>>> {
        let step_ref = &self.bundle.steps[step];
        let secrets = SecretRefs::default();
        let pctx = PlanCtx::new(ctx, &self.bundle, step_ref, &secrets);
        builtins.plan(&step_ref.directive, &pctx)
    }

    pub fn sink(&self) -> EventSink {
        EventSink::new().with(Box::new(self.recorder.clone()))
    }

    pub fn apply_one(&self, action: &dyn Action) -> Result<Outcome> {
        let ctx = RunCtx::new(&self.sink(), StepId { bundle: 0, step: 0 });
        action.apply(&ctx)
    }

    /// Applies every action, panicking on the first error.
    pub fn apply(&self, actions: &[Box<dyn Action>]) -> Vec<Outcome> {
        actions
            .iter()
            .map(|a| {
                self.apply_one(a.as_ref())
                    .unwrap_or_else(|e| panic!("{}: {e:#}", a.describe()))
            })
            .collect()
    }

    pub fn apply_err(&self, action: &dyn Action) -> String {
        match self.apply_one(action) {
            Ok(o) => panic!("expected failure, got {o:?}"),
            Err(e) => format!("{e:#}"),
        }
    }
}
