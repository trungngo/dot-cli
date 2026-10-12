//! The seam between templates and secret providers. Planning collects the logical names a run
//! needs, and how each is used ([`SecretUse`]); the prepare stage resolves them (in
//! `dot-secrets`) into something implementing [`SecretLookup`], which [`crate::Context::secrets`]
//! hands to the steps as they are applied. Nothing in `dot-core` ever reads a provider.
//!
//! Values are bytes: a secret used as a file (`secret_file:`) may be binary; only a
//! secret used through `secret()` in a template has to be UTF-8 text.
//!
//! Zeroing is best-effort. The cache's own copies, and every [`SecretBytes`] handed out, are
//! zeroed on drop; but a value rendered into a template passes through minijinja's (and the
//! standard library's) own buffers, which are freed without zeroing. Shell steps keep secrets
//! out of the script text altogether (they reach the child through its environment, see
//! [`crate::engine::PlanCtx::render_command`]), so nothing a local user can list with `ps` holds
//! one.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::RwLock;

/// How a step uses a secret.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecretUse {
    /// Through `secret("name")` in a template: must be UTF-8 text.
    pub text: bool,
    /// As file content (`secret_file:`): any bytes.
    pub file: bool,
}

impl SecretUse {
    pub const TEXT: SecretUse = SecretUse {
        text: true,
        file: false,
    };
    pub const FILE: SecretUse = SecretUse {
        text: false,
        file: true,
    };

    /// Both uses.
    pub fn merge(self, other: SecretUse) -> SecretUse {
        SecretUse {
            text: self.text || other.text,
            file: self.file || other.file,
        }
    }
}

/// A resolved secret's bytes, zeroed on drop. No `Display`; `Debug` prints `***`.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The value as text, when it is UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(crate::redact::MASK)
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.iter_mut().for_each(|b| *b = 0);
        // Keeps the writes from being optimised away as dead stores.
        std::hint::black_box(&mut self.0);
    }
}

/// Resolved secret values by logical name.
pub trait SecretLookup: Send + Sync {
    /// The value, or `None` when the name was not resolved.
    fn lookup(&self, name: &str) -> Option<SecretBytes>;

    /// The secret is optional (`secrets.yml`: `optional: true`) and has no value here: the
    /// steps that use it are skipped instead of failing.
    fn absent(&self, name: &str) -> bool {
        let _ = name;
        false
    }
}

/// Resolves a name the prepare stage did not (a step that had nothing to do when the run was
/// planned, and has work by the time it runs), without asking anyone.
pub type LateResolve = Box<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

/// A simple thread-safe [`SecretLookup`] whose values are zeroed on drop. The prepare stage
/// fills it; steps read it.
#[derive(Default)]
pub struct SecretCache {
    values: RwLock<BTreeMap<String, SecretBytes>>,
    absent: RwLock<std::collections::BTreeSet<String>>,
    /// Names already tried without success: never resolved late.
    missing: RwLock<std::collections::BTreeSet<String>>,
    late: RwLock<Option<LateResolve>>,
}

impl SecretCache {
    /// Marks an optional secret that has no value (see [`SecretLookup::absent`]).
    pub fn mark_absent(&self, name: impl Into<String>) {
        self.absent
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.into());
    }

    /// A name the prepare stage tried and could not resolve: not asked for again.
    pub fn mark_missing(&self, name: impl Into<String>) {
        self.missing
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.into());
    }

    /// What [`SecretLookup::lookup`] asks for a name it does not hold (see [`LateResolve`]).
    pub fn set_late(&self, late: LateResolve) {
        *self.late.write().unwrap_or_else(|e| e.into_inner()) = Some(late);
    }

    pub fn insert(&self, name: impl Into<String>, value: impl Into<Vec<u8>>) {
        self.values
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.into(), SecretBytes::new(value));
    }

    pub fn contains(&self, name: &str) -> bool {
        self.values
            .read()
            .map(|v| v.contains_key(name))
            .unwrap_or(false)
    }

    pub fn len(&self) -> usize {
        self.values.read().map(|v| v.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for SecretCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretCache({} values)", self.len())
    }
}

impl SecretLookup for SecretCache {
    fn lookup(&self, name: &str) -> Option<SecretBytes> {
        if let Some(v) = self
            .values
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
        {
            return Some(v.clone());
        }
        let tried = self
            .missing
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name);
        if tried || self.absent(name) {
            return None;
        }
        let late = self.late.read().unwrap_or_else(|e| e.into_inner());
        let value = late.as_ref()?(name)?;
        drop(late);
        self.insert(name, value);
        self.values
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    fn absent(&self, name: &str) -> bool {
        self.absent
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_bytes_and_never_printed() {
        let c = SecretCache::default();
        c.insert("tok", "s3cret");
        c.insert("bin", vec![0xff, 0x00]);
        let v = c.lookup("tok").unwrap();
        assert_eq!(v.as_str(), Some("s3cret"));
        assert_eq!(format!("{v:?}"), "***");
        assert_eq!(c.lookup("bin").unwrap().as_str(), None);
        assert_eq!(c.lookup("bin").unwrap().as_bytes(), [0xff, 0x00]);
        assert!(c.lookup("nope").is_none());
        assert!(!format!("{c:?}").contains("s3cret"));
    }

    #[test]
    fn late_names_are_resolved_once_and_absent_ones_never() {
        let c = SecretCache::default();
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = asked.clone();
        c.set_late(Box::new(move |n| {
            log.lock().unwrap().push(n.to_string());
            (n == "late").then(|| b"v".to_vec())
        }));
        c.mark_absent("opt");
        assert_eq!(c.lookup("late").unwrap().as_str(), Some("v"));
        assert_eq!(c.lookup("late").unwrap().as_str(), Some("v"));
        assert!(c.lookup("nope").is_none());
        assert!(c.lookup("opt").is_none() && c.absent("opt"));
        assert_eq!(*asked.lock().unwrap(), ["late", "nope"]);
    }
}
