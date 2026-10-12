//! Asking the user before applying, and for variables declared with a `prompt:`. Behind small
//! traits so the tty renderer can own the questions later (and tests can answer them).

use std::io::{BufRead, Write};
use std::path::Path;

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use dot_core::event::StepViewState;
use dot_core::graph::Resolution;
use dot_core::workspace::Workspace;
use dot_core::{EventSink, Plan, Prepare, SecretUse, Status};

/// Asks yes/no questions.
pub trait Prompter {
    /// `default` is the answer to an empty reply.
    fn confirm(&mut self, question: &str, default: bool) -> std::io::Result<bool>;
}

/// A source of input lines.
pub trait ReadLine {
    /// Appends one line (with its newline) to `buf`; `Ok(0)` at end of input.
    fn read_line(&mut self, buf: &mut String) -> std::io::Result<usize>;
}

impl<R: BufRead> ReadLine for R {
    fn read_line(&mut self, buf: &mut String) -> std::io::Result<usize> {
        BufRead::read_line(self, buf)
    }
}

/// Standard input, locked only while one line is read. A lock held for the whole run would
/// deadlock anything else on this thread that reads the terminal later, such as the hidden
/// input asking for a missing secret (`console` reads it through `Stdin::read_line`).
pub struct StdinLines;

impl ReadLine for StdinLines {
    fn read_line(&mut self, buf: &mut String) -> std::io::Result<usize> {
        std::io::stdin().read_line(buf)
    }
}

/// Reads the answer from a line of input; the question goes to `out`.
pub struct LinePrompter<R, W> {
    input: R,
    out: W,
}

impl<R: ReadLine, W: Write> LinePrompter<R, W> {
    pub fn new(input: R, out: W) -> Self {
        Self { input, out }
    }
}

impl<R: ReadLine, W: Write> Prompter for LinePrompter<R, W> {
    fn confirm(&mut self, question: &str, default: bool) -> std::io::Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            write!(self.out, "{question} {hint} ")?;
            self.out.flush()?;
            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 {
                // End of input: take nothing for granted.
                writeln!(self.out)?;
                return Ok(false);
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "" => return Ok(default),
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => writeln!(self.out, "Please answer y or n.")?,
            }
        }
    }
}

/// Asks for a variable's value.
pub trait VarAsker {
    /// One line of input after `question`; without echo when `hidden`.
    fn ask(&mut self, question: &str, hidden: bool) -> std::io::Result<String>;
    /// A line of feedback (why an answer was refused, where answers were saved).
    fn say(&mut self, line: &str);
}

/// Asks on the terminal (`console` reads hidden input from the tty itself).
pub struct TermAsker(pub console::Term);

impl VarAsker for TermAsker {
    fn ask(&mut self, question: &str, hidden: bool) -> std::io::Result<String> {
        self.0.write_str(question)?;
        if hidden {
            self.0.read_secure_line()
        } else {
            self.0.read_line()
        }
    }

    fn say(&mut self, line: &str) {
        let _ = self.0.write_line(line);
    }
}

/// How many times an answer may be refused before giving up.
const ATTEMPTS: usize = 3;

/// Asks for every variable declared with a `prompt:` that has no value (hidden input for
/// sensitive ones), checks each answer against its declaration (asking again when it does not
/// fit), sets the answers for this run and writes the `persist: local` ones to `local_env`
/// (owner-only when any is sensitive). With `dry_run` nothing is written. Returns the answered
/// sensitive values, for the redactor.
pub fn ask_vars(
    ws: &mut Workspace,
    res: &Resolution,
    asker: &mut dyn VarAsker,
    local_env: &Path,
    dry_run: bool,
) -> Result<Vec<String>> {
    let prompts = ws.var_prompts(res);
    let mut answers = Vec::new();
    let mut keep: Vec<(String, String)> = Vec::new();
    let mut keep_private = false;
    let mut sensitive = Vec::new();
    for p in prompts {
        let question = format!(
            "{} [{}]{}: ",
            p.question.trim_end_matches([':', ' ']),
            p.name,
            if p.sensitive { " (hidden)" } else { "" }
        );
        let mut attempt = 0;
        let value = loop {
            attempt += 1;
            let answer = asker.ask(&question, p.sensitive)?;
            let answer = answer.trim_end_matches(['\r', '\n']).to_string();
            let problems = if answer.is_empty() {
                vec![format!("`{}` needs a value", p.name)]
            } else {
                ws.answer_problems(res, &p.name, &answer)
            };
            if problems.is_empty() {
                break answer;
            }
            for line in &problems {
                asker.say(line);
            }
            if attempt >= ATTEMPTS {
                bail!("no usable value for variable `{}`", p.name);
            }
        };
        if p.sensitive {
            sensitive.push(value.clone());
        }
        if p.persist {
            keep_private |= p.sensitive;
            keep.push((p.name.clone(), value.clone()));
        }
        answers.push((p.name, value));
    }
    ws.answer(&answers);
    if !keep.is_empty() {
        let names: Vec<&str> = keep.iter().map(|(n, _)| n.as_str()).collect();
        if dry_run {
            asker.say(&format!(
                "dry run: would save {} to {}",
                names.join(", "),
                local_env.display()
            ));
        } else {
            let entries: Vec<(&str, &str)> =
                keep.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
            dot_secrets::persist_env(local_env, &entries, keep_private)?;
            asker.say(&format!(
                "saved {} to {}",
                names.join(", "),
                local_env.display()
            ));
        }
    }
    Ok(sensitive)
}

/// Whether a plan would change anything (or fail): if not, there is nothing to confirm.
pub fn plan_has_work(plan: &Plan) -> bool {
    let view = plan.view();
    view.has_errors()
        || view.bundles.iter().filter(|b| b.skip.is_none()).any(|b| {
            b.steps.iter().any(|s| match &s.state {
                StepViewState::Ready(actions) => {
                    actions.iter().any(|a| a.status != Status::Satisfied)
                }
                _ => false,
            })
        })
}

/// A prepare stage that first shows the plan and asks "Apply?", then runs the real one.
pub struct ConfirmPrepare<'a> {
    pub prompter: &'a mut dyn Prompter,
    /// Prints the preview.
    pub preview: &'a mut dyn FnMut(&Plan),
    pub inner: &'a mut dyn Prepare,
}

impl Prepare for ConfirmPrepare<'_> {
    fn prepare(&mut self, plan: &Plan, sink: &EventSink) -> Result<()> {
        if plan_has_work(plan) {
            (self.preview)(plan);
            if !self.prompter.confirm("Apply?", true)? {
                bail!("cancelled: nothing was applied");
            }
        }
        self.inner.prepare(plan, sink)
    }

    fn before_bundle(
        &mut self,
        name: &str,
        secrets: &BTreeMap<String, SecretUse>,
        sink: &EventSink,
    ) -> Result<()> {
        self.inner.before_bundle(name, secrets, sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(input: &str, default: bool) -> (bool, String) {
        let mut out = Vec::new();
        let answer = LinePrompter::new(input.as_bytes(), &mut out)
            .confirm("Apply?", default)
            .unwrap();
        (answer, String::from_utf8(out).unwrap())
    }

    #[test]
    fn answers() {
        assert!(ask("\n", true).0);
        assert!(!ask("\n", false).0);
        assert!(!ask("n\n", true).0);
        assert!(ask("YES\n", false).0);
        let (a, out) = ask("maybe\ny\n", false);
        assert!(a);
        assert_eq!(out, "Apply? [y/N] Please answer y or n.\nApply? [y/N] ");
        assert!(!ask("", true).0, "end of input never means yes");
    }

    struct Scripted(bool, usize);
    impl Prompter for Scripted {
        fn confirm(&mut self, _: &str, _: bool) -> std::io::Result<bool> {
            self.1 += 1;
            Ok(self.0)
        }
    }

    struct Inner(bool);
    impl Prepare for Inner {
        fn prepare(&mut self, _: &Plan, _: &EventSink) -> Result<()> {
            self.0 = true;
            Ok(())
        }
    }

    fn plan_with_pending() -> Plan {
        use dot_core::engine::{PlannedAction, PlannedBundle, PlannedState, PlannedStep};
        struct A;
        impl dot_core::Action for A {
            fn describe(&self) -> String {
                "a".into()
            }
            fn status(&self) -> Result<Status> {
                Ok(Status::Pending)
            }
            fn apply(&self, _: &dot_core::RunCtx) -> Result<dot_core::Outcome> {
                Ok(dot_core::Outcome::Changed)
            }
        }
        Plan {
            bundles: vec![PlannedBundle {
                name: "b".into(),
                file: "/b".into(),
                skip: None,
                steps: vec![PlannedStep {
                    id: dot_core::StepId { bundle: 0, step: 0 },
                    kind: "shell",
                    label: "x".into(),
                    state: PlannedState::Ready(vec![PlannedAction {
                        action: Box::new(A),
                        description: "a".into(),
                        status: Status::Pending,
                    }]),
                    secrets: Vec::new(),
                }],
                secrets: Default::default(),
                needed: Default::default(),
            }],
            secrets: Default::default(),
            needed: Default::default(),
        }
    }

    #[test]
    fn confirm_prepare_asks_then_delegates() {
        let plan = plan_with_pending();
        let shown = std::cell::Cell::new(0);
        let mut preview = |_: &Plan| shown.set(shown.get() + 1);
        let mut prompter = Scripted(false, 0);
        let mut inner = Inner(false);
        let mut p = ConfirmPrepare {
            prompter: &mut prompter,
            preview: &mut preview,
            inner: &mut inner,
        };
        let e = p.prepare(&plan, &EventSink::new()).unwrap_err();
        assert!(e.to_string().contains("cancelled"));
        assert!(!inner.0, "declined: secrets are never unlocked");

        let mut prompter = Scripted(true, 0);
        let mut p = ConfirmPrepare {
            prompter: &mut prompter,
            preview: &mut preview,
            inner: &mut inner,
        };
        p.prepare(&plan, &EventSink::new()).unwrap();
        assert!(inner.0);
        assert_eq!(shown.get(), 2);

        // Nothing to do: nothing to ask.
        let empty = Plan {
            bundles: vec![],
            secrets: Default::default(),
            needed: Default::default(),
        };
        let mut prompter = Scripted(false, 0);
        let mut p = ConfirmPrepare {
            prompter: &mut prompter,
            preview: &mut preview,
            inner: &mut inner,
        };
        p.prepare(&empty, &EventSink::new()).unwrap();
        assert_eq!(prompter.1, 0);
    }

    /// Answers from a script; records questions (and whether each was hidden) and feedback.
    #[derive(Default)]
    struct ScriptedAsker {
        answers: Vec<&'static str>,
        asked: Vec<(String, bool)>,
        said: Vec<String>,
    }

    impl VarAsker for ScriptedAsker {
        fn ask(&mut self, question: &str, hidden: bool) -> std::io::Result<String> {
            self.asked.push((question.to_string(), hidden));
            Ok(self.answers.remove(0).to_string())
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
    }

    struct Repo {
        _tmp: tempfile::TempDir,
        root: std::path::PathBuf,
    }

    impl Repo {
        fn new(dot_yml: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            std::fs::create_dir_all(root.join("home")).unwrap();
            std::fs::create_dir_all(root.join("repo/bundles/app")).unwrap();
            std::fs::write(root.join("repo/dot.yml"), dot_yml).unwrap();
            std::fs::write(root.join("repo/bundles/app/bundle.yml"), "steps: []\n").unwrap();
            Self { _tmp: tmp, root }
        }

        fn open(&self) -> (Workspace, Resolution, std::path::PathBuf) {
            let inputs = dot_core::workspace::Inputs {
                home: self.root.join("home"),
                cwd: self.root.join("repo"),
                ..Default::default()
            };
            let ws = Workspace::open(&inputs, |_| {
                dot_core::Facts::new(
                    dot_core::facts::Os::Linux,
                    "x86_64",
                    dot_core::facts::Role::Server,
                )
            })
            .unwrap();
            let res = ws.resolve(&[], false).unwrap();
            (ws, res, inputs.local_env())
        }
    }

    const PROMPTING: &str = concat!(
        "vars:\n",
        "  email: { prompt: \"Your e-mail:\", persist: local, pattern: '.+@.+' }\n",
        "  gpg_pin: { prompt: \"GPG PIN\", persist: local, sensitive: true, type: int }\n",
        "  session: { prompt: \"Session name\" }\n",
    );

    #[test]
    fn prompted_vars_are_asked_checked_and_persisted_once() {
        let repo = Repo::new(PROMPTING);
        let (mut ws, res, local_env) = repo.open();
        let mut asker = ScriptedAsker {
            answers: vec!["", "me@example.com", "12x", "4242", "dev"],
            ..Default::default()
        };
        let sensitive = ask_vars(&mut ws, &res, &mut asker, &local_env, false).unwrap();
        assert_eq!(sensitive, ["4242"]);
        let questions: Vec<(&str, bool)> =
            asker.asked.iter().map(|(q, h)| (q.as_str(), *h)).collect();
        assert_eq!(
            questions,
            [
                ("Your e-mail [email]: ", false),
                ("Your e-mail [email]: ", false),
                ("GPG PIN [gpg_pin] (hidden): ", true),
                ("GPG PIN [gpg_pin] (hidden): ", true),
                ("Session name [session]: ", false),
            ]
        );
        assert!(asker.said[0].contains("needs a value"), "{:?}", asker.said);
        assert!(asker.said[1].contains("expected int"), "{:?}", asker.said);
        assert!(
            !asker.said[1].contains("12x"),
            "sensitive: {:?}",
            asker.said
        );
        assert!(asker.said[2].starts_with("saved email, gpg_pin to "));
        ws.check_vars(&res).unwrap();
        assert_eq!(ws.vars.get("session").unwrap().as_string(), "dev");

        // Persisted (owner-only: it holds the PIN), the one-run answer is not.
        let text = std::fs::read_to_string(&local_env).unwrap();
        assert_eq!(text, "email=\"me@example.com\"\ngpg_pin=\"4242\"\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&local_env).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Next run: only the unpersisted one is asked again.
        let (mut ws, res, _) = repo.open();
        let names: Vec<String> = ws.var_prompts(&res).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["session"]);
        let mut asker = ScriptedAsker {
            answers: vec!["prod"],
            ..Default::default()
        };
        ask_vars(&mut ws, &res, &mut asker, &local_env, false).unwrap();
        assert_eq!(asker.asked.len(), 1);
    }

    #[test]
    fn a_dry_run_asks_but_writes_nothing_and_bad_answers_give_up() {
        let repo = Repo::new(PROMPTING);
        let (mut ws, res, local_env) = repo.open();
        let mut asker = ScriptedAsker {
            answers: vec!["a@b", "1", "s"],
            ..Default::default()
        };
        ask_vars(&mut ws, &res, &mut asker, &local_env, true).unwrap();
        assert!(!local_env.exists());
        assert!(
            asker
                .said
                .last()
                .unwrap()
                .starts_with("dry run: would save")
        );

        let (mut ws, res, _) = repo.open();
        let mut asker = ScriptedAsker {
            answers: vec!["x", "y", "z"],
            ..Default::default()
        };
        let e = ask_vars(&mut ws, &res, &mut asker, &local_env, false).unwrap_err();
        assert!(e.to_string().contains("`email`"), "{e}");
        assert!(!local_env.exists());
    }
}
