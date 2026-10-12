//! Everything a repo-mode command needs, assembled from explicit inputs: the local config, the
//! facts, the repository, the profile, the variable layers and the bundle graph. Nothing here
//! reads the process environment or `$HOME` itself; the CLI passes them in, so tests use temp
//! dirs and fixed environments.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config::{Bundle, ConfigError, Defaults};
use crate::engine::Context;
use crate::facts::{Facts, Role};
use crate::graph::{Graph, GraphError, Reason, Resolution, Selection};
use crate::repo::{Located, Repo, find_repo};
use crate::vars::{EnvFile, Layer, Rank, Source, Vars, example_layer, load_env_files};

/// Keys in `local.env` that configure `dot` itself rather than define variables.
pub const CONTROL_KEYS: [&str; 3] = ["DOT_REPO", "DOT_PROFILE", "DOT_ROLE"];

/// What the command line and the process provide.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub home: PathBuf,
    pub cwd: PathBuf,
    /// Snapshot of the process environment.
    pub env: BTreeMap<String, String>,
    /// `--repo`
    pub repo: Option<PathBuf>,
    /// `--profile`
    pub profile: Option<String>,
    /// `--var KEY=value`, in order.
    pub vars: Vec<(String, String)>,
    /// `--env-file path`, in order.
    pub env_files: Vec<PathBuf>,
}

impl Inputs {
    fn env(&self, key: &str) -> Option<&str> {
        self.env
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// `$XDG_CONFIG_HOME`, else `~/.config` (on every OS, so one layout works everywhere).
    pub fn xdg_config_home(&self) -> PathBuf {
        match self.env("XDG_CONFIG_HOME") {
            Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
            _ => self.home.join(".config"),
        }
    }

    /// `dot`'s own config directory: `<config home>/dot`.
    pub fn config_dir(&self) -> PathBuf {
        self.xdg_config_home().join("dot")
    }

    /// `~/.config/dot/local.env`: machine-local values and `DOT_REPO` / `DOT_PROFILE` /
    /// `DOT_ROLE`; never committed.
    pub fn local_env(&self) -> PathBuf {
        self.config_dir().join("local.env")
    }

    /// The control keys of `local.env` (empty when there is none).
    pub fn local_control(&self) -> Result<BTreeMap<String, String>, ConfigError> {
        let file = EnvFile {
            path: self.local_env(),
            rank: Rank::Local,
            label: "local.env".into(),
            required: false,
        };
        let layers = load_env_files(&[file], &[])?;
        Ok(layers
            .into_iter()
            .flat_map(|l| l.values)
            .filter(|(k, _)| CONTROL_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k, crate::vars::value_string(&v)))
            .collect())
    }

    /// The role override: `DOT_ROLE` from the environment, else from `local.env`.
    pub fn role_override(&self, local: &BTreeMap<String, String>) -> Result<Option<Role>, String> {
        let raw = self.env("DOT_ROLE").map(|v| (v, "DOT_ROLE")).or_else(|| {
            local
                .get("DOT_ROLE")
                .map(|v| (v.as_str(), "DOT_ROLE in local.env"))
        });
        match raw {
            None => Ok(None),
            Some((v, from)) => Role::parse(v)
                .map(Some)
                .ok_or_else(|| format!("{from}: unknown role `{v}` (desktop or server)")),
        }
    }

    /// The variables every mode shares: facts (the known folders from this environment), the
    /// process environment and `--var`s. Repo mode adds the repo, profile and `.env` layers in
    /// between.
    pub fn base_vars(&self, facts: &Facts) -> Vars {
        let mut vars = Vars::from_facts(facts, &self.home);
        // Windows looks its environment up case-insensitively (`ProgramFiles`, `APPDATA`).
        let windows = facts.os == crate::facts::Os::Windows;
        // The platform's own Documents folder (Windows' known folder, redirected to OneDrive or
        // not; the XDG documents directory), for this machine only.
        let documents = (facts.os == crate::facts::Os::host())
            .then(dirs::document_dir)
            .flatten()
            .map(|d| d.display().to_string());
        vars.set_known_folders(facts, &self.home, |k| match self.env(k) {
            _ if k == crate::folders::DOCUMENTS_KEY => documents.clone(),
            Some(v) => Some(v.to_string()),
            None if windows => self
                .env
                .iter()
                .find(|(n, v)| n.eq_ignore_ascii_case(k) && !v.is_empty())
                .map(|(_, v)| v.clone()),
            None => None,
        });
        vars.set_environment(self.env.clone());
        if !self.vars.is_empty() {
            let mut cli = Layer::new(Source::cli());
            for (k, v) in &self.vars {
                cli.set(k.clone(), v.clone().into());
            }
            vars.push_layer(cli);
        }
        vars
    }
}

/// A variable to ask for before a run (see [`Workspace::var_prompts`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarPrompt {
    pub name: String,
    /// The declaration's `prompt:` text.
    pub question: String,
    /// Asked without echo, and written to `local.env` owner-only.
    pub sensitive: bool,
    /// `persist: local`: the answer is written to `local.env`, so it is asked once.
    pub persist: bool,
}

/// Why a repo-mode command could not start.
#[derive(Debug)]
pub enum OpenError {
    /// Problems in config files; all of them.
    Config(Vec<ConfigError>),
    Other(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Config(errs) => {
                let lines: Vec<String> = errs.iter().map(ToString::to_string).collect();
                f.write_str(&lines.join("\n"))
            }
            OpenError::Other(e) => f.write_str(e),
        }
    }
}

impl From<ConfigError> for OpenError {
    fn from(e: ConfigError) -> Self {
        OpenError::Config(vec![e])
    }
}

/// An opened repository, ready to plan.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub located: Located,
    pub repo: Repo,
    pub facts: Facts,
    /// Selected profile name and how it was chosen.
    pub profile: Option<(String, String)>,
    /// Every variable layer except the bundles' own.
    pub vars: Vars,
    /// `dot.yml`'s defaults under the profile's.
    pub defaults: Defaults,
    /// Capability -> provider.
    pub providers: BTreeMap<String, String>,
    /// The profile's secret provider, else `dot.yml`'s.
    pub secrets_provider: Option<String>,
}

impl Workspace {
    /// Finds and loads the repository, detects facts (`detect` gets the role override) and picks
    /// the profile.
    pub fn open(
        inputs: &Inputs,
        detect: impl FnOnce(Option<Role>) -> Facts,
    ) -> Result<Workspace, OpenError> {
        let local = inputs.local_control()?;
        let role = inputs.role_override(&local).map_err(OpenError::Other)?;
        let facts = detect(role);
        let located = find_repo(
            inputs.repo.as_deref(),
            inputs.env("DOT_REPO"),
            &inputs.cwd,
            local.get("DOT_REPO").map(String::as_str),
            &inputs.home,
        )
        .map_err(OpenError::Other)?;
        let repo = Repo::load(&located.root).map_err(OpenError::Config)?;

        // Profile `match:` values may use facts and dot.yml vars.
        let mut vars = inputs.base_vars(&facts);
        // `.env.example`: keys it lists are required, keys marked `# secret` sensitive.
        if let Some(example) = example_layer(&located.root.join(".env.example"))? {
            vars.push_layer(example);
        }
        vars.push_layer(repo.config.vars.clone());
        let pinned = inputs
            .env("DOT_PROFILE")
            .map(|p| (p.to_string(), "DOT_PROFILE".to_string()))
            .or_else(|| {
                local
                    .get("DOT_PROFILE")
                    .map(|p| (p.clone(), inputs.local_env().display().to_string()))
            });
        let choice = repo
            .select_profile(
                &facts,
                &vars,
                inputs.profile.as_deref(),
                pinned.as_ref().map(|(p, s)| (p.as_str(), s.as_str())),
            )
            .map_err(OpenError::Other)?
            .map(|c| (c.profile.clone(), c.how));

        let profile = choice.as_ref().map(|(p, _)| p);
        if let Some(p) = profile {
            vars.push_layer(p.vars.clone());
            vars.set_fact("profile", p.name.clone());
        }
        let root = &located.root;
        let mut files = vec![EnvFile {
            path: root.join(".env"),
            rank: Rank::DotEnv,
            label: ".env".into(),
            required: false,
        }];
        if let Some(p) = profile {
            files.push(EnvFile {
                path: root.join(format!(".env.{}", p.name)),
                rank: Rank::DotEnvProfile,
                label: format!(".env.{}", p.name),
                required: false,
            });
        }
        files.push(EnvFile {
            path: root.join(".env.local"),
            rank: Rank::Local,
            label: ".env.local".into(),
            required: false,
        });
        files.push(EnvFile {
            path: inputs.local_env(),
            rank: Rank::Local,
            label: local_env_label(&inputs.local_env(), &inputs.home),
            required: false,
        });
        for f in &inputs.env_files {
            let path = if f.is_absolute() {
                f.clone()
            } else {
                inputs.cwd.join(f)
            };
            files.push(EnvFile {
                label: format!("--env-file {}", f.display()),
                path,
                rank: Rank::Cli,
                required: true,
            });
        }
        for layer in load_env_files(&files, &CONTROL_KEYS)? {
            vars.push_layer(layer);
        }

        let defaults = match profile {
            Some(p) => p.defaults.or(&repo.config.defaults),
            None => repo.config.defaults.clone(),
        };
        let providers = repo.providers(profile);
        let secrets_provider = profile
            .and_then(|p| p.secrets_provider.clone())
            .or_else(|| repo.config.secrets_provider.clone());
        Ok(Workspace {
            located,
            facts,
            profile: choice.map(|(p, how)| (p.name, how)),
            vars,
            defaults,
            providers,
            secrets_provider,
            repo,
        })
    }

    pub fn profile_name(&self) -> Option<&str> {
        self.profile.as_ref().map(|(n, _)| n.as_str())
    }

    pub fn graph(&self) -> Graph<'_> {
        Graph {
            bundles: &self.repo.bundles,
            facts: &self.facts,
            vars: &self.vars,
            providers: &self.providers,
        }
    }

    /// What to resolve: `requested` bundles, else the profile's selection, else `dot.yml`'s,
    /// else every bundle; plus implicit bundles and dependencies unless `no_deps`.
    pub fn selection(&self, requested: &[String], no_deps: bool) -> Selection {
        let profile = self
            .profile_name()
            .and_then(|n| self.repo.config.profile(n));
        let roots: Vec<(String, Reason)> = if !requested.is_empty() {
            requested
                .iter()
                .map(|n| (n.clone(), Reason::Requested))
                .collect()
        } else if let Some(list) = profile.and_then(|p| p.bundles.as_ref()) {
            let by = format!("profile {}", profile.map(|p| p.name.as_str()).unwrap_or(""));
            list.iter()
                .map(|n| (n.clone(), Reason::Selected(by.clone())))
                .collect()
        } else if let Some(list) = &self.repo.config.bundles {
            list.iter()
                .map(|n| (n.clone(), Reason::Selected("dot.yml".into())))
                .collect()
        } else {
            self.repo
                .bundles
                .keys()
                .map(|n| (n.clone(), Reason::Selected("default (all bundles)".into())))
                .collect()
        };
        Selection {
            roots,
            implicit: !no_deps,
            expand: !no_deps,
        }
    }

    pub fn resolve(
        &self,
        requested: &[String],
        no_deps: bool,
    ) -> Result<Resolution, Vec<GraphError>> {
        self.graph().resolve(&self.selection(requested, no_deps))
    }

    /// The resolved bundles, in order.
    pub fn bundles(&self, res: &Resolution) -> Vec<Bundle> {
        res.order
            .iter()
            .filter_map(|r| self.repo.bundles.get(&r.name).cloned())
            .collect()
    }

    /// The engine context for this workspace.
    pub fn context(&self, home: &Path) -> Context {
        let mut ctx = Context::new(self.facts.clone(), home.to_path_buf());
        ctx.vars = self.vars.clone();
        ctx.defaults = self.defaults.clone();
        ctx.profile = self.profile_name().map(str::to_string);
        ctx.packages = self.repo.config.packages.clone();
        ctx
    }

    /// Up-front validation of the variables the resolved bundles see: every missing required
    /// variable (all names at once), and values that do not fit their declarations. Checked as
    /// each bundle sees them (its own `vars:` on top), or as the repository defines them when
    /// no bundle is resolved. A templated value that cannot be rendered is not an error here: a
    /// `dot.yml` value may refer to a variable only some bundles define, and a step that uses
    /// it fails with the reason when it is planned.
    pub fn check_vars(&self, res: &Resolution) -> Result<(), String> {
        self.check_vars_including(res, &[])
    }

    /// [`Workspace::check_vars`], counting `also_missing` as missing too (the prompted
    /// variables of a run that cannot ask, so every missing name is reported at once).
    pub fn check_vars_including(
        &self,
        res: &Resolution,
        also_missing: &[String],
    ) -> Result<(), String> {
        let mut missing: BTreeSet<String> = also_missing.iter().cloned().collect();
        let mut problems = BTreeSet::new();
        let mut check = |v: &Vars| {
            missing.extend(v.missing_required());
            problems.extend(v.value_problems().iter().cloned());
        };
        let bundles = self.bundles(res);
        if bundles.is_empty() {
            check(&self.vars);
        }
        for b in &bundles {
            check(&self.vars.with_bundle(b));
        }
        let mut out = Vec::new();
        if !missing.is_empty() {
            out.push(format!(
                "missing required variables: {} (set them in .env.local, {}, the environment, \
                 or with --var NAME=value)",
                missing.into_iter().collect::<Vec<_>>().join(", "),
                "~/.config/dot/local.env"
            ));
        }
        out.extend(problems);
        if out.is_empty() {
            Ok(())
        } else {
            Err(out.join("\n"))
        }
    }

    /// Variables to ask for before a run: declared with a `prompt:` and without a value, as
    /// `dot.yml` and the profile define them or as a resolved bundle sees them. In the order
    /// they are declared: `dot.yml`, the profile, then each bundle in plan order.
    pub fn var_prompts(&self, res: &Resolution) -> Vec<VarPrompt> {
        let mut out: Vec<VarPrompt> = Vec::new();
        let mut collect = |v: &Vars| {
            for (name, question, sensitive, persist) in v.unanswered_prompts() {
                if !out.iter().any(|p| p.name == name) {
                    out.push(VarPrompt {
                        name,
                        question,
                        sensitive,
                        persist,
                    });
                }
            }
        };
        collect(&self.vars);
        for b in self.bundles(res) {
            collect(&self.vars.with_bundle(&b));
        }
        out
    }

    /// For `dot plan`: the variables `dot apply` will ask for render as placeholders, and the
    /// steps that use them are planned as unknown instead of failing with "undefined variable".
    pub fn defer_prompts(&mut self, res: &Resolution) {
        let names: Vec<String> = self.var_prompts(res).into_iter().map(|p| p.name).collect();
        if !names.is_empty() {
            self.vars.defer_prompts(&names);
        }
    }

    /// Sets answered variables for this run (above `.env` files, below the process
    /// environment and `--var`, like `local.env`).
    pub fn answer(&mut self, answers: &[(String, String)]) {
        if answers.is_empty() {
            return;
        }
        let mut layer = Layer::new(Source::new(Rank::Local, "answered at prompt"));
        for (k, v) in answers {
            layer.set(k.clone(), serde_json::Value::String(v.clone()));
        }
        self.vars.push_layer(layer);
    }

    /// Why `value` does not fit variable `name` (its type or pattern) as the resolved bundles
    /// see it; empty when it does. Messages never show a sensitive value.
    pub fn answer_problems(&self, res: &Resolution, name: &str, value: &str) -> Vec<String> {
        let mut ws = self.clone();
        ws.answer(&[(name.to_string(), value.to_string())]);
        let prefix = format!("variable `{name}` ");
        let mut problems = BTreeSet::new();
        let mut check = |v: &Vars| {
            problems.extend(
                v.value_problems()
                    .iter()
                    .filter(|p| p.starts_with(&prefix))
                    .cloned(),
            );
        };
        check(&ws.vars);
        for b in ws.bundles(res) {
            check(&ws.vars.with_bundle(&b));
        }
        problems.into_iter().collect()
    }

    /// Values of sensitive variables the run can see, for the redactor.
    pub fn sensitive_values(&self, res: &Resolution) -> Vec<String> {
        let mut out = BTreeSet::new();
        let mut collect = |v: &Vars| {
            for (_, val) in v.iter() {
                let s = val.as_string();
                if val.sensitive && !s.is_empty() {
                    out.insert(s);
                }
            }
        };
        collect(&self.vars);
        for b in self.bundles(res) {
            collect(&self.vars.with_bundle(&b));
        }
        out.into_iter().collect()
    }
}

/// How `local.env` is named as a variable source (a label, never stored): `~/` and the rest in
/// `/` form on every OS, as the docs write it (`~/.config/dot/local.env`); outside home, the
/// path as this OS spells it.
fn local_env_label(path: &Path, home: &Path) -> String {
    let label = crate::paths::display(path, home);
    if cfg!(windows) && label.starts_with("~/") {
        label.replace('\\', "/")
    } else {
        label
    }
}

/// `dot check` for the repository's `.env` files: `.env`, `.env.<profile>` for every profile,
/// `.env.local` and `.env.example` must parse, and their values must fit the declarations in
/// `dot.yml`, the profiles and the bundles (type and pattern). Never shows a value that is (or
/// looks) sensitive. `facts` and `home` only give templates something to render with.
pub fn check_env_files(root: &Path, repo: &Repo, facts: &Facts, home: &Path) -> Vec<ConfigError> {
    let mut errors = Vec::new();
    let example = match example_layer(&root.join(".env.example")) {
        Ok(l) => l,
        Err(e) => {
            errors.push(e);
            None
        }
    };
    let file = |name: String, rank: Rank| EnvFile {
        path: root.join(&name),
        rank,
        label: name,
        required: false,
    };
    let mut profiles: Vec<Option<&crate::repo::Profile>> = vec![None];
    profiles.extend(repo.config.profiles.iter().map(Some));
    let mut problems = BTreeSet::new();
    let mut parse_errors: BTreeMap<String, ConfigError> = BTreeMap::new();
    for profile in profiles {
        let mut files = vec![file(".env".into(), Rank::DotEnv)];
        if let Some(p) = profile {
            files.push(file(format!(".env.{}", p.name), Rank::DotEnvProfile));
        }
        files.push(file(".env.local".into(), Rank::Local));
        // Each file alone first, so every broken one is reported (not just the first).
        let mut good = Vec::new();
        for f in files {
            match load_env_files(std::slice::from_ref(&f), &CONTROL_KEYS) {
                Ok(_) => good.push(f),
                Err(e) => {
                    parse_errors.insert(e.to_string(), e);
                }
            }
        }
        let Ok(env_layers) = load_env_files(&good, &CONTROL_KEYS) else {
            continue;
        };
        let mut vars = Vars::from_facts(facts, home);
        if let Some(l) = &example {
            vars.push_layer(l.clone());
        }
        vars.push_layer(repo.config.vars.clone());
        if let Some(p) = profile {
            vars.push_layer(p.vars.clone());
            vars.set_fact("profile", p.name.clone());
        }
        for l in env_layers {
            vars.push_layer(l);
        }
        problems.extend(vars.value_problems().iter().cloned());
        for b in repo.bundles.values() {
            problems.extend(vars.with_bundle(b).value_problems().iter().cloned());
        }
    }
    errors.extend(parse_errors.into_values());
    errors.extend(problems.into_iter().map(|p| ConfigError::new(root, p)));
    errors
}

#[cfg(test)]
mod tests;
