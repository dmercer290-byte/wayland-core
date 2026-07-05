//! The single cross-platform process-spawning chokepoint.
//!
//! Two modes, mirroring the Genesis security model:
//!
//! - **Argv mode** ([`command_argv`]) — the OS resolves the program against
//!   `PATH` and each argument is a separate argv entry; no shell interpreter
//!   is involved, so metacharacters in arguments are never interpreted. This
//!   is the only safe mode for LLM-supplied arguments.
//! - **Shell-string mode** ([`shell_command`]) — runs `sh -c <str>` on Unix
//!   and `cmd /C <str>` on Windows. Metacharacters ARE interpreted. Reserved
//!   for surfaces whose contract is "run a shell command" (the bash tool).
//!   Never `format!`-interpolate LLM-supplied data into the string here —
//!   pass the model's command through verbatim as the whole string instead.

use tokio::process::Command;

/// Build a command that runs `program` with `args` directly (no shell).
pub fn command_argv(program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd
}

/// Build a command that runs `script` through the platform shell.
pub fn shell_command(script: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(script);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }
}
