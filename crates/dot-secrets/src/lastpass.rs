//! `lastpass`: wraps `lastpass-cli` (`lpass`), which must be installed first (e.g. by a bundle).
//!
//! | ref | reads |
//! |---|---|
//! | `lastpass://Dev/GitLab#password` | the item's password |
//! | `lastpass://Dev/GitLab#username` / `#notes` | its username / notes |
//! | `lastpass://Dev/GitLab#field=Token` | custom field `Token` |
//! | `lastpass://Dev/VPN#attach=work.ovpn` | attachment `work.ovpn` (a file secret) |
//! | `lastpass://id/1234#password` | item 1234, for names that are ambiguous |
//!
//! An item is named as `lpass` names it: `Folder/Item` (shared folders included) matched
//! against the item's full name, else its bare name; `id/<n>` matches the id.
//!
//! - `available`: `lpass` is on the search path (`$PATH` unless injected).
//! - `unlock`: `lpass status -q`; when logged out, `lpass login --trust <username>` on the real
//!   terminal (master password, MFA), with the live view suspended by the resolver. Without a
//!   terminal it fails, which the resolver turns into a fallback.
//! - `resolve`: one `lpass sync` per run, then one `lpass show --sync=no --json <items…>` for
//!   the whole batch. `lpass show` accepts several names, but it fails the whole call when none
//!   matches (and versions differ in how they treat a partial miss), so a failed batch is
//!   retried one item at a time, where "Could not find" means missing. A custom field absent from
//!   the JSON (older CLIs print no `fields`) is read with `--field=<name>`; attachments with
//!   `--attach=<id>` after listing the item's attachment ids.
//! - `store`: `lpass edit` (existing item) or `lpass add` (new one) with `--non-interactive`;
//!   the value goes on stdin, never on the command line. Attachments cannot be written.
//!
//! Output captured from `lpass` is zeroed when dropped; error messages only ever quote its
//! stderr.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use zeroize::{Zeroize, Zeroizing};

use crate::config::LastpassConfig;
use crate::exec::{find_program, first_line};
use crate::reference::LASTPASS;
use crate::{Caps, Interactive, SecretError, SecretProvider, SecretRef, SecretValue};

/// What `lpass show` prints when a name matches nothing.
const NOT_FOUND: &str = "Could not find";

#[derive(Debug)]
pub struct LastpassProvider {
    config: LastpassConfig,
    program: OsString,
    search_path: Option<OsString>,
    synced: AtomicBool,
}

impl LastpassProvider {
    pub fn new(config: LastpassConfig) -> Self {
        Self {
            config,
            program: OsString::from("lpass"),
            search_path: None,
            synced: AtomicBool::new(false),
        }
    }

    /// Runs `program` instead of `lpass` (a path, or a name looked up on the search path).
    pub fn with_program(mut self, program: impl Into<OsString>) -> Self {
        self.program = program.into();
        self
    }

    /// Looks programs up in `path` instead of `$PATH`.
    pub fn with_search_path(mut self, path: Option<OsString>) -> Self {
        self.search_path = path;
        self
    }

    pub fn username(&self) -> Option<&str> {
        self.config.username.as_deref()
    }

    fn exe(&self) -> Result<PathBuf> {
        find_program(&self.program, self.search_path.as_deref()).ok_or_else(|| {
            SecretError::Unavailable {
                provider: LASTPASS,
                reason: format!("`{}` is not on PATH", self.program.to_string_lossy()),
            }
            .into()
        })
    }

    fn command(&self) -> Result<Command> {
        let mut c = Command::new(self.exe()?);
        // Never let a subcommand stop to ask on the terminal: only `login` may do that.
        c.stdin(Stdio::null());
        Ok(c)
    }

    /// Runs `lpass <args>` with `stdin` (or nothing) as input, capturing its output.
    fn run(&self, args: &[&OsStr], stdin: Option<&[u8]>) -> Result<Captured> {
        let mut c = self.command()?;
        c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if stdin.is_some() {
            c.stdin(Stdio::piped());
        }
        let what = || format!("running `lpass {}`", args_shown(args));
        let mut child = c.spawn().with_context(what)?;
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().expect("stdin is piped");
            pipe.write_all(input).with_context(what)?;
        }
        let out = child.wait_with_output().with_context(what)?;
        Ok(Captured::from(out))
    }

    fn logged_in(&self) -> Result<bool> {
        let out = self.run(&[OsStr::new("status"), OsStr::new("-q")], None)?;
        Ok(out.success)
    }

    /// One `lpass sync` per provider (so per run), before the first read.
    fn sync_once(&self) -> Result<()> {
        if self.synced.load(Ordering::SeqCst) {
            return Ok(());
        }
        let out = self.run(&[OsStr::new("sync")], None)?;
        if !out.success {
            bail!("`lpass sync` failed: {}", first_line(&out.stderr));
        }
        self.synced.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Every item matching any of `names` (`lpass` names: `Folder/Item`, a bare name or an id):
    /// one call for the batch, or one per name when the batch fails.
    fn show_json(&self, names: &BTreeSet<String>) -> Result<Vec<Item>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<&OsStr> = ["show", "--sync=no", "--json"]
            .iter()
            .map(OsStr::new)
            .collect();
        args.extend(names.iter().map(OsStr::new));
        let out = self.run(&args, None)?;
        if out.success {
            return parse_items(&out.stdout);
        }
        let batch_error = first_line(&out.stderr);
        if names.len() == 1 {
            return if batch_error.contains(NOT_FOUND) {
                Ok(Vec::new())
            } else {
                Err(anyhow::anyhow!("`lpass show` failed: {batch_error}"))
            };
        }
        let mut items = Vec::new();
        for name in names {
            items.extend(self.show_json(&BTreeSet::from([name.clone()]))?);
        }
        Ok(items)
    }

    /// `lpass show --field=<name>`: `None` when the item has no such field.
    fn show_field(&self, id: &str, field: &str) -> Result<Option<SecretValue>> {
        let flag = format!("--field={field}");
        let out = self.run(
            &[
                OsStr::new("show"),
                OsStr::new("--sync=no"),
                OsStr::new(&flag),
                OsStr::new(id),
            ],
            None,
        )?;
        if !out.success {
            let err = first_line(&out.stderr);
            if err.contains(NOT_FOUND) {
                return Ok(None);
            }
            bail!("`lpass show --field` failed: {err}");
        }
        let mut bytes = out.stdout.to_vec();
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        Ok((!bytes.is_empty()).then(|| SecretValue::new(bytes)))
    }

    /// The attachment of item `id` named `filename`: its id from the item's listing, then its
    /// bytes. `None` when there is no such attachment.
    fn attachment(&self, id: &str, filename: &str) -> Result<Option<SecretValue>> {
        let out = self.run(
            &[
                OsStr::new("show"),
                OsStr::new("--sync=no"),
                OsStr::new("--color=never"),
                OsStr::new(id),
            ],
            None,
        )?;
        if !out.success {
            bail!("`lpass show` failed: {}", first_line(&out.stderr));
        }
        let listing = String::from_utf8_lossy(&out.stdout);
        let att_ids: Vec<String> = listing
            .lines()
            .filter_map(|l| l.split_once(": "))
            .filter(|(a, name)| a.starts_with("att-") && *name == filename)
            .map(|(a, _)| a.to_string())
            .collect();
        drop(listing);
        let att = match att_ids.as_slice() {
            [] => return Ok(None),
            [one] => one,
            _ => bail!("item {id} has several attachments named `{filename}`"),
        };
        let flag = format!("--attach={att}");
        let out = self.run(
            &[
                OsStr::new("show"),
                OsStr::new("--sync=no"),
                OsStr::new("--quiet"),
                OsStr::new(&flag),
                OsStr::new(id),
            ],
            None,
        )?;
        if !out.success {
            bail!("`lpass show --attach` failed: {}", first_line(&out.stderr));
        }
        Ok(Some(SecretValue::new(out.stdout.to_vec())))
    }

    fn value(&self, item: &Item, field: &Field<'_>) -> Result<Option<SecretValue>> {
        let text = |s: &str| (!s.is_empty()).then(|| SecretValue::from(s.to_string()));
        Ok(match field {
            Field::Password => text(&item.password),
            Field::Username => text(&item.username),
            Field::Notes => text(&item.note),
            Field::Custom(name) => match &item.fields {
                Some(fields) => fields
                    .iter()
                    .find(|f| f.name == *name)
                    .and_then(|f| text(&f.value)),
                // This CLI does not print fields in JSON: ask for the one field.
                None => self.show_field(&item.id, name)?,
            },
            Field::Attach(file) => self.attachment(&item.id, file)?,
        })
    }
}

/// The item a ref names, among `items`: `Ok(None)` when none matches.
fn find<'a>(items: &'a [Item], r: &SecretRef, sel: &Selector<'_>) -> Result<Option<&'a Item>> {
    let mut matches: Vec<&Item> = match sel {
        Selector::Id(id) => items.iter().filter(|i| i.id == *id).collect(),
        Selector::Name(name) => {
            let full: Vec<&Item> = items.iter().filter(|i| i.full_name() == *name).collect();
            if full.is_empty() {
                items.iter().filter(|i| i.name == *name).collect()
            } else {
                full
            }
        }
    };
    matches.sort_by(|a, b| a.id.cmp(&b.id));
    matches.dedup_by(|a, b| a.id == b.id);
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one)),
        many => {
            let ids: Vec<&str> = many.iter().map(|i| i.id.as_str()).collect();
            let field = r.locator().rsplit_once('#').map_or("", |(_, f)| f);
            bail!(
                "`{r}` matches {} items (ids {}); name one as `lastpass://id/<n>#{field}`",
                many.len(),
                ids.join(", ")
            )
        }
    }
}

impl SecretProvider for LastpassProvider {
    fn scheme(&self) -> &'static str {
        LASTPASS
    }

    fn capabilities(&self) -> Caps {
        Caps {
            read: true,
            write: true,
            files: true,
            write_files: false,
            batch: true,
            needs_unlock: true,
        }
    }

    fn available(&self) -> Result<bool> {
        Ok(find_program(&self.program, self.search_path.as_deref()).is_some())
    }

    fn unlock(&mut self, tty: &mut dyn Interactive) -> Result<()> {
        if self.logged_in()? {
            return Ok(());
        }
        let hint = format!(
            "run `lpass login --trust {}`",
            self.username().unwrap_or("<username>")
        );
        if !tty.is_interactive() {
            bail!("not logged in to LastPass; {hint} first");
        }
        let Some(user) = self.config.username.clone() else {
            bail!(
                "not logged in to LastPass and no username to log in with: set \
                 `providers.lastpass.username` in secrets.yml, or {hint} yourself"
            );
        };
        // On the real terminal: master password and MFA prompts belong to the user.
        let status = self
            .command()?
            .args(["login", "--trust", &user])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("running `lpass login`")?;
        if !status.success() {
            bail!("`lpass login --trust {user}` failed");
        }
        if !self.logged_in()? {
            bail!("still not logged in after `lpass login`; {hint}");
        }
        Ok(())
    }

    fn resolve(&self, refs: &[SecretRef]) -> Result<Vec<Option<SecretValue>>> {
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: Vec<(Selector<'_>, Field<'_>)> =
            refs.iter().map(parse).collect::<Result<_>>()?;
        self.sync_once()?;
        let names: BTreeSet<String> = parsed.iter().map(|(s, _)| s.arg().to_string()).collect();
        let items = self.show_json(&names)?;
        refs.iter()
            .zip(&parsed)
            .map(|(r, (sel, field))| match find(&items, r, sel)? {
                Some(item) => self.value(item, field),
                None => Ok(None),
            })
            .collect()
    }

    fn store(&self, r: &SecretRef, v: &SecretValue) -> Result<()> {
        let (sel, field) = parse(r)?;
        let flag = match field {
            Field::Password => "--password".to_string(),
            Field::Username => "--username".to_string(),
            Field::Notes => "--notes".to_string(),
            Field::Custom(name) => format!("--field={name}"),
            Field::Attach(_) => bail!(
                "`{r}`: lpass cannot add attachments; attach the file to the item in LastPass"
            ),
        };
        if v.expose_str().is_none() {
            bail!("`{r}`: LastPass fields hold text; this value is not UTF-8");
        }
        self.sync_once()?;
        let items = self.show_json(&BTreeSet::from([sel.arg().to_string()]))?;
        let (verb, target) = match find(&items, r, &sel)? {
            Some(item) => ("edit", item.id.clone()),
            None => match sel {
                Selector::Id(id) => bail!("`{r}`: there is no LastPass item with id {id}"),
                Selector::Name(name) => ("add", name.to_string()),
            },
        };
        let out = self.run(
            &[
                OsStr::new(verb),
                OsStr::new("--sync=now"),
                OsStr::new("--non-interactive"),
                OsStr::new(&flag),
                OsStr::new(&target),
            ],
            Some(v.expose()),
        )?;
        if !out.success {
            bail!("`lpass {verb}` failed: {}", first_line(&out.stderr));
        }
        Ok(())
    }

    fn location(&self) -> Option<String> {
        Some(match self.username() {
            Some(u) => format!("LastPass ({u})"),
            None => "LastPass".to_string(),
        })
    }
}

/// Output of one `lpass` run; zeroed when dropped.
struct Captured {
    success: bool,
    stdout: Zeroizing<Vec<u8>>,
    stderr: Vec<u8>,
}

impl From<Output> for Captured {
    fn from(o: Output) -> Self {
        Self {
            success: o.status.success(),
            stdout: Zeroizing::new(o.stdout),
            stderr: o.stderr,
        }
    }
}

fn args_shown(args: &[&OsStr]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Which item a ref names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selector<'a> {
    Id(&'a str),
    Name(&'a str),
}

impl Selector<'_> {
    /// The argument `lpass show` takes for it.
    fn arg(&self) -> &str {
        match self {
            Selector::Id(id) | Selector::Name(id) => id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field<'a> {
    Password,
    Username,
    Notes,
    Custom(&'a str),
    Attach(&'a str),
}

/// Splits a (validated) `lastpass://` locator into item and field.
fn parse(r: &SecretRef) -> Result<(Selector<'_>, Field<'_>)> {
    if r.provider() != LASTPASS {
        return Err(SecretError::WrongProvider {
            reference: r.to_string(),
            expected: LASTPASS,
            actual: r.provider().to_string(),
        }
        .into());
    }
    let Some((item, field)) = r.locator().rsplit_once('#') else {
        bail!("`{r}`: missing `#field`");
    };
    let sel = match item.strip_prefix("id/") {
        Some(id) => Selector::Id(id),
        None => Selector::Name(item),
    };
    let field = match field {
        "password" => Field::Password,
        "username" => Field::Username,
        "notes" => Field::Notes,
        f => match (f.strip_prefix("field="), f.strip_prefix("attach=")) {
            (Some(n), _) => Field::Custom(n),
            (_, Some(n)) => Field::Attach(n),
            _ => bail!("`{r}`: unknown field selector `#{f}`"),
        },
    };
    Ok((sel, field))
}

/// One entry of `lpass show --json`. Values are zeroed when dropped.
#[derive(Deserialize, Default)]
struct Item {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    fullname: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    note: String,
    /// Custom fields; absent from the JSON of older CLIs.
    #[serde(default)]
    fields: Option<Vec<ItemField>>,
}

impl Item {
    fn full_name(&self) -> String {
        if !self.fullname.is_empty() {
            self.fullname.clone()
        } else if self.group.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.group, self.name)
        }
    }
}

impl Drop for Item {
    fn drop(&mut self) {
        self.username.zeroize();
        self.password.zeroize();
        self.note.zeroize();
    }
}

#[derive(Deserialize)]
struct ItemField {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

impl Drop for ItemField {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

fn parse_items(stdout: &[u8]) -> Result<Vec<Item>> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    // The parse error would quote the offending text: never show it.
    serde_json::from_slice(stdout)
        .map_err(|_| anyhow::anyhow!("`lpass show --json` printed something that is not JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> SecretRef {
        SecretRef::parse(s).unwrap()
    }

    #[test]
    fn refs_split_into_item_and_field() {
        let cases = [
            (
                "lastpass://Dev/GitLab#password",
                Selector::Name("Dev/GitLab"),
                Field::Password,
            ),
            (
                "lastpass://Shared-Team/Sub/Item#field=API Token",
                Selector::Name("Shared-Team/Sub/Item"),
                Field::Custom("API Token"),
            ),
            (
                "lastpass://id/42#attach=work.ovpn",
                Selector::Id("42"),
                Field::Attach("work.ovpn"),
            ),
            ("lastpass://Top#notes", Selector::Name("Top"), Field::Notes),
        ];
        for (text, sel, field) in cases {
            let rf = r(text);
            assert_eq!(parse(&rf).unwrap(), (sel, field), "{text}");
        }
        assert!(parse(&r("envfile:A")).is_err());
    }

    fn item(id: &str, group: &str, name: &str) -> Item {
        let mut i = Item::default();
        i.id = id.into();
        i.name = name.into();
        i.group = group.into();
        i
    }

    #[test]
    fn items_match_by_full_name_then_name_then_id() {
        let items = vec![
            item("1", "Dev", "GitLab"),
            item("2", "Ops", "GitLab"),
            item("3", "", "Solo"),
        ];
        let rf = r("lastpass://Dev/GitLab#password");
        let (sel, _) = parse(&rf).unwrap();
        assert_eq!(find(&items, &rf, &sel).unwrap().unwrap().id, "1");
        let rf = r("lastpass://Solo#password");
        let (sel, _) = parse(&rf).unwrap();
        assert_eq!(find(&items, &rf, &sel).unwrap().unwrap().id, "3");
        let rf = r("lastpass://id/2#password");
        let (sel, _) = parse(&rf).unwrap();
        assert_eq!(find(&items, &rf, &sel).unwrap().unwrap().id, "2");
        let rf = r("lastpass://GitLab#username");
        let (sel, _) = parse(&rf).unwrap();
        let e = find(&items, &rf, &sel).err().unwrap().to_string();
        assert!(
            e.contains("matches 2 items (ids 1, 2)") && e.contains("lastpass://id/<n>#username"),
            "{e}"
        );
        let rf = r("lastpass://Nope/X#password");
        let (sel, _) = parse(&rf).unwrap();
        assert!(find(&items, &rf, &sel).unwrap().is_none());
    }

    #[test]
    fn json_parse_errors_never_quote_the_output() {
        let e = parse_items(b"{\"password\": \"hunter2\"").err().unwrap();
        assert!(!e.to_string().contains("hunter2"));
        assert!(parse_items(b"  \n").unwrap().is_empty());
        let items = parse_items(
            br#"[{"id":"7","name":"GitLab","fullname":"Dev/GitLab","group":"Dev","username":"u","password":"p","url":"","note":"","last_touch":"0"}]"#,
        )
        .unwrap();
        assert_eq!(items[0].full_name(), "Dev/GitLab");
        assert!(items[0].fields.is_none());
    }
}
