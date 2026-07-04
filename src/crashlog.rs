//! Crash logging: a panic hook that appends every panic to a persistent log.
//!
//! "The app randomly crashed" is undebuggable without evidence. This hook makes
//! any panic — on the GUI thread or any worker — leave a timestamped line in
//! `$XDG_STATE_HOME/rewind/crash.log` (default `~/.local/state/rewind/crash.log`)
//! before the default hook prints to stderr, so crash reports always come with
//! a file to look at.

use std::io::Write;
use std::path::PathBuf;

/// `${XDG_STATE_HOME:-~/.local/state}/rewind`.
fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"))
        })
        // Windows / stripped-down environments: fall back next to the profile.
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rewind")
}

/// Install the panic hook. Call once, first thing in `main`.
pub fn install() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let line = format!(
            "[{ts}] rewind v{} panicked on thread '{thread}': {info}\n",
            env!("CARGO_PKG_VERSION")
        );

        let dir = state_dir();
        let path = dir.join("crash.log");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(line.as_bytes());
        }
        eprintln!("rewind: a crash was recorded to {}", path.display());

        default_hook(info);
    }));
}
