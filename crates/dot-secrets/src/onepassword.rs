//! `1password`: will wrap the `op` CLI. Not implemented yet: until it is, the provider is
//! registered so `op://` refs parse and validate, but it reports itself unavailable, which sends
//! every `op://` ref to its fallback.

use anyhow::Result;

use crate::config::OnePasswordConfig;
use crate::reference::ONEPASSWORD;
use crate::{Caps, Interactive, SecretError, SecretProvider, SecretRef, SecretValue};

#[derive(Debug, Clone)]
pub struct OnePasswordProvider {
    config: OnePasswordConfig,
}

impl OnePasswordProvider {
    pub fn new(config: OnePasswordConfig) -> Self {
        Self { config }
    }

    pub fn account(&self) -> Option<&str> {
        self.config.account.as_deref()
    }

    fn unavailable() -> anyhow::Error {
        SecretError::Unavailable {
            provider: ONEPASSWORD,
            reason: "the 1password provider is not implemented yet".into(),
        }
        .into()
    }
}

impl SecretProvider for OnePasswordProvider {
    fn scheme(&self) -> &'static str {
        ONEPASSWORD
    }

    /// What the finished provider will offer, so callers can plan around it.
    fn capabilities(&self) -> Caps {
        Caps {
            read: true,
            write: true,
            files: false,
            write_files: false,
            batch: true,
            needs_unlock: true,
        }
    }

    fn available(&self) -> Result<bool> {
        Ok(false)
    }

    fn unlock(&mut self, _tty: &mut dyn Interactive) -> Result<()> {
        Err(Self::unavailable())
    }

    fn resolve(&self, _refs: &[SecretRef]) -> Result<Vec<Option<SecretValue>>> {
        Err(Self::unavailable())
    }

    fn store(&self, _r: &SecretRef, _v: &SecretValue) -> Result<()> {
        Err(Self::unavailable())
    }
}
