mod adopt;
mod cloudinit;
mod commands;
mod doctor;
mod envcmd;
mod handoff;
mod hints;
mod ops;
mod prompt;
mod remote;
mod remove;
mod render;
mod secrets;
mod selfupdate;
mod transfer;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Exit code for config and usage errors (bad YAML, missing files, unresolvable graph).
pub const EXIT_CONFIG: u8 = 2;

#[derive(Parser)]
#[command(
    name = "dot",
    version,
    about = "Bootstrap and maintain a machine from a dotfiles repo"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply bundles: the profile's selection, or the named bundles and their dependencies.
    Apply(ApplyArgs),
    /// Show what `apply` would do: the resolved bundle order and each step, changing nothing.
    Plan(PlanArgs),
    /// Validate dot.yml, every bundle and the bundle graph (or the given files) and report
    /// every error.
    Check(CheckArgs),
    /// Show the bundle dependency graph.
    Graph(GraphArgs),
    /// Explain why a bundle is (or is not) part of a run.
    Why(WhyArgs),
    /// Show every variable with its value and where it came from (sensitive values redacted).
    Vars(VarsArgs),
    /// Print the JSON Schema for bundle.yml, dot.yml or secrets.yml.
    Schema {
        #[arg(value_enum, default_value_t = SchemaKind::Bundle)]
        which: SchemaKind,
    },
    /// Print detected platform facts.
    Facts,
    /// Secrets: where they live and whether they are all there (never their values).
    #[command(subcommand)]
    Secrets(SecretsCommand),
    /// Print (or write) a file for your shell to source: every non-sensitive variable, plus
    /// what `path` / `env` steps have set up.
    Env(envcmd::EnvArgs),
    /// Report drift (anything the selected bundles would change) and orphans (what earlier
    /// runs made that the configuration no longer has). Exits 1 when not in sync.
    Status(StatusArgs),
    /// Show how the machine differs from the configuration: file diffs (secrets never shown),
    /// link targets, missing and outdated packages.
    Diff(DiffArgs),
    /// Check what dot needs: repository, profile, package manager, sudo, secrets, git, clock,
    /// state store, PATH, locks.
    Doctor(DoctorArgs),
    /// Move an existing file or directory into a bundle of the repository and link it back.
    Adopt(AdoptArgs),
    /// Undo what dot recorded for a bundle: remove the links and files it made, put back what
    /// was there before.
    Remove(RemoveArgs),
    /// Update dot itself to the latest GitHub release, checked against the release's
    /// SHA256SUMS, and print the old and new versions.
    #[command(name = "self-update")]
    SelfUpdate(selfupdate::SelfUpdateArgs),
    /// Print cloud-init user data that installs dot and applies a profile on first boot.
    CloudInit(cloudinit::CloudInitArgs),
    /// What `dot apply --host` asks of the machine it sets up: its facts, pinned profile and
    /// the names (never values) in its local.env, as JSON.
    #[command(name = "host-info", hide = true)]
    HostInfo,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Bundles to check (default: the profile's selection; orphans are only reported then).
    #[arg(value_name = "BUNDLE")]
    pub bundles: Vec<String>,
    #[command(flatten)]
    pub repo: RepoArgs,
    /// Only the named bundles: no dependencies, no implicit bundles (as `dot apply --no-deps`).
    #[arg(long, requires = "bundles")]
    pub no_deps: bool,
    /// Also show the repository, profile and bundle order.
    #[arg(short, long)]
    pub verbose: bool,
}

#[derive(Args)]
pub struct DiffArgs {
    /// Bundles to compare (default: the profile's selection).
    #[arg(value_name = "BUNDLE")]
    pub bundles: Vec<String>,
    #[command(flatten)]
    pub repo: RepoArgs,
    /// Only the named bundles: no dependencies, no implicit bundles (as `dot apply --no-deps`).
    #[arg(long, requires = "bundles")]
    pub no_deps: bool,
}

#[derive(Args)]
pub struct DoctorArgs {
    #[command(flatten)]
    pub repo: RepoArgs,
}

#[derive(Args)]
pub struct AdoptArgs {
    /// The file or directory to adopt (under the home directory).
    #[arg(value_parser = path_arg())]
    pub path: PathBuf,
    /// The bundle to add it to, created when missing (default: named after the file).
    #[arg(long, value_name = "NAME")]
    pub bundle: Option<String>,
    /// Its file name in the bundle (default: its own, without a leading dot).
    #[arg(long, value_name = "FILE")]
    pub name: Option<String>,
    /// Show what would happen; change nothing.
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub repo: RepoArgs,
}

#[derive(Args)]
pub struct RemoveArgs {
    /// The bundle to remove.
    pub bundle: String,
    /// Also remove the selected bundles that require it (they are removed first).
    #[arg(long)]
    pub cascade: bool,
    /// Also uninstall the packages dot installed for it.
    #[arg(long)]
    pub packages: bool,
    /// Show what would be removed; change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Do not ask for confirmation.
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,
    #[command(flatten)]
    pub repo: RepoArgs,
}

#[derive(Subcommand)]
enum SecretsCommand {
    /// Report secrets that are missing, undeclared or unused, unsafe secrets-file permissions
    /// and committed values for secret keys. Never prints a value; unlocks nothing.
    Check(SecretsCheckArgs),
    /// Resolve secrets here (any provider: envfile, LastPass, ...) and keep them in another
    /// machine's envfile (secrets.env and files/, owner-only), over ssh's stdin. Never through
    /// the repository, a command line or a temporary file.
    Push(transfer::PushArgs),
    /// Write a passphrase-encrypted (age) backup of the envfile secrets.
    Export(transfer::ExportArgs),
    /// Restore a backup made by `dot secrets export`, owner-only.
    Import(transfer::ImportArgs),
    /// Keep the secrets another machine hands over on stdin (run there by `dot secrets push`).
    #[command(hide = true)]
    Receive,
    /// Relay stdin to a run through a private FIFO (run there by `dot apply --host` when ssh's
    /// stdin is the terminal).
    #[command(hide = true)]
    Relay,
}

#[derive(Args)]
pub struct SecretsCheckArgs {
    #[command(flatten)]
    pub repo: RepoArgs,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum SchemaKind {
    /// `bundles/<name>/bundle.yml`
    Bundle,
    /// `dot.yml`
    Repo,
    /// `secrets.yml`
    Secrets,
}

/// The value parser of every path argument: a verbatim Windows path (`\\?\C:\...`) is taken
/// in its ordinary spelling ([`dot_core::paths::plain`]), so what `dot` says about it never
/// shows the verbatim form; any other path exactly as given.
pub(crate) fn path_arg() -> impl clap::builder::TypedValueParser<Value = PathBuf> {
    use clap::builder::TypedValueParser as _;
    clap::builder::PathBufValueParser::new().map(dot_core::paths::plain)
}

/// Where the repository is and which profile and extra variables to use.
#[derive(Args, Clone, Default)]
pub struct RepoArgs {
    /// Dotfiles repository (default: DOT_REPO, the nearest directory above the current one
    /// holding dot.yml, then DOT_REPO in ~/.config/dot/local.env, then ~/.dot).
    #[arg(long, value_name = "DIR", value_parser = path_arg())]
    pub repo: Option<PathBuf>,
    /// Profile to use instead of the pinned or detected one.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,
    /// Set a variable (highest precedence); repeatable.
    #[arg(long = "var", value_name = "KEY=VALUE")]
    pub vars: Vec<String>,
    /// Read variables from a dotenv file (highest precedence); repeatable.
    #[arg(long = "env-file", value_name = "FILE", value_parser = path_arg())]
    pub env_files: Vec<PathBuf>,
}

#[derive(Args)]
pub struct ApplyArgs {
    /// Bundles to apply (default: the profile's selection).
    #[arg(value_name = "BUNDLE")]
    pub bundles: Vec<String>,
    /// Apply these bundle files directly instead of a repository, in order.
    #[arg(short = 'c', long = "config", num_args = 1.., value_name = "FILE",
          conflicts_with_all = ["bundles", "no_deps", "profile", "repo", "host"], value_parser = path_arg())]
    pub config: Vec<PathBuf>,
    #[command(flatten)]
    pub repo: RepoArgs,
    #[command(flatten)]
    pub remote: remote::HostArgs,
    #[command(flatten)]
    pub send: remote::SendArgs,
    /// Read what the machine that started this run hands over (`-`: stdin, else a FIFO).
    #[arg(long, hide = true, value_name = "SOURCE", conflicts_with = "host")]
    pub secrets_from: Option<String>,
    /// Only the named bundles: no dependencies, no implicit bundles.
    #[arg(long)]
    pub no_deps: bool,
    /// Apply bundles even when their `check:` says they are already satisfied.
    #[arg(long)]
    pub force: bool,
    /// Never prompt: no confirmation, and fail instead of asking for anything.
    #[arg(short = 'y', long = "yes", visible_alias = "non-interactive")]
    pub yes: bool,
    /// Report what would change; change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Also upgrade installed packages that are outdated, in every `package` step (by default
    /// only missing ones are installed, unless a step says `upgrade: true`), and run the
    /// `upgrade:` command of `shell` steps whose `unless` succeeds.
    #[arg(long)]
    pub upgrade: bool,
    /// Run up to N bundles at the same time. A bundle still waits for the bundles it requires
    /// or runs after; package managers run one at a time, and a step that needs the terminal
    /// or root runs alone. Plain and log output stay in plan order.
    #[arg(short = 'j', long, value_name = "N", default_value_t = 1,
          value_parser = clap::value_parser!(u16).range(1..=256))]
    pub jobs: u16,
    /// More output: -v shows every step, -vv also streams command output.
    #[arg(short, long, action = clap::ArgAction::Count, conflicts_with = "quiet")]
    pub verbose: u8,
    /// Only failures and the summary.
    #[arg(short, long)]
    pub quiet: bool,
    /// Directory for run logs (default: $XDG_STATE_HOME/dot/logs or ~/.local/state/dot/logs).
    #[arg(long, value_name = "DIR", value_parser = path_arg())]
    pub log_dir: Option<PathBuf>,
    /// One timestamped line per event instead of the live view (the default when stdout is not
    /// a terminal or TERM=dumb).
    #[arg(long, conflicts_with = "json")]
    pub plain: bool,
    /// One JSON object per event on stdout.
    #[arg(long)]
    pub json: bool,
    /// Colour in the live view.
    #[arg(long, value_enum, value_name = "WHEN", default_value_t = render::ColorChoice::Auto)]
    pub color: render::ColorChoice,
}

#[derive(Args)]
pub struct PlanArgs {
    /// Bundles to plan (default: the profile's selection).
    #[arg(value_name = "BUNDLE")]
    pub bundles: Vec<String>,
    /// Plan these bundle files directly instead of a repository, in order.
    #[arg(short = 'c', long = "config", num_args = 1.., value_name = "FILE",
          conflicts_with_all = ["bundles", "no_deps", "profile", "repo", "host"], value_parser = path_arg())]
    pub config: Vec<PathBuf>,
    #[command(flatten)]
    pub repo: RepoArgs,
    #[command(flatten)]
    pub remote: remote::HostArgs,
    /// Read what the machine that started this run hands over (`-`: stdin, else a FIFO).
    #[arg(long, hide = true, value_name = "SOURCE", conflicts_with = "host")]
    pub secrets_from: Option<String>,
    /// Only the named bundles: no dependencies, no implicit bundles.
    #[arg(long)]
    pub no_deps: bool,
    /// Plan bundles even when their `check:` says they are already satisfied.
    #[arg(long)]
    pub force: bool,
    /// Plan as `apply --upgrade` would: outdated packages are upgraded, and `shell` steps with
    /// `upgrade:` run it.
    #[arg(long)]
    pub upgrade: bool,
    /// Print the plan as one JSON object (the `plan_ready` event of `apply --json`).
    #[arg(long)]
    pub json: bool,
    /// More output: -v also lists the steps skipped for this machine and the satisfied actions.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(Args)]
pub struct CheckArgs {
    /// Bundle files or directories to check instead of the whole repository.
    #[arg(value_parser = path_arg())]
    pub paths: Vec<PathBuf>,
    #[command(flatten)]
    pub repo: RepoArgs,
}

#[derive(Args)]
pub struct GraphArgs {
    /// Bundles to start from (default: the profile's selection).
    #[arg(value_name = "BUNDLE")]
    pub bundles: Vec<String>,
    /// Graphviz output.
    #[arg(long)]
    pub dot: bool,
    #[command(flatten)]
    pub repo: RepoArgs,
}

#[derive(Args)]
pub struct WhyArgs {
    pub bundle: String,
    #[command(flatten)]
    pub repo: RepoArgs,
}

#[derive(Args)]
pub struct VarsArgs {
    /// Include this bundle's own `vars:`.
    pub bundle: Option<String>,
    #[command(flatten)]
    pub repo: RepoArgs,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Commands that feed another process (ssh) keep SIGPIPE ignored too: one that exits before
    // reading everything is an error to report, not a reason to die.
    let feeds_ssh = match &cli.command {
        Command::Plan(a) => a.remote.host.is_some(),
        Command::Secrets(SecretsCommand::Push(_)) => true,
        _ => false,
    };
    if !matches!(cli.command, Command::Apply(_)) && !feeds_ssh {
        default_sigpipe();
    }
    let result = match cli.command {
        Command::Apply(a) => commands::apply(a),
        Command::Plan(a) => commands::plan(a),
        Command::Check(a) => commands::check(a),
        Command::Graph(a) => commands::graph(a),
        Command::Why(a) => commands::why(a),
        Command::Vars(a) => commands::vars(a),
        Command::Schema { which } => commands::schema(which),
        Command::Facts => commands::facts(),
        Command::Secrets(SecretsCommand::Check(a)) => commands::secrets_check(a),
        Command::Secrets(SecretsCommand::Push(a)) => transfer::push(a),
        Command::Secrets(SecretsCommand::Export(a)) => transfer::export(a),
        Command::Secrets(SecretsCommand::Import(a)) => transfer::import(a),
        Command::Secrets(SecretsCommand::Receive) => handoff::receive(),
        Command::Secrets(SecretsCommand::Relay) => handoff::relay(),
        Command::HostInfo => remote::host_info(),
        Command::Env(a) => envcmd::env(a),
        Command::Status(a) => ops::status(a),
        Command::Diff(a) => ops::diff(a),
        Command::Doctor(a) => doctor::doctor(a),
        Command::Adopt(a) => adopt::adopt(a),
        Command::Remove(a) => remove::remove(a),
        Command::SelfUpdate(a) => selfupdate::self_update(a),
        Command::CloudInit(a) => cloudinit::cloud_init(a),
    };
    result.unwrap_or_else(|e| {
        eprintln!("error: {e:#}");
        ExitCode::from(EXIT_CONFIG)
    })
}

/// Report commands (`plan`, `vars`, `secrets check`, …) end quietly when their reader goes away
/// (`dot plan | head`) instead of panicking on a broken pipe. `apply` keeps SIGPIPE ignored: its
/// renderers tolerate a closed stdout so the run still finishes and its log file stays complete.
fn default_sigpipe() {
    #[cfg(unix)]
    // SAFETY: called once at startup, before any thread is spawned; restores the default action.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
