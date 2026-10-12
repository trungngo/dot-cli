//! Provider name → factory. A run builds the registry once, then the [`crate::Resolver`]
//! instantiates only the providers the plan's refs actually use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::SecretProvider;
use crate::config::ProvidersConfig;
use crate::envfile::EnvfileProvider;
use crate::git::Git;
use crate::reference::{ENVFILE, LASTPASS, ONEPASSWORD};

/// Builds a provider from the `providers:` section of `secrets.yml`.
pub type ProviderFactory =
    Box<dyn Fn(&ProvidersConfig) -> Result<Box<dyn SecretProvider>> + Send + Sync>;

/// Where providers find the user's files. Injected so tests never touch the real home.
#[derive(Debug, Clone)]
pub struct ProviderEnv {
    pub home: PathBuf,
    /// `~/.config/dot`: default home of `secrets.env` and `files/`.
    pub config_dir: PathBuf,
    pub git: Git,
    /// Where vendor CLIs (`lpass`) are looked up: `None` is `$PATH`. Tests point it at a temp
    /// directory so they never reach a real CLI.
    pub search_path: Option<std::ffi::OsString>,
}

impl ProviderEnv {
    /// The current user's home.
    pub fn system() -> Result<Self> {
        let home = dirs::home_dir().context("cannot determine the home directory")?;
        Ok(Self::with_home(home))
    }

    /// A home directory other than the user's (tests, `--host`).
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            config_dir: home.join(".config").join("dot"),
            home,
            git: Git::system(),
            search_path: None,
        }
    }

    pub fn with_git(mut self, git: Git) -> Self {
        self.git = git;
        self
    }

    /// Looks vendor CLIs up in `path` instead of `$PATH`.
    pub fn with_search_path(mut self, path: impl Into<std::ffi::OsString>) -> Self {
        self.search_path = Some(path.into());
        self
    }

    /// Expands a configured path: `~` / `~/…` is the home directory, a relative path is
    /// relative to [`ProviderEnv::config_dir`], an absolute path is kept.
    pub fn expand(&self, path: &str) -> PathBuf {
        if path == "~" {
            return self.home.clone();
        }
        if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
            return self.home.join(rest);
        }
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.config_dir.join(p)
        }
    }
}

#[derive(Default)]
pub struct Registry {
    factories: BTreeMap<String, ProviderFactory>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.factories.keys()).finish()
    }
}

impl Registry {
    /// No providers.
    pub fn new() -> Self {
        Self::default()
    }

    /// `envfile`, plus `lastpass` / `1password` when their features are enabled.
    pub fn builtin(env: &ProviderEnv) -> Self {
        let mut reg = Self::new();
        let e = env.clone();
        reg.register(ENVFILE, move |cfg: &ProvidersConfig| {
            let p = EnvfileProvider::from_config(cfg.envfile.as_ref(), &e);
            Ok(Box::new(p) as Box<dyn SecretProvider>)
        });
        #[cfg(feature = "secrets-lastpass")]
        {
            let search_path = env.search_path.clone();
            reg.register(LASTPASS, move |cfg: &ProvidersConfig| {
                let p = crate::lastpass::LastpassProvider::new(
                    cfg.lastpass.clone().unwrap_or_default(),
                )
                .with_search_path(search_path.clone());
                Ok(Box::new(p) as Box<dyn SecretProvider>)
            });
        }
        #[cfg(feature = "secrets-1password")]
        reg.register(ONEPASSWORD, |cfg: &ProvidersConfig| {
            let p = crate::onepassword::OnePasswordProvider::new(
                cfg.onepassword.clone().unwrap_or_default(),
            );
            Ok(Box::new(p) as Box<dyn SecretProvider>)
        });
        reg
    }

    /// Adds (or replaces) the factory for `name`.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        factory: impl Fn(&ProvidersConfig) -> Result<Box<dyn SecretProvider>> + Send + Sync + 'static,
    ) -> &mut Self {
        self.factories.insert(name.into(), Box::new(factory));
        self
    }

    pub fn contains(&self, name: &str) -> bool {
        self.factories.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.factories.keys().map(String::as_str)
    }

    /// Instantiates `name`; `None` if no such provider is registered.
    pub fn create(
        &self,
        name: &str,
        config: &ProvidersConfig,
    ) -> Option<Result<Box<dyn SecretProvider>>> {
        self.factories.get(name).map(|f| f(config))
    }
}

/// Why a provider is not registered, phrased to follow "provider `x`, which …".
pub(crate) fn missing_provider_hint(name: &str) -> String {
    match name {
        LASTPASS => "is not compiled into this build (cargo feature `secrets-lastpass`)".into(),
        ONEPASSWORD => "is not compiled into this build (cargo feature `secrets-1password`)".into(),
        _ => "is not a known provider".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_paths() {
        let env = ProviderEnv::with_home("/h");
        assert_eq!(env.config_dir, Path::new("/h/.config/dot"));
        assert_eq!(env.expand("~"), Path::new("/h"));
        assert_eq!(env.expand("~/x/secrets.env"), Path::new("/h/x/secrets.env"));
        assert_eq!(env.expand("rel.env"), Path::new("/h/.config/dot/rel.env"));
        #[cfg(unix)]
        assert_eq!(env.expand("/abs/s.env"), Path::new("/abs/s.env"));
    }

    #[test]
    fn builtin_registers_features() {
        let reg = Registry::builtin(&ProviderEnv::with_home("/h"));
        assert!(reg.contains("envfile"));
        assert_eq!(reg.contains("lastpass"), cfg!(feature = "secrets-lastpass"));
        assert_eq!(
            reg.contains("1password"),
            cfg!(feature = "secrets-1password")
        );
        assert!(!reg.contains("mock"));
        assert!(reg.create("nope", &ProvidersConfig::default()).is_none());
        assert!(missing_provider_hint("lastpass").contains("secrets-lastpass"));
    }
}
