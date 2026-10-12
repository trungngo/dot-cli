//! An in-memory provider for tests: `<name>:KEY` values and `<name>-file:path` files, with
//! switchable capabilities and call counters. Clones share state, so a test can keep a handle
//! while a [`crate::Registry`] owns the provider.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};

use crate::{Caps, Interactive, SecretProvider, SecretRef, SecretValue};

#[derive(Default)]
struct State {
    /// `(is_file, locator)` → value.
    values: Mutex<BTreeMap<(bool, String), SecretValue>>,
    unlocks: AtomicUsize,
    resolve_calls: AtomicUsize,
    unavailable: AtomicBool,
}

#[derive(Clone)]
pub struct MemoryProvider {
    name: &'static str,
    caps: Caps,
    fail_unlock: Option<String>,
    state: Arc<State>,
}

impl std::fmt::Debug for MemoryProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryProvider")
            .field("name", &self.name)
            .field("caps", &self.caps)
            .finish_non_exhaustive()
    }
}

impl MemoryProvider {
    /// Readable, writable, file-capable and batched; available; no unlock prompt.
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            caps: Caps {
                read: true,
                write: true,
                files: true,
                write_files: true,
                batch: true,
                needs_unlock: false,
            },
            fail_unlock: None,
            state: Arc::default(),
        }
    }

    pub fn with_value(self, key: &str, value: &str) -> Self {
        self.put(false, key, value.as_bytes().to_vec());
        self
    }

    pub fn with_file(self, path: &str, bytes: &[u8]) -> Self {
        self.put(true, path, bytes.to_vec());
        self
    }

    pub fn with_caps(mut self, caps: Caps) -> Self {
        self.caps = caps;
        self
    }

    pub fn unavailable(self) -> Self {
        self.set_available(false);
        self
    }

    /// Makes the provider (and every clone) available or not, e.g. as if a bundle had just
    /// installed its CLI.
    pub fn set_available(&self, yes: bool) {
        self.state.unavailable.store(!yes, Ordering::SeqCst);
    }

    pub fn failing_unlock(mut self, message: &str) -> Self {
        self.fail_unlock = Some(message.to_string());
        self
    }

    fn put(&self, file: bool, locator: &str, bytes: Vec<u8>) {
        let mut v = self.state.values.lock().unwrap_or_else(|e| e.into_inner());
        v.insert((file, locator.to_string()), SecretValue::new(bytes));
    }

    /// How many times `unlock` ran (across clones).
    pub fn unlocks(&self) -> usize {
        self.state.unlocks.load(Ordering::SeqCst)
    }

    /// How many times `resolve` ran (across clones).
    pub fn resolve_calls(&self) -> usize {
        self.state.resolve_calls.load(Ordering::SeqCst)
    }

    /// A boxed clone sharing this provider's state.
    pub fn boxed(&self) -> Box<dyn SecretProvider> {
        Box::new(self.clone())
    }

    fn check(&self, r: &SecretRef) -> Result<()> {
        if r.provider() != self.name {
            bail!("ref `{r}` is not for provider `{}`", self.name);
        }
        Ok(())
    }
}

impl SecretProvider for MemoryProvider {
    fn scheme(&self) -> &'static str {
        self.name
    }

    fn capabilities(&self) -> Caps {
        self.caps
    }

    fn available(&self) -> Result<bool> {
        Ok(!self.state.unavailable.load(Ordering::SeqCst))
    }

    fn unlock(&mut self, _tty: &mut dyn Interactive) -> Result<()> {
        self.state.unlocks.fetch_add(1, Ordering::SeqCst);
        match &self.fail_unlock {
            Some(msg) => bail!("{msg}"),
            None => Ok(()),
        }
    }

    fn resolve(&self, refs: &[SecretRef]) -> Result<Vec<Option<SecretValue>>> {
        self.state.resolve_calls.fetch_add(1, Ordering::SeqCst);
        if !self.caps.batch && refs.len() > 1 {
            bail!("provider `{}` does not batch", self.name);
        }
        let values = self.state.values.lock().unwrap_or_else(|e| e.into_inner());
        refs.iter()
            .map(|r| {
                self.check(r)?;
                Ok(values.get(&(r.is_file(), r.locator().to_string())).cloned())
            })
            .collect()
    }

    fn store(&self, r: &SecretRef, v: &SecretValue) -> Result<()> {
        self.check(r)?;
        if !self.caps.write {
            return Err(crate::SecretError::Unsupported(self.name).into());
        }
        self.put(r.is_file(), r.locator(), v.expose().to_vec());
        Ok(())
    }

    fn location(&self) -> Option<String> {
        Some(format!("memory provider `{}`", self.name))
    }
}
