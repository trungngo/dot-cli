//! Secrets are requested by logical name; `secrets.yml` maps each name to a provider reference.
//! Directives have no other way to read one, so switching provider never touches a bundle.
//!
//! The pieces, in the order a run uses them:
//!
//! - [`SecretsConfig`]: the parsed `secrets.yml` (logical name → [`SecretRef`], fallback, kind).
//! - [`Registry`]: provider name → factory. `envfile` is always built in; `lastpass` and
//!   `1password` are registered behind the `secrets-lastpass` / `secrets-1password` features.
//! - [`Resolver`]: given the names a plan needs, groups their refs by provider, unlocks each
//!   provider once, resolves each group in one batch, applies fallbacks, and either returns every
//!   value or fails naming every secret it could not resolve.
//! - [`SecretValue`]: the resolved bytes; zeroed on drop, no `Display`, `Debug` prints `***`.
//!
//! [`contract::run`] is the behaviour every provider must pass; it runs here against
//! [`EnvfileProvider`] and [`mock::MemoryProvider`].

#[cfg_attr(not(windows), allow(dead_code))]
mod acl;
mod config;
pub mod contract;
mod dotenv;
mod envfile;
mod exec;
mod fsutil;
mod git;
#[cfg(feature = "secrets-lastpass")]
mod lastpass;
pub mod mock;
#[cfg(feature = "secrets-1password")]
mod onepassword;
mod persist;
pub mod private_file;
mod reference;
mod registry;
mod resolver;
mod scan;
pub mod transfer;

use std::fmt;

use anyhow::Result;
use zeroize::Zeroize;

pub use config::{
    EnvfileConfig, LastpassConfig, OnePasswordConfig, ProvidersConfig, RefSpec, SecretEntry,
    SecretKind, SecretSpec, SecretsConfig, SecretsConfigError, json_schema,
};
pub use envfile::{CheckReport, EnvfileProvider, Expected};
pub use exec::find_program;
pub use git::{Git, GitPlacement};
#[cfg(feature = "secrets-lastpass")]
pub use lastpass::LastpassProvider;
#[cfg(feature = "secrets-1password")]
pub use onepassword::OnePasswordProvider;
pub use persist::{merge_env, persist_env};
pub use reference::{RefError, SecretRef};
pub use registry::{ProviderEnv, ProviderFactory, Registry};
pub use resolver::{Failure, Partial, Problem, ResolveError, Resolved, Resolver};
pub use scan::{Leak, example_secret_keys, read_example_secret_keys, scan_committed_env_files};
pub use transfer::{Merge, merge};

/// A resolved secret. Deliberately has no `Display`, and `Debug` prints only `***`. The bytes
/// are zeroed when the value is dropped (clones included).
#[derive(Clone)]
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The value as text, when it is UTF-8 (file secrets may not be).
    pub fn expose_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for SecretValue {
    fn from(s: String) -> Self {
        Self(s.into_bytes())
    }
}

impl From<Vec<u8>> for SecretValue {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl zeroize::ZeroizeOnDrop for SecretValue {}

/// What a provider can do. The resolver consults `files` before sending a file ref, `batch` to
/// decide between one `resolve` call and one per ref, and `needs_unlock` to pause the console
/// around `unlock`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub read: bool,
    /// `store` accepts value refs.
    pub write: bool,
    /// File refs can be resolved.
    pub files: bool,
    /// `store` accepts file refs too (`lpass` cannot add attachments, for one).
    pub write_files: bool,
    pub batch: bool,
    pub needs_unlock: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("provider `{0}` does not support this operation")]
    Unsupported(&'static str),
    #[error("provider `{provider}` is not available: {reason}")]
    Unavailable {
        provider: &'static str,
        reason: String,
    },
    #[error("ref `{reference}` belongs to provider `{actual}`, not `{expected}`")]
    WrongProvider {
        reference: String,
        expected: &'static str,
        actual: String,
    },
}

/// The terminal as seen by a provider's `unlock` (master password, MFA prompt) and, later, by
/// prompting for a missing secret. The CLI implements it on top of its renderer: `suspend` /
/// `resume` bracket a prompt with `NeedsTty` / `TtyReleased` so the live view steps aside.
pub trait Interactive {
    /// Whether a person can answer prompts (`false` under `--non-interactive` or without a TTY).
    /// A provider that would have to prompt must fail instead.
    fn is_interactive(&self) -> bool;

    /// The console is about to be used directly (e.g. `lpass login` on the real TTY).
    fn suspend(&mut self, _reason: &str) {}

    /// The direct use announced by [`Interactive::suspend`] is over.
    fn resume(&mut self) {}

    /// Reads one line without echo.
    fn read_secret(&mut self, _prompt: &str) -> Result<SecretValue> {
        anyhow::bail!("cannot prompt: not running interactively")
    }
}

/// No terminal: nothing can be asked.
#[derive(Debug, Clone, Copy, Default)]
pub struct NonInteractive;

impl Interactive for NonInteractive {
    fn is_interactive(&self) -> bool {
        false
    }
}

/// One ref's answer from [`SecretProvider::resolve_each`]: its value (`None`: the backend has
/// none), or why this ref alone could not be read.
pub type RefResult = std::result::Result<Option<SecretValue>, String>;

/// A secret backend. Built in: `envfile` (now), `lastpass` and `1password` (feature-gated, wrap
/// the vendor CLIs). `scheme` is the provider name that [`SecretRef::provider`] returns for the
/// refs it serves (`envfile` serves both `envfile:` and `envfile-file:` refs).
pub trait SecretProvider: Send + Sync {
    fn scheme(&self) -> &'static str;
    fn capabilities(&self) -> Caps;
    /// Whether the backend can be used on this machine (e.g. its CLI is on `PATH`). A provider
    /// that is not available has its refs treated as missing, so fallbacks apply.
    fn available(&self) -> Result<bool>;
    /// Called once per run, before any `resolve`, with the live console paused when
    /// [`Caps::needs_unlock`] is set. Providers also run their guardrails here.
    fn unlock(&mut self, tty: &mut dyn Interactive) -> Result<()>;
    /// Resolves a batch so CLI-backed providers can use a single invocation. Returns one entry
    /// per ref, in order: `None` when the backend has no value for it. `Err` is reserved for the
    /// provider itself failing (it is reported for every ref in the batch, and never falls back).
    fn resolve(&self, refs: &[SecretRef]) -> Result<Vec<Option<SecretValue>>>;
    /// [`SecretProvider::resolve`] with a problem that concerns one ref only (a file secret
    /// others can read) reported for that ref alone, as its entry's `Err` (a message naming
    /// paths, never values), so the rest of the batch still resolves. The outer `Err` is the
    /// provider failing as a whole. Default: `resolve`, where every problem is provider-wide.
    fn resolve_each(&self, refs: &[SecretRef]) -> Result<Vec<RefResult>> {
        Ok(self.resolve(refs)?.into_iter().map(Ok).collect())
    }
    /// Writes a value. Only providers with [`Caps::write`] implement it.
    fn store(&self, _r: &SecretRef, _v: &SecretValue) -> Result<()> {
        Err(SecretError::Unsupported(self.scheme()).into())
    }
    /// Where values live, for messages (`~/.config/dot/secrets.env`). Never a value.
    fn location(&self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests;
