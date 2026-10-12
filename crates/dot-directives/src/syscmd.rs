//! Running system-tool commands from an action: output forwarded as `Output` events, failures
//! with the tool's own message. Privileged commands go through `sudo -n`, which never prompts:
//! actions that use them say so ([`dot_core::Action::needs_sudo`]) and the run asks once, up
//! front.

use anyhow::{Context as _, Result};
use dot_core::{RunCtx, Stream};
use dot_platform::exec::{Cmd, Output, System};

/// Runs `cmd` and fails unless it exits 0.
pub fn run(sys: &System, cmd: &Cmd, ctx: &RunCtx) -> Result<Output> {
    let out = sys
        .run(cmd)
        .with_context(|| format!("running `{}`", cmd.line()))?;
    for line in out.stdout.lines() {
        ctx.output(Stream::Stdout, line);
    }
    for line in out.stderr.lines() {
        ctx.output(Stream::Stderr, line);
    }
    if !out.success() {
        return Err(out.failure(cmd));
    }
    Ok(out)
}

/// Runs each command in turn, stopping at the first failure.
pub fn run_all(sys: &System, cmds: &[Cmd], ctx: &RunCtx) -> Result<()> {
    for c in cmds {
        run(sys, c, ctx)?;
    }
    Ok(())
}
