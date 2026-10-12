//! `docs/reference.md` is generated from the JSON Schemas `dot schema bundle|repo|secrets`
//! prints (the ones every release publishes), so the reference cannot drift from what `dot`
//! accepts: this test renders it and fails when the committed file differs. After changing a
//! config struct or its doc comments, regenerate it with
//!
//!     DOT_BLESS=1 cargo test -p dot-cli --test reference
//!
//! A test rather than an `xtask`: it runs with the rest of `cargo test --workspace` (locally and
//! in CI) and needs no extra crate or command to remember.

use std::collections::BTreeMap;
use std::process::Command;

use serde_json::Value;

const DOC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/reference.md");

const RELEASES: &str = "https://github.com/trungngo/dot-cli/releases/latest/download";

/// The files, in the order they are documented: `dot schema` argument, file, published schema.
const FILES: [(&str, &str, &str); 3] = [
    ("repo", "dot.yml", "dot.schema.json"),
    ("bundle", "bundles/<name>/bundle.yml", "bundle.schema.json"),
    ("secrets", "secrets.yml", "secrets.schema.json"),
];

fn schema(kind: &str) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_dot"))
        .args(["schema", kind])
        .output()
        .unwrap();
    assert!(out.status.success(), "dot schema {kind}: {out:?}");
    serde_json::from_slice(&out.stdout).unwrap()
}

/// One line for a table cell: no line breaks, no bare `|`.
fn cell(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

fn anchor(def: &str) -> String {
    def.to_lowercase()
}

fn def_name(reference: &str) -> &str {
    reference.rsplit('/').next().unwrap_or(reference)
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A short, human-readable type for a schema.
fn type_of(s: &Value) -> String {
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let name = def_name(r);
        return format!("[{name}](#{})", anchor(name));
    }
    if let Some(values) = s.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(|v| format!("`{}`", scalar(v)))
            .collect::<Vec<_>>()
            .join(", ");
    }
    if let Some(c) = s.get("const") {
        return format!("`{}`", scalar(c));
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(alts) = s.get(key).and_then(Value::as_array) {
            let mut seen: Vec<String> = Vec::new();
            for t in alts.iter().map(type_of).filter(|t| !t.is_empty()) {
                if !seen.contains(&t) {
                    seen.push(t);
                }
            }
            // Alternatives that only constrain the value (patterns) say nothing of its type.
            seen.retain(|t| t != "any");
            // `x` or `list of x`: one value or several.
            if let [one, list] = seen.as_slice()
                && *list == format!("list of {one}")
            {
                return format!("{one} (or a list)");
            }
            if !seen.is_empty() {
                return seen.join(" or ");
            }
        }
    }
    if let Some(all) = s.get("allOf").and_then(Value::as_array)
        && let Some(first) = all.first()
    {
        return type_of(first);
    }
    let types: Vec<&str> = match s.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let named: Vec<String> = types
        .iter()
        .filter(|t| **t != "null")
        .map(|t| match *t {
            "string" => "string".to_string(),
            "boolean" => "bool".to_string(),
            "integer" => "int".to_string(),
            "number" => "number".to_string(),
            "array" => match s.get("items") {
                Some(items) => format!("list of {}", type_of(items)),
                None => "list".to_string(),
            },
            "object" => match s.get("additionalProperties") {
                Some(ap @ Value::Object(_)) => format!("map of {}", type_of(ap)),
                _ => "mapping".to_string(),
            },
            other => other.to_string(),
        })
        .collect();
    if named.is_empty() {
        if types.contains(&"null") {
            return "null".into();
        }
        return "any".into();
    }
    named.join(" or ")
}

/// A property's description: its own, else the one of the definition it refers to.
fn describe(p: &Value, defs: &BTreeMap<String, Value>) -> String {
    let own = p.get("description").and_then(Value::as_str);
    let referred = || {
        let r = p.get("$ref").and_then(Value::as_str).or_else(|| {
            ["anyOf", "oneOf", "allOf"].iter().find_map(|k| {
                p.get(*k)?
                    .as_array()?
                    .iter()
                    .find_map(|a| a.get("$ref").and_then(Value::as_str))
            })
        })?;
        defs.get(def_name(r))?.get("description")?.as_str()
    };
    let mut text = cell(own.or_else(referred).unwrap_or(""));
    if let Some(d) = p.get("default").filter(|d| !d.is_null())
        && !matches!(d, Value::Array(a) if a.is_empty())
        && !matches!(d, Value::Object(o) if o.is_empty())
        && !text.to_lowercase().contains("default")
    {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&format!("Default: `{}`.", scalar(d)));
    }
    text
}

fn properties_table(s: &Value, defs: &BTreeMap<String, Value>, out: &mut String) {
    let Some(props) = s.get("properties").and_then(Value::as_object) else {
        return;
    };
    let required: Vec<&str> = s
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    out.push_str("| key | type | description |\n|---|---|---|\n");
    for (name, p) in props {
        let req = if required.contains(&name.as_str()) {
            " (required)"
        } else {
            ""
        };
        out.push_str(&format!(
            "| `{name}`{req} | {} | {} |\n",
            cell(&type_of(p)),
            describe(p, defs)
        ));
    }
    out.push('\n');
}

/// The values of an enumeration, with their descriptions where there are any.
fn values_list(s: &Value, out: &mut String) -> bool {
    let alts = ["oneOf", "anyOf"]
        .iter()
        .find_map(|k| s.get(*k).and_then(Value::as_array));
    if let Some(alts) = alts
        && alts
            .iter()
            .all(|a| a.get("const").is_some() || a.get("enum").is_some())
    {
        out.push_str("One of:\n\n");
        for a in alts {
            let values: Vec<String> = match (a.get("const"), a.get("enum")) {
                (Some(c), _) => vec![scalar(c)],
                (_, Some(Value::Array(vs))) => vs.iter().map(scalar).collect(),
                _ => Vec::new(),
            };
            let values = values
                .iter()
                .map(|v| format!("`{v}`"))
                .collect::<Vec<_>>()
                .join(", ");
            match a.get("description").and_then(Value::as_str) {
                Some(d) => out.push_str(&format!("- {values}: {}\n", cell(d))),
                None => out.push_str(&format!("- {values}\n")),
            }
        }
        out.push('\n');
        return true;
    }
    false
}

fn render() -> String {
    let mut out = String::new();
    out.push_str(
        "# Configuration reference\n\n\
         <!-- Generated from `dot schema repo|bundle|secrets` by crates/dot-cli/tests/reference.rs:\n     \
         do not edit by hand. Regenerate with `DOT_BLESS=1 cargo test -p dot-cli --test reference`. -->\n\n\
         Every key `dot` accepts in a dotfiles repository, generated from the JSON Schemas that\n\
         `dot schema repo|bundle|secrets` prints and every release publishes. What the directives\n\
         do when they run (ordering, idempotence, `unless`, undo and state) is in\n\
         [design.md](design.md).\n\n\
         Unknown keys are errors. Paths may start with `~`; most strings are rendered as templates\n\
         (`{{ var }}`) first.\n\n\
         For completion and validation in editors with the YAML language server (VS Code's YAML\n\
         extension, Neovim, Helix, ...), put the matching line at the top of each file:\n\n",
    );
    for (_, file, published) in FILES {
        out.push_str(&format!(
            "    # yaml-language-server: $schema={RELEASES}/{published}   ({file})\n"
        ));
    }
    out.push_str("\nContents:\n\n");
    for (_, file, _) in FILES {
        let a: String = file
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == ' ')
            .collect::<String>()
            .replace(' ', "-")
            .to_lowercase();
        out.push_str(&format!("- [`{file}`](#{a})\n"));
    }
    out.push_str("- [Types](#types)\n\n");

    let mut defs: BTreeMap<String, Value> = BTreeMap::new();
    let mut sections = String::new();
    for (kind, file, published) in FILES {
        let s = schema(kind);
        if let Some(d) = s.get("$defs").and_then(Value::as_object) {
            for (name, def) in d {
                if let Some(prev) = defs.get(name) {
                    assert_eq!(prev, def, "two schemas define `{name}` differently");
                }
                defs.insert(name.clone(), def.clone());
            }
        }
        sections.push_str(&format!("## `{file}`\n\n"));
        if let Some(d) = s.get("description").and_then(Value::as_str) {
            sections.push_str(&format!("{}\n\n", cell(d)));
        }
        sections.push_str(&format!(
            "Schema: [`{published}`]({RELEASES}/{published}) (`dot schema {kind}`).\n\n"
        ));
        properties_table(&s, &defs, &mut sections);
    }
    out.push_str(&sections);

    out.push_str("## Types\n\n");
    for (name, def) in &defs {
        out.push_str(&format!("### {name}\n\n"));
        if let Some(d) = def.get("description").and_then(Value::as_str) {
            out.push_str(&format!("{}\n\n", cell(d)));
        }
        if def.get("properties").is_some() {
            properties_table(def, &defs, &mut out);
        } else if !values_list(def, &mut out) {
            out.push_str(&format!("Type: {}.\n\n", type_of(def)));
        }
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

#[test]
fn the_reference_is_generated_from_the_schemas() {
    let want = render();
    if std::env::var_os("DOT_BLESS").is_some() {
        std::fs::write(DOC, &want).unwrap();
        return;
    }
    // A checkout with CRLF line endings (git on Windows) holds the same text.
    let have = std::fs::read_to_string(DOC)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    if have != want {
        let line = have
            .lines()
            .zip(want.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| have.lines().count().min(want.lines().count()));
        panic!(
            "docs/reference.md is out of date (first difference at line {}): regenerate it with \
             `DOT_BLESS=1 cargo test -p dot-cli --test reference`",
            line + 1
        );
    }
}
