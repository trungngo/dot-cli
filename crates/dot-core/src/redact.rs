//! Masks known secret values in any text a renderer writes. Every renderer passes output through
//! a shared [`Redactor`]; `dot-secrets` registers each resolved value with it. With no secrets
//! the set is empty, so text passes through unchanged.
//!
//! Child output reaches renderers one line at a time, so a multi-line secret (a PEM key, an
//! `.ovpn` profile) is never seen whole: each of its lines is registered as well. Registered
//! values are held in buffers that are zeroed when the redactor is dropped.

use std::borrow::Cow;
use std::sync::{Arc, RwLock};

pub const MASK: &str = "***";

/// Lines of a multi-line secret shorter than this (after trimming) are not masked on their own:
/// `}` or `-----END-----` style lines would otherwise mask ordinary output everywhere.
pub const MIN_FRAGMENT: usize = 8;

/// A UTF-8 string whose bytes are overwritten with zeros on drop.
pub(crate) struct Zeroizing(Vec<u8>);

impl Zeroizing {
    pub(crate) fn new(s: String) -> Self {
        Self(s.into_bytes())
    }

    pub(crate) fn as_str(&self) -> &str {
        // Built only from `String`s and never mutated until drop.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

impl Drop for Zeroizing {
    fn drop(&mut self) {
        self.0.iter_mut().for_each(|b| *b = 0);
        // Keeps the writes from being optimised away as dead stores.
        std::hint::black_box(&mut self.0);
    }
}

/// Cheap to clone; all clones share one set of secrets.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Arc<RwLock<Vec<Zeroizing>>>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.secrets.read().map(|s| s.len()).unwrap_or(0);
        write!(f, "Redactor({n} patterns)")
    }
}

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a value to mask. Empty values are ignored (they would match everywhere). A
    /// multi-line value is also registered line by line (lines of at least [`MIN_FRAGMENT`]
    /// characters), since output is redacted one line at a time.
    pub fn add(&self, secret: impl Into<String>) {
        let secret = Zeroizing::new(secret.into());
        let mut s = self.secrets.write().unwrap_or_else(|e| e.into_inner());
        let text = secret.as_str();
        if text.contains(['\n', '\r']) {
            for line in text.lines() {
                let line = line.trim();
                if line.chars().count() >= MIN_FRAGMENT {
                    Self::insert(&mut s, Zeroizing::new(line.to_string()));
                }
            }
        }
        Self::insert(&mut s, secret);
    }

    fn insert(set: &mut Vec<Zeroizing>, value: Zeroizing) {
        if value.0.is_empty() || set.iter().any(|v| v.0 == value.0) {
            return;
        }
        set.push(value);
        // Longest first, so a secret containing another is masked whole.
        set.sort_by_key(|v| std::cmp::Reverse(v.0.len()));
    }

    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let secrets = self.secrets.read().unwrap_or_else(|e| e.into_inner());
        if !secrets.iter().any(|s| text.contains(s.as_str())) {
            return Cow::Borrowed(text);
        }
        let mut out = text.to_string();
        for s in secrets.iter() {
            out = out.replace(s.as_str(), MASK);
        }
        Cow::Owned(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_passes_through() {
        let r = Redactor::new();
        assert!(matches!(r.redact("hello"), Cow::Borrowed("hello")));
    }

    #[test]
    fn masks_registered_values_in_all_clones() {
        let r = Redactor::new();
        let clone = r.clone();
        r.add("hunter2");
        r.add("");
        assert_eq!(clone.redact("pw=hunter2 again hunter2"), "pw=*** again ***");
    }

    #[test]
    fn multi_line_secret_is_masked_line_by_line() {
        let r = Redactor::new();
        let pem = "-----BEGIN KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEF\r\nAASCBKcwggSjAgEAAoIBAQC7\n}\n-----END KEY-----\n";
        r.add(pem);
        // As a renderer sees it: one line per Output event.
        let lines: Vec<String> = pem.lines().map(|l| r.redact(l).into_owned()).collect();
        assert_eq!(lines, ["***", "***", "***", "}", "***"]);
        // The whole value is still masked when it appears in one piece of text.
        assert_eq!(r.redact(&format!("x{pem}y")), "x***y");
        // Short lines are not registered on their own.
        assert_eq!(r.redact("a } b"), "a } b");
    }
}
