//! `hoard desktop` / `hoard server`: the CLI is the single entry point to the
//! family. These subcommands forward to the already-installed sibling binaries
//! (`hoard-desktop`, `hoard-server`) instead of compiling Tauri/Axum into the
//! CLI, which would bloat it. Extra arguments pass through as-is.

use anyhow::{bail, Result};

/// Replaces (unix) or launches and waits for (elsewhere) the `binary`,
/// forwarding `args` and inheriting stdio. On unix it uses `exec` so signals and
/// exit code propagate cleanly.
pub fn run(binary: &str, args: &[String]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec only returns if the launch fails.
        let err = std::process::Command::new(binary).args(args).exec();
        if err.kind() == std::io::ErrorKind::NotFound {
            bail!("`{binary}` is not on PATH. Install the component or add it to PATH.");
        }
        Err(err.into())
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(binary).args(args).status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("`{binary}` is not on PATH. Install the component or add it to PATH.")
            }
            Err(e) => Err(e.into()),
        }
    }
}
