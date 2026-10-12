//! Which program runs a `shell:` step's command string (and `unless:` / bundle `check:`).
//!
//! A step picks one with `interpreter:` (else the `defaults: { shell: { interpreter } }` of its
//! bundle, profile or `dot.yml`, else the host default): `sh`, `bash`, `zsh`, `pwsh`,
//! `powershell` or `cmd`. The host default is `sh` on Unix, and `pwsh` (PowerShell 7) on
//! Windows when it is installed, else Windows PowerShell (`powershell`), which every Windows has.
//!
//! PowerShell scripts, multi-line ones included, run as `-NoProfile -NonInteractive
//! -EncodedCommand <base64 of UTF-16LE>` rather than from a temporary `.ps1` file:
//!
//! - nothing is written to disk, so there is no file to clean up after a crash and no window in
//!   which another process could swap the script;
//! - the execution policy applies to script files, not to `-Command` / `-EncodedCommand`, so no
//!   `-ExecutionPolicy Bypass` is needed and a Group Policy that forbids overriding it cannot
//!   break steps;
//! - base64 has no quoting to get wrong: Windows passes one command line string that every
//!   program re-parses its own way, and PowerShell's `-Command` re-parsing of quotes is
//!   notoriously lossy.
//!
//! The price is length: the whole command line is limited to 32767 characters, so a script can
//! be about 12000 characters. [`Interpreter::validate`] rejects longer ones at plan time (put the
//! code in a `.ps1` file in the bundle and run that).
//!
//! Exit codes: `sh -c` exits with the status of its last command. PowerShell's `-Command`
//! would exit 0 or 1 only, so the script is wrapped ([`powershell_script`]) to exit with the
//! last native command's code when the last command failed (1 for a failed cmdlet), and a
//! script's own `exit N` exits with `N`.

use std::process::Command;

/// Longest command line Windows accepts (`CreateProcess`), in UTF-16 units.
pub const MAX_COMMAND_LINE: usize = 32767;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Interpreter {
    /// `sh -c <script>`
    Sh,
    /// `bash -c <script>`
    Bash,
    /// `zsh -c <script>`
    Zsh,
    /// `pwsh -NoProfile -NonInteractive -EncodedCommand <script>` (PowerShell 7)
    Pwsh,
    /// `powershell` (Windows PowerShell 5.1), as `pwsh`
    PowerShell,
    /// `cmd /D /S /C "<script>"`
    Cmd,
}

impl Interpreter {
    pub const ALL: [Interpreter; 6] = [
        Interpreter::Sh,
        Interpreter::Bash,
        Interpreter::Zsh,
        Interpreter::Pwsh,
        Interpreter::PowerShell,
        Interpreter::Cmd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Interpreter::Sh => "sh",
            Interpreter::Bash => "bash",
            Interpreter::Zsh => "zsh",
            Interpreter::Pwsh => "pwsh",
            Interpreter::PowerShell => "powershell",
            Interpreter::Cmd => "cmd",
        }
    }

    pub fn parse(s: &str) -> Option<Interpreter> {
        Self::ALL.into_iter().find(|i| i.as_str() == s)
    }

    /// The program that is run.
    pub fn program(self) -> &'static str {
        self.as_str()
    }

    /// The interpreter for the machine we are running on: `sh` on Unix; on Windows `pwsh` when
    /// it is on `PATH`, else `powershell`.
    pub fn host_default() -> Self {
        if cfg!(windows) {
            Self::windows_default(crate::exec::which("pwsh").is_some())
        } else {
            Interpreter::Sh
        }
    }

    /// The Windows default: PowerShell 7 when installed, else Windows PowerShell.
    pub fn windows_default(pwsh_installed: bool) -> Self {
        if pwsh_installed {
            Interpreter::Pwsh
        } else {
            Interpreter::PowerShell
        }
    }

    /// Only Windows has it (`pwsh` also runs on macOS and Linux).
    pub fn windows_only(self) -> bool {
        matches!(self, Interpreter::PowerShell | Interpreter::Cmd)
    }

    /// Whether `script` can be run faithfully; call at plan time so a problem fails before
    /// anything runs.
    ///
    /// `cmd /C` ends the command at the first line break, so the rest of a multi-line script
    /// would silently not run while the step still reported success: such scripts are rejected
    /// (use `pwsh`, or put the lines in a `.cmd` file and run that). PowerShell scripts must fit
    /// the command line once encoded.
    pub fn validate(self, script: &str) -> Result<(), String> {
        match self {
            Interpreter::Sh | Interpreter::Bash | Interpreter::Zsh => Ok(()),
            Interpreter::Cmd => {
                if trim_line_end(script).contains(['\n', '\r']) {
                    Err(
                        "cmd.exe runs only the first line of a multi-line command; use \
                         `interpreter: pwsh`, join the lines with `&&`, or put them in a .cmd \
                         file and run that"
                            .to_string(),
                    )
                } else {
                    Ok(())
                }
            }
            Interpreter::Pwsh | Interpreter::PowerShell => {
                let len: usize = self
                    .argv(script, false)
                    .iter()
                    .map(|a| a.encode_utf16().count() + 1)
                    .sum();
                if len > MAX_COMMAND_LINE {
                    Err(format!(
                        "this {} script is too long to pass on a command line ({} characters \
                         encoded, the limit is {MAX_COMMAND_LINE}); put it in a .ps1 file in \
                         the bundle and run that",
                        self.as_str(),
                        len
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }

    /// How a script refers to environment variable `name`. Secrets reach a command this way
    /// (the value in the child's environment, only the reference in its command line): `sh`
    /// expands `${NAME}` inside double quotes and unquoted, PowerShell `${env:NAME}` likewise,
    /// `cmd` expands `%NAME%` everywhere. Inside single quotes (`sh`, PowerShell) nothing
    /// expands, so `secret()` must not be used there.
    pub fn env_ref(self, name: &str) -> String {
        match self {
            Interpreter::Sh | Interpreter::Bash | Interpreter::Zsh => format!("${{{name}}}"),
            Interpreter::Pwsh | Interpreter::PowerShell => format!("${{env:{name}}}"),
            Interpreter::Cmd => format!("%{name}%"),
        }
    }

    /// Program and arguments that run `script`. `interactive`: the command gets the terminal
    /// (PowerShell then may prompt; otherwise it runs `-NonInteractive`). `cmd` is special:
    /// see [`Interpreter::command`].
    pub fn argv(self, script: &str, interactive: bool) -> Vec<String> {
        let mut v = vec![self.program().to_string()];
        match self {
            Interpreter::Sh | Interpreter::Bash | Interpreter::Zsh => {
                v.extend(["-c".to_string(), script.to_string()]);
            }
            Interpreter::Pwsh | Interpreter::PowerShell => {
                v.push("-NoProfile".into());
                if !interactive {
                    v.push("-NonInteractive".into());
                }
                v.push("-EncodedCommand".into());
                v.push(encode_command(&powershell_script(script)));
            }
            Interpreter::Cmd => {
                v.extend(["/D", "/S", "/C"].map(String::from));
                v.push(trim_line_end(script).to_string());
            }
        }
        v
    }

    /// A `Command` that runs `script` without a terminal; the caller sets cwd, env and stdio.
    /// `script` should have passed [`Interpreter::validate`].
    pub fn command(self, script: &str) -> Command {
        self.command_with(script, false)
    }

    /// Like [`Interpreter::command`]; `interactive` as for [`Interpreter::argv`].
    pub fn command_with(self, script: &str, interactive: bool) -> Command {
        let argv = self.argv(script, interactive);
        let mut c = Command::new(&argv[0]);
        match self {
            #[cfg(windows)]
            Interpreter::Cmd => {
                // cmd.exe does its own parsing: pass the command line through untouched.
                use std::os::windows::process::CommandExt;
                c.raw_arg(cmd_line(script));
            }
            _ => {
                c.args(&argv[1..]);
            }
        }
        c
    }
}

impl std::fmt::Display for Interpreter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The arguments cmd.exe receives: `/D` skips AutoRun, and `/S` with the whole script in one
/// pair of quotes makes cmd strip exactly those outer quotes, so a script that itself starts and
/// ends with quotes (`"C:\Program Files\x.exe" "arg"`) is not mangled.
pub fn cmd_line(script: &str) -> String {
    format!("/D /S /C \"{}\"", trim_line_end(script))
}

/// A YAML block scalar (`cmd: |`) ends with a newline; that is not a second line.
fn trim_line_end(script: &str) -> &str {
    script.trim_end_matches(['\n', '\r'])
}

/// The script PowerShell runs: `script` between a reset of `$LASTEXITCODE` and a trailer that
/// turns a failed last command into a non-zero exit code (its own code for a native command,
/// 1 for a cmdlet), so the step fails the way `sh -c` would. `exit N` inside `script` still
/// exits with `N`.
pub fn powershell_script(script: &str) -> String {
    format!(
        "$global:LASTEXITCODE = 0\n{}\nif (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }} else {{ exit 1 }} }}\nexit 0\n",
        trim_line_end(script)
    )
}

/// `-EncodedCommand`'s format: base64 of the script's UTF-16LE bytes.
pub fn encode_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&bytes)
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn sh_runs_script() {
        let out = Interpreter::Sh.command("echo $((1 + 2))").output().unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "3");
    }

    #[test]
    fn names_round_trip_and_defaults() {
        for i in Interpreter::ALL {
            assert_eq!(Interpreter::parse(i.as_str()), Some(i));
        }
        assert_eq!(Interpreter::parse("fish"), None);
        assert_eq!(Interpreter::windows_default(true), Interpreter::Pwsh);
        assert_eq!(Interpreter::windows_default(false), Interpreter::PowerShell);
        if cfg!(unix) {
            assert_eq!(Interpreter::host_default(), Interpreter::Sh);
        }
        assert!(Interpreter::Cmd.windows_only() && Interpreter::PowerShell.windows_only());
        assert!(!Interpreter::Pwsh.windows_only());
    }

    #[test]
    fn env_refs() {
        assert_eq!(Interpreter::Sh.env_ref("DOT_SECRET_x"), "${DOT_SECRET_x}");
        assert_eq!(Interpreter::Zsh.env_ref("DOT_SECRET_x"), "${DOT_SECRET_x}");
        assert_eq!(Interpreter::Cmd.env_ref("DOT_SECRET_x"), "%DOT_SECRET_x%");
        assert_eq!(
            Interpreter::Pwsh.env_ref("DOT_SECRET_x"),
            "${env:DOT_SECRET_x}"
        );
    }

    #[test]
    fn cmd_rejects_multi_line_scripts() {
        let e = Interpreter::Cmd
            .validate("mkdir foo\ncopy a b")
            .unwrap_err();
        assert!(e.contains("first line") && e.contains("pwsh"), "{e}");
        assert!(Interpreter::Cmd.validate("mkdir foo\r\ncopy a b").is_err());
        // A block scalar's trailing newline is fine.
        assert!(Interpreter::Cmd.validate("mkdir foo && copy a b\n").is_ok());
        assert!(Interpreter::Sh.validate("a\nb").is_ok());
        assert!(Interpreter::Pwsh.validate("a\nb").is_ok());
    }

    #[test]
    fn cmd_line_quotes_the_whole_script_once() {
        assert_eq!(
            cmd_line("\"C:\\Program Files\\x.exe\" \"arg\"\n"),
            "/D /S /C \"\"C:\\Program Files\\x.exe\" \"arg\"\""
        );
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // What `[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('dir'))` gives.
        assert_eq!(encode_command("dir"), "ZABpAHIA");
    }

    fn decode(b64: &str) -> String {
        const ABC: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut bits = Vec::new();
        for c in b64.chars().filter(|c| *c != '=') {
            let v = ABC.find(c).unwrap() as u32;
            bits.extend((0..6).rev().map(|i| (v >> i) & 1));
        }
        let bytes: Vec<u8> = bits
            .chunks(8)
            .filter(|c| c.len() == 8)
            .map(|c| c.iter().fold(0u8, |a, b| (a << 1) | *b as u8))
            .collect();
        let units: Vec<u16> = bytes
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn powershell_argv_encodes_the_wrapped_script() {
        let script = "Write-Host 'héllo'\nnpm install -g x\n";
        let argv = Interpreter::Pwsh.argv(script, false);
        assert_eq!(
            argv[..4],
            ["pwsh", "-NoProfile", "-NonInteractive", "-EncodedCommand"]
        );
        let decoded = decode(&argv[4]);
        assert_eq!(decoded, powershell_script(script));
        assert!(
            decoded.starts_with(
                "$global:LASTEXITCODE = 0\nWrite-Host 'héllo'\nnpm install -g x\nif (-not $?)"
            ),
            "{decoded}"
        );
        assert!(decoded.ends_with("exit 0\n"));
        let interactive = Interpreter::PowerShell.argv("Read-Host", true);
        assert_eq!(
            interactive[..3],
            ["powershell", "-NoProfile", "-EncodedCommand"]
        );
        assert_eq!(
            Interpreter::Bash.argv("echo hi", false),
            ["bash", "-c", "echo hi"]
        );
        assert_eq!(
            Interpreter::Cmd.argv("echo hi\n", false),
            ["cmd", "/D", "/S", "/C", "echo hi"]
        );
    }

    #[test]
    fn overlong_powershell_scripts_are_rejected_at_plan_time() {
        let fits = "x".repeat(10_000);
        assert!(Interpreter::Pwsh.validate(&fits).is_ok());
        let e = Interpreter::Pwsh.validate(&"x".repeat(13_000)).unwrap_err();
        assert!(e.contains(".ps1 file"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn bash_and_zsh_run_when_installed() {
        for i in [Interpreter::Bash, Interpreter::Zsh] {
            if crate::exec::which(i.program()).is_none() {
                continue;
            }
            let out = i.command("exit 7").output().unwrap();
            assert_eq!(out.status.code(), Some(7), "{i}");
        }
    }

    /// Runs PowerShell for real where it is installed (Windows; pwsh on a dev machine).
    #[test]
    fn powershell_propagates_exit_codes_when_installed() {
        let candidates = [Interpreter::Pwsh, Interpreter::PowerShell];
        let Some(ps) = candidates
            .into_iter()
            .find(|i| crate::exec::which(i.program()).is_some())
        else {
            return;
        };
        let code = |script: &str| ps.command(script).output().unwrap().status.code();
        assert_eq!(code("$x = 1\nWrite-Output $x"), Some(0));
        assert_eq!(code("Write-Output a\nexit 5\nWrite-Output b"), Some(5));
        assert_eq!(
            code("Get-Item C:/dot/no/such/path/x -ErrorAction Continue"),
            Some(1)
        );
        let out = ps
            .command("Write-Output \"$env:DOT_TEST_V\"\nWrite-Output two")
            .env("DOT_TEST_V", "one")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::trim)
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn cmd_runs_a_quoted_program() {
        let out = Interpreter::Cmd
            .command("\"cmd\" /C \"echo hi\"")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }
}
