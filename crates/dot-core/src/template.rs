//! `{{ var }}` templating of config strings (paths, commands, modes, `when:` values), through
//! minijinja with strict undefined: an undefined variable is an error, never an empty string.
//!
//! Values are validated twice: while parsing, only values without a template are checked (a
//! templated value cannot be judged before it is rendered); at plan time every value is rendered
//! and then checked again.
//!
//! Templates see every variable (see [`crate::vars`]) and one function, `secret(name)`. What
//! `secret()` does depends on the [`SecretMode`]: while planning it records the name and renders
//! a placeholder (nothing is resolved, so `dot plan` never unlocks a provider); while applying it
//! returns the value resolved in the prepare stage, or, for a command line, a reference to an
//! environment variable holding it ([`SecretMode::Indirect`]).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use minijinja::{Environment, Error, ErrorKind, UndefinedBehavior};

use crate::secret::SecretLookup;
pub use crate::vars::Vars;

type Ctx = std::collections::BTreeMap<String, minijinja::Value>;
type Unrendered = std::collections::BTreeMap<String, String>;

/// Whether `s` contains template syntax and so must be rendered before it can be validated.
pub fn is_template(s: &str) -> bool {
    s.contains("{{") || s.contains("{%") || s.contains("{#")
}

/// The placeholder `secret(name)` renders to while planning.
pub fn secret_placeholder(name: &str) -> String {
    format!("<secret:{name}>")
}

/// What `secret(name)` does in a template.
#[derive(Clone, Default)]
pub enum SecretMode {
    /// Not allowed here (`when:`, bundle `check:`).
    #[default]
    Forbidden,
    /// Planning: record the name, render [`secret_placeholder`].
    Placeholder,
    /// Applying: the resolved value; an unresolved name is an error.
    Resolve(Arc<dyn SecretLookup>),
    /// Planning with the values that could be resolved without asking anyone (`dot status`):
    /// the value when the lookup has it, else [`secret_placeholder`] (and the name recorded).
    Available(Arc<dyn SecretLookup>),
    /// Applying a command line: `secret(name)` renders a reference to an environment variable
    /// (see [`Indirect`]) and the value goes to the child's environment, never into its argv.
    Indirect(Arc<Indirect>),
}

/// The state of [`SecretMode::Indirect`] for one render: which secrets were bound to which
/// environment variable.
pub struct Indirect {
    lookup: Arc<dyn SecretLookup>,
    /// How the interpreter spells a variable reference, e.g. `${NAME}` or `%NAME%`.
    reference: Box<dyn Fn(&str) -> String + Send + Sync>,
    bound: Mutex<Vec<(String, String)>>,
}

impl Indirect {
    pub fn new(
        lookup: Arc<dyn SecretLookup>,
        reference: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            lookup,
            reference: Box::new(reference),
            bound: Mutex::default(),
        }
    }

    /// The variable a secret is passed in: `DOT_SECRET_` and the name, with characters that
    /// cannot appear in a variable name escaped.
    pub fn var_name(secret: &str) -> String {
        let mut out = String::from("DOT_SECRET_");
        for c in secret.chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                out.push(c);
            } else {
                out.push_str(&format!("_x{:02X}_", u32::from(c)));
            }
        }
        out
    }

    /// The reference text `secret(name)` renders to.
    pub fn reference(&self, var: &str) -> String {
        (self.reference)(var)
    }

    fn bind(&self, name: &str) -> Result<String, Error> {
        match self.lookup.lookup(name) {
            Some(v) if v.as_str().is_some() => {}
            Some(_) => return Err(not_text(name)),
            None => return Err(unresolved(name)),
        }
        let var = Self::var_name(name);
        let mut bound = self.bound.lock().unwrap_or_else(|e| e.into_inner());
        if !bound.iter().any(|(n, _)| n == name) {
            bound.push((name.to_string(), var.clone()));
        }
        Ok(self.reference(&var))
    }

    /// `(secret name, variable)` pairs bound so far.
    pub fn bound(&self) -> Vec<(String, String)> {
        self.bound.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn lookup(&self) -> &Arc<dyn SecretLookup> {
        &self.lookup
    }
}

fn unresolved(name: &str) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!("secret `{name}` was not resolved"),
    )
}

fn not_text(name: &str) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!("secret `{name}` is not UTF-8 text, so `secret()` cannot use it"),
    )
}

/// A rendered string and the secret names it asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub secrets: Vec<String>,
}

/// Names minijinja provides itself; never reported as undefined variables.
const GLOBALS: [&str; 7] = [
    "secret",
    "range",
    "dict",
    "namespace",
    "debug",
    "loop",
    "self",
];

impl Vars {
    /// Renders `text` with `secret()` forbidden. The error names the offending expression.
    pub fn render(&self, text: &str) -> Result<String, String> {
        self.render_with(text, &SecretMode::Forbidden)
            .map(|r| r.text)
    }

    /// Renders `text`; `secrets` decides what `secret()` does.
    pub fn render_with(&self, text: &str, secrets: &SecretMode) -> Result<Rendered, String> {
        render_in(&self.template_ctx, &self.unrendered, text, secrets)
    }

    /// Evaluates a step's `if:` expression (Jinja, without the braces: `godot_mono`,
    /// `client_id != ""`, `codename in ["jammy", "noble"] or version >= "25.04"`) to a truth
    /// value, as `{% if %}` would judge it. `secret()` is not allowed; an undefined variable is
    /// an error. A `{{ ... }}` around the whole expression is accepted too.
    pub fn eval_bool(&self, expr: &str) -> Result<bool, String> {
        let trimmed = expr.trim();
        let inner = trimmed
            .strip_prefix("{{")
            .and_then(|t| t.strip_suffix("}}"))
            .unwrap_or(trimmed)
            .trim();
        // Rendered as a template, so the errors (undefined variables, why a variable could
        // not be rendered) read like every other one.
        let text = format!("{{% if {inner} %}}1{{% else %}}0{{% endif %}}");
        match render_in(
            &self.template_ctx,
            &self.unrendered,
            &text,
            &SecretMode::Forbidden,
        )?
        .text
        .as_str()
        {
            "1" => Ok(true),
            _ => Ok(false),
        }
    }
}

/// The variable names `text` refers to (excluding names it defines itself and globals).
pub(crate) fn referenced_names(text: &str) -> BTreeSet<String> {
    if !is_template(text) {
        return BTreeSet::new();
    }
    let env = Environment::new();
    match env.template_from_str(text) {
        Ok(t) => t
            .undeclared_variables(false)
            .into_iter()
            .filter(|n| !GLOBALS.contains(&n.as_str()))
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// Renders `text` against `ctx`. `unrendered`: variables left out of `ctx` because their own
/// template failed, and why (so an error names the real cause).
pub(crate) fn render_in(
    ctx: &Ctx,
    unrendered: &Unrendered,
    text: &str,
    secrets: &SecretMode,
) -> Result<Rendered, String> {
    if !is_template(text) {
        return Ok(Rendered {
            text: text.to_string(),
            secrets: Vec::new(),
        });
    }
    let requested: Arc<Mutex<BTreeSet<String>>> = Arc::default();
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_keep_trailing_newline(true);
    {
        let requested = requested.clone();
        let mode = secrets.clone();
        env.add_function("secret", move |name: String| -> Result<String, Error> {
            requested
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.clone());
            match &mode {
                SecretMode::Forbidden => Err(Error::new(
                    ErrorKind::InvalidOperation,
                    format!("secret(\"{name}\") cannot be used here"),
                )),
                SecretMode::Placeholder => Ok(secret_placeholder(&name)),
                SecretMode::Available(lookup) => match lookup.lookup(&name) {
                    Some(v) => v
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| not_text(&name)),
                    None => Ok(secret_placeholder(&name)),
                },
                SecretMode::Resolve(lookup) => match lookup.lookup(&name) {
                    Some(v) => v
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| not_text(&name)),
                    None => Err(unresolved(&name)),
                },
                SecretMode::Indirect(ind) => ind.bind(&name),
            }
        });
    }
    let result = env.template_from_str(text).and_then(|t| t.render(ctx));
    let text_out = result.map_err(|e| describe_error(&env, text, ctx, unrendered, &e))?;
    let secrets = requested
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect();
    Ok(Rendered {
        text: text_out,
        secrets,
    })
}

fn describe_error(
    env: &Environment<'_>,
    text: &str,
    ctx: &Ctx,
    unrendered: &Unrendered,
    e: &Error,
) -> String {
    let mut msg = None;
    if e.kind() == ErrorKind::UndefinedError
        && let Ok(t) = env.template_from_str(text)
    {
        let mut names: Vec<String> = t
            .undeclared_variables(false)
            .into_iter()
            .filter(|n| !ctx.contains_key(n) && !GLOBALS.contains(&n.as_str()))
            .collect();
        names.sort();
        // A variable that is set but whose own template failed: say why.
        if let Some(n) = names.iter().find(|n| unrendered.contains_key(*n)) {
            return format!(
                "variable `{n}` could not be rendered: {}",
                unrendered[n.as_str()]
            );
        }
        msg = match names.as_slice() {
            [] => None,
            [one] => Some(format!("undefined variable `{one}`")),
            many => Some(format!(
                "undefined variables {}",
                many.iter()
                    .map(|n| format!("`{n}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
    }
    let mut msg = msg.unwrap_or_else(|| match e.detail() {
        Some(d) => format!("{}: {d}", e.kind()),
        None => e.kind().to_string(),
    });
    if text.trim_end().contains('\n')
        && let Some(line) = e.line()
    {
        msg.push_str(&format!(" (template line {line})"));
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::SecretCache;

    fn vars() -> Vars {
        let mut v = Vars::default();
        v.set("home", "/h");
        v.set("os", "linux");
        v
    }

    #[test]
    fn detects_templates() {
        assert!(is_template("{{ x }}/a"));
        assert!(is_template("{% if %}"));
        assert!(!is_template("${HOME}/a {not}"));
    }

    #[test]
    fn substitutes_names_and_supports_jinja() {
        assert_eq!(vars().render("{{home}}/x/{{ os }}").unwrap(), "/h/x/linux");
        assert_eq!(vars().render("plain {x}").unwrap(), "plain {x}");
        assert_eq!(vars().render("{{ os | upper }}").unwrap(), "LINUX");
        assert_eq!(
            vars()
                .render("{% if os == 'linux' %}tux{% else %}other{% endif %}")
                .unwrap(),
            "tux"
        );
        assert_eq!(vars().render("{{ nope | default('d') }}").unwrap(), "d");
        assert_eq!(vars().render("a\n{{ os }}\n").unwrap(), "a\nlinux\n");
    }

    #[test]
    fn undefined_is_an_error() {
        let e = vars().render("{{ config_dir }}/nvim").unwrap_err();
        assert_eq!(e, "undefined variable `config_dir`");
        let e = vars().render("{% if a %}{{ b }}{% endif %}").unwrap_err();
        assert!(e.contains("`a`") && e.contains("`b`"), "{e}");
    }

    #[test]
    fn syntax_errors_name_the_line() {
        assert!(vars().render("{{ home").is_err());
        let e = vars().render("ok\n{% if x %}\n").unwrap_err();
        assert!(e.contains("syntax error"), "{e}");
        assert!(e.contains("template line"), "{e}");
    }

    #[test]
    fn secret_modes() {
        let v = vars();
        let e = v.render("{{ secret('tok') }}").unwrap_err();
        assert!(e.contains("cannot be used here"), "{e}");
        let r = v
            .render_with("t={{ secret('tok') }}", &SecretMode::Placeholder)
            .unwrap();
        assert_eq!(r.text, "t=<secret:tok>");
        assert_eq!(r.secrets, ["tok"]);
        let cache = Arc::new(SecretCache::default());
        let mode = SecretMode::Resolve(cache.clone());
        let e = v.render_with("{{ secret('tok') }}", &mode).unwrap_err();
        assert!(e.contains("secret `tok` was not resolved"), "{e}");
        cache.insert("tok", "s3cret");
        assert_eq!(
            v.render_with("{{ secret('tok') }}", &mode).unwrap().text,
            "s3cret"
        );
        cache.insert("bin", vec![0xff]);
        let e = v.render_with("{{ secret('bin') }}", &mode).unwrap_err();
        assert!(e.contains("not UTF-8 text"), "{e}");
    }

    #[test]
    fn indirect_secrets_render_as_variable_references() {
        let v = vars();
        let cache = Arc::new(SecretCache::default());
        cache.insert("api-tok", "s3cret");
        let ind = Arc::new(Indirect::new(cache.clone(), |var: &str| {
            format!("${{{var}}}")
        }));
        let r = v
            .render_with(
                "curl -H \"{{ secret('api-tok') }}\" {{ secret('api-tok') }}",
                &SecretMode::Indirect(ind.clone()),
            )
            .unwrap();
        assert_eq!(
            r.text,
            "curl -H \"${DOT_SECRET_api_x2D_tok}\" ${DOT_SECRET_api_x2D_tok}"
        );
        assert!(!r.text.contains("s3cret"));
        assert_eq!(
            ind.bound(),
            [("api-tok".to_string(), "DOT_SECRET_api_x2D_tok".to_string())]
        );
        let e = v
            .render_with("{{ secret('nope') }}", &SecretMode::Indirect(ind))
            .unwrap_err();
        assert!(e.contains("was not resolved"), "{e}");
    }
}
