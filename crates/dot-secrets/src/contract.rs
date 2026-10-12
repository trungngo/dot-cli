//! The behaviour every [`SecretProvider`] must show. A provider's tests implement [`Harness`]
//! and call [`run`]; any deviation panics with the check that failed.
//!
//! ```text
//! struct MyHarness;
//! impl dot_secrets::contract::Harness for MyHarness { … }
//! #[test]
//! fn contract() { dot_secrets::contract::run(&MyHarness); }
//! ```
//!
//! Checked: availability and unlock; single and batched resolve (order kept, duplicates
//! allowed); a missing key is `None`, and through a [`Resolver`] an error naming every missing
//! secret; file secrets (when [`Caps::files`]); `Debug` never shows a value; and a store →
//! resolve round trip for awkward values (when [`Caps::write`], otherwise `store` must fail),
//! and for file bytes (when [`Caps::write_files`], otherwise storing a file must fail).

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::config::{RefSpec, SecretKind, SecretSpec, SecretsConfig};
use crate::registry::Registry;
use crate::resolver::Resolver;
use crate::{Caps, NonInteractive, SecretProvider, SecretRef, SecretValue};

/// Content a harness must place in a fresh provider before handing it out.
#[derive(Debug, Clone, Default)]
pub struct Seed {
    /// `(key, value)`, addressed with [`Harness::value_ref`].
    pub values: Vec<(String, String)>,
    /// `(relative path, bytes)`, addressed with [`Harness::file_ref`].
    pub files: Vec<(String, Vec<u8>)>,
}

pub trait Harness {
    /// A new provider, sharing nothing with earlier ones, that holds exactly `seed`.
    fn provider(&self, seed: &Seed) -> Box<dyn SecretProvider>;
    /// The ref for value `key` (an env-style key: letters, digits, `_`).
    fn value_ref(&self, key: &str) -> SecretRef;
    /// The ref for file `path` (relative, `/`-separated); `None` without file support.
    fn file_ref(&self, path: &str) -> Option<SecretRef>;
}

const ONE: (&str, &str) = ("DOT_CONTRACT_ONE", "contract-value-one");
const TWO: (&str, &str) = ("DOT_CONTRACT_TWO", "contract value two # not a comment");
const THREE: (&str, &str) = (
    "DOT_CONTRACT_THREE",
    "multi\nline $HOME ${X} 'single' \"double\" \\ back",
);
const FILE: (&str, &[u8]) = (
    "certs/client.pem",
    b"-----BEGIN KEY-----\n\x00\x01\xfe\xff\n",
);
const ABSENT: &str = "DOT_CONTRACT_ABSENT";

fn seed() -> Seed {
    Seed {
        values: [ONE, TWO, THREE]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        files: vec![(FILE.0.to_string(), FILE.1.to_vec())],
    }
}

/// Runs every check against fresh providers from `h`.
pub fn run(h: &dyn Harness) {
    basics(h);
    resolve_single(h);
    resolve_batch(h);
    missing(h);
    missing_through_resolver(h);
    file_secrets(h);
    redaction(h);
    store_round_trip(h);
}

fn text(v: &Option<SecretValue>) -> Option<&str> {
    v.as_ref().map(|v| v.expose_str().expect("value is UTF-8"))
}

/// One call when the provider batches, else one call per ref (as the resolver does).
fn resolve(p: &dyn SecretProvider, refs: &[SecretRef]) -> Vec<Option<SecretValue>> {
    let out = if p.capabilities().batch {
        p.resolve(refs).expect("batch resolve failed")
    } else {
        refs.iter()
            .flat_map(|r| p.resolve(std::slice::from_ref(r)).expect("resolve failed"))
            .collect()
    };
    assert_eq!(out.len(), refs.len(), "one result per ref");
    out
}

fn fresh(h: &dyn Harness) -> Box<dyn SecretProvider> {
    let mut p = h.provider(&seed());
    assert!(
        p.available().expect("available() errored"),
        "provider must be available"
    );
    p.unlock(&mut NonInteractive).expect("unlock failed");
    p
}

fn basics(h: &dyn Harness) {
    let p = fresh(h);
    let caps = p.capabilities();
    assert!(caps.read, "a provider must be readable");
    assert_eq!(
        h.value_ref("K").provider(),
        p.scheme(),
        "value_ref must address this provider"
    );
    if let Some(f) = h.file_ref("a/b") {
        assert_eq!(
            f.provider(),
            p.scheme(),
            "file_ref must address this provider"
        );
        assert!(f.is_file(), "file_ref must be a file ref");
    }
    assert!(
        p.resolve(&[]).expect("empty batch").is_empty(),
        "empty batch → empty result"
    );
}

fn resolve_single(h: &dyn Harness) {
    let p = fresh(h);
    for (k, v) in [ONE, TWO, THREE] {
        let got = p.resolve(&[h.value_ref(k)]).expect("resolve failed");
        assert_eq!(got.len(), 1);
        assert_eq!(text(&got[0]), Some(v), "value of {k}");
    }
}

fn resolve_batch(h: &dyn Harness) {
    let p = fresh(h);
    let refs = [
        h.value_ref(THREE.0),
        h.value_ref(ONE.0),
        h.value_ref(ABSENT),
        h.value_ref(TWO.0),
        h.value_ref(ONE.0),
    ];
    let got = resolve(p.as_ref(), &refs);
    let got: Vec<Option<&str>> = got.iter().map(text).collect();
    assert_eq!(
        got,
        [Some(THREE.1), Some(ONE.1), None, Some(TWO.1), Some(ONE.1)],
        "results keep order, allow duplicates, and report a missing key as None"
    );
}

fn missing(h: &dyn Harness) {
    let p = fresh(h);
    let got = p
        .resolve(&[h.value_ref(ABSENT)])
        .expect("missing is not an error");
    assert!(got[0].is_none(), "a missing key resolves to None");
    if let Some(f) = h.file_ref("no/such/file") {
        assert!(p.resolve(&[f]).expect("missing file is not an error")[0].is_none());
    }
}

/// Wraps a harness provider in a one-shot registry.
fn resolver_for(h: &dyn Harness, secrets: BTreeMap<String, SecretSpec>) -> Resolver {
    let provider = h.provider(&seed());
    let scheme = provider.scheme();
    let slot = Mutex::new(Some(provider));
    let mut registry = Registry::new();
    registry.register(scheme, move |_| {
        slot.lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| anyhow::anyhow!("the resolver must create each provider once"))
    });
    let config = SecretsConfig {
        default_provider: scheme.to_string(),
        secrets,
        ..SecretsConfig::default()
    };
    Resolver::new(registry, config)
}

fn spec(r: SecretRef, kind: SecretKind) -> SecretSpec {
    SecretSpec {
        reference: RefSpec::Ref(r),
        fallback: None,
        kind,
        optional: false,
    }
}

fn missing_through_resolver(h: &dyn Harness) {
    let mut secrets = BTreeMap::new();
    secrets.insert(
        "present".to_string(),
        spec(h.value_ref(ONE.0), SecretKind::Value),
    );
    secrets.insert(
        "absent_one".to_string(),
        spec(h.value_ref(&format!("{ABSENT}_1")), SecretKind::Value),
    );
    secrets.insert(
        "absent_two".to_string(),
        spec(h.value_ref(&format!("{ABSENT}_2")), SecretKind::Value),
    );
    let mut resolver = resolver_for(h, secrets);

    let ok = resolver
        .resolve(["present"], &mut NonInteractive)
        .expect("a present secret resolves");
    assert_eq!(
        ok.get("present").and_then(SecretValue::expose_str),
        Some(ONE.1)
    );

    let err = resolver
        .resolve(
            ["present", "absent_two", "absent_one", "undeclared"],
            &mut NonInteractive,
        )
        .expect_err("missing secrets must fail the whole resolve");
    let names: Vec<&str> = err.names().collect();
    assert_eq!(
        names,
        ["absent_one", "absent_two", "undeclared"],
        "every missing name, sorted"
    );
    let msg = err.to_string();
    for needle in [
        "absent_one",
        "absent_two",
        "undeclared",
        &format!("{ABSENT}_1"),
        &format!("{ABSENT}_2"),
    ] {
        assert!(msg.contains(needle), "error must mention `{needle}`: {msg}");
    }
    assert!(!msg.contains(ONE.1), "error must not contain values: {msg}");
}

fn file_secrets(h: &dyn Harness) {
    let p = fresh(h);
    let (Some(f), true) = (h.file_ref(FILE.0), p.capabilities().files) else {
        return;
    };
    let got = resolve(p.as_ref(), &[f.clone(), h.value_ref(ONE.0)]);
    assert_eq!(
        got[0].as_ref().map(SecretValue::expose),
        Some(FILE.1),
        "file bytes, verbatim"
    );
    assert_eq!(
        text(&got[1]),
        Some(ONE.1),
        "files and values mix in one batch"
    );

    let mut secrets = BTreeMap::new();
    secrets.insert("cert".to_string(), spec(f, SecretKind::File));
    let mut resolver = resolver_for(h, secrets);
    let r = resolver
        .resolve(["cert"], &mut NonInteractive)
        .expect("file secret resolves");
    assert_eq!(r.get("cert").map(SecretValue::expose), Some(FILE.1));
}

fn redaction(h: &dyn Harness) {
    let p = fresh(h);
    let got = resolve(p.as_ref(), &[h.value_ref(ONE.0), h.value_ref(TWO.0)]);
    let v = got[0].as_ref().expect("seeded value");
    assert_eq!(format!("{v:?}"), "***");
    assert_eq!(format!("{v:#?}"), "***");
    let all = format!("{got:?}");
    assert!(
        !all.contains(ONE.1) && !all.contains("contract value two"),
        "{all}"
    );

    let mut secrets = BTreeMap::new();
    secrets.insert(
        "one".to_string(),
        spec(h.value_ref(ONE.0), SecretKind::Value),
    );
    let mut resolver = resolver_for(h, secrets);
    let resolved = resolver
        .resolve(["one"], &mut NonInteractive)
        .expect("resolves");
    let shown = format!("{resolved:?} {resolver:?}");
    assert!(shown.contains("one") && shown.contains("***"), "{shown}");
    assert!(!shown.contains(ONE.1), "{shown}");
}

fn store_round_trip(h: &dyn Harness) {
    let p = fresh(h);
    let Caps {
        write,
        files,
        write_files,
        ..
    } = p.capabilities();
    let value = SecretValue::from("stored".to_string());
    if !write {
        assert!(
            p.store(&h.value_ref("DOT_CONTRACT_NEW"), &value).is_err(),
            "store must fail without Caps.write"
        );
        return;
    }
    let awkward = [
        "plain",
        "with spaces # and hash",
        "quote ' and \" and $HOME and ${X} and \\ backslash",
        "multi\nline\nvalue\n",
        "trailing backslash\\",
        "  padded  ",
        "unicode ✓ ключ",
    ];
    let refs: Vec<SecretRef> = (0..awkward.len())
        .map(|i| h.value_ref(&format!("DOT_CONTRACT_STORED_{i}")))
        .collect();
    for (r, v) in refs.iter().zip(awkward) {
        p.store(r, &SecretValue::from(v.to_string()))
            .unwrap_or_else(|e| panic!("store {r}: {e:#}"));
    }
    let got = resolve(p.as_ref(), &refs);
    for ((r, v), g) in refs.iter().zip(awkward).zip(&got) {
        assert_eq!(text(g), Some(v), "round trip of {r}");
    }

    // Overwrite keeps other keys intact.
    p.store(&refs[0], &SecretValue::from("replaced".to_string()))
        .expect("overwrite");
    let got = resolve(
        p.as_ref(),
        &[
            refs[0].clone(),
            refs[1].clone(),
            h.value_ref(ONE.0),
            h.value_ref(THREE.0),
        ],
    );
    let got: Vec<Option<&str>> = got.iter().map(text).collect();
    assert_eq!(
        got,
        [
            Some("replaced"),
            Some(awkward[1]),
            Some(ONE.1),
            Some(THREE.1)
        ]
    );

    if let (Some(f), true, false) = (h.file_ref("stored/new.bin"), files, write_files) {
        assert!(
            p.store(&f, &SecretValue::new(vec![1])).is_err(),
            "storing a file must fail without Caps.write_files"
        );
    }
    if let (Some(f), true, true) = (h.file_ref("stored/new.bin"), files, write_files) {
        let bytes = vec![0u8, 1, 2, 0xff, b'\n'];
        p.store(&f, &SecretValue::new(bytes.clone()))
            .expect("store file");
        let got = p.resolve(&[f]).expect("resolve stored file");
        assert_eq!(
            got[0].as_ref().map(SecretValue::expose),
            Some(bytes.as_slice())
        );
    }
}
