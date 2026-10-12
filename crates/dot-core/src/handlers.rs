//! Handlers: a bundle's `handlers:` are steps that run once, at the end of the bundle, when a
//! step that names them in `notify:` changed something (Ansible-like):
//!
//! ```yaml
//! steps:
//!   - template:
//!       /etc/ssh/sshd_config.d/10-dot.conf: { src: sshd.conf.j2, root: true }
//!     notify: restart-sshd
//! handlers:
//!   restart-sshd:
//!     service: { name: ssh, restart: true }
//! ```
//!
//! A notification must not be lost when the bundle fails after the notifying step changed
//! something: the next run finds that step satisfied and would not notify again. So a real run
//! records a notification in the `handlers` section of the state store ([`crate::state`]) as
//! soon as the notifying step finishes changed (a later step may panic, or the run be killed),
//! and at the bundle's end keeps only the handlers it was due to run and did not (the bundle
//! failed first, or the handler itself failed); the next run starts with them notified. A bundle is dropped from it once its handlers have all run.
//! Written under the store's lock, so bundles finishing at once (`--jobs N`) keep each other's.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;

use crate::state::{StateStore, sections};

/// The plan note of a handler nothing is going to notify.
pub const NOT_NOTIFIED: &str =
    "handler: not notified (no step that notifies it has anything to do)";

type Pending = BTreeMap<String, BTreeSet<String>>;

/// Handlers due to run, per bundle, left over from an earlier run.
#[derive(Debug, Clone)]
pub struct PendingHandlers {
    pub store: StateStore,
}

impl PendingHandlers {
    pub fn in_dir(state_dir: &Path) -> Self {
        Self {
            store: StateStore::at(state_dir),
        }
    }

    /// The handlers of `bundle` still due from an earlier run. Unreadable state: nothing
    /// pending (the worst outcome is a missed restart, which a warning cannot fix either).
    pub fn get(&self, bundle: &str) -> BTreeSet<String> {
        self.store
            .section::<Pending>(sections::HANDLERS)
            .remove(bundle)
            .unwrap_or_default()
    }

    /// Adds `names` to the handlers of `bundle` that are due, as soon as a step notifying them
    /// changed something: if the run dies before the bundle's end (a panic, Ctrl-C), the next
    /// run still fires them. [`PendingHandlers::set`] settles the list at the bundle's end.
    pub fn add(&self, bundle: &str, names: &[String]) -> Result<()> {
        let have = self.get(bundle);
        if names.iter().all(|n| have.contains(n)) {
            return Ok(());
        }
        self.store
            .update_section(sections::HANDLERS, |all: &mut Pending| {
                all.entry(bundle.to_string())
                    .or_default()
                    .extend(names.iter().cloned());
            })
    }

    /// Records `names` as the handlers of `bundle` still due (none: forgets the bundle).
    pub fn set(&self, bundle: &str, names: &BTreeSet<String>) -> Result<()> {
        if self.get(bundle) == *names {
            return Ok(());
        }
        self.store
            .update_section(sections::HANDLERS, |all: &mut Pending| {
                if names.is_empty() {
                    all.remove(bundle);
                } else {
                    all.insert(bundle.to_string(), names.clone());
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_forgets_per_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let p = PendingHandlers::in_dir(&tmp.path().join("state"));
        assert!(p.get("ssh").is_empty());
        let names: BTreeSet<String> = ["restart-sshd".to_string()].into();
        p.set("ssh", &names).unwrap();
        p.set("other", &["x".to_string()].into()).unwrap();
        assert_eq!(p.get("ssh"), names);
        p.set("ssh", &BTreeSet::new()).unwrap();
        assert!(p.get("ssh").is_empty());
        assert!(p.store.read().sections.contains_key(sections::HANDLERS));
        p.set("other", &BTreeSet::new()).unwrap();
        assert!(
            !p.store.read().sections.contains_key(sections::HANDLERS),
            "dropped once nothing is pending"
        );
        std::fs::write(p.store.path(), "junk").unwrap();
        assert!(p.get("ssh").is_empty());
    }
}
