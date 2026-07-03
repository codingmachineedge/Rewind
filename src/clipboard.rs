//! Optional "copy the clip to the clipboard" support.
//!
//! Puts a file reference (`text/uri-list` with a `file://` URI) on the desktop
//! clipboard so the saved clip can be pasted straight into a chat app or file
//! manager. Best-effort and dependency-free: it shells out to the standard
//! clipboard tools (`wl-copy` on Wayland, `xclip`/`xsel` on X11), so nothing is
//! compiled in and the core still builds on any host.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Copy `path` to the clipboard as a pasteable file reference. Returns a short
/// human-readable message on success, or an error string.
pub fn copy_file(path: &Path) -> Result<String, String> {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let uri = format!("file://{}", abs.display());
    let payload = format!("{uri}\n");

    // Prefer the tool matching the session, then fall back to the others.
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let uri_list: &[(&str, &[&str])] = &[
        ("wl-copy", &["--type", "text/uri-list"]),
        ("xclip", &["-selection", "clipboard", "-t", "text/uri-list", "-i"]),
    ];
    let x11_first: &[(&str, &[&str])] = &[
        ("xclip", &["-selection", "clipboard", "-t", "text/uri-list", "-i"]),
        ("xsel", &["--clipboard", "--input"]),
        ("wl-copy", &["--type", "text/uri-list"]),
    ];
    let candidates = if wayland { uri_list } else { x11_first };

    let mut last_err =
        String::from("no clipboard tool found (install `wl-clipboard` or `xclip`)");
    for (tool, args) in candidates {
        match feed(tool, args, payload.as_bytes()) {
            Ok(()) => return Ok(format!("clip copied to clipboard via {tool}")),
            Err(e) => last_err = format!("{tool}: {e}"),
        }
    }
    Err(last_err)
}

/// Spawn `tool args`, write `input` to its stdin, and wait. The clipboard tools
/// fork a background server and the foreground process exits promptly, so this
/// does not block on the clipboard being consumed.
fn feed(tool: &str, args: &[&str], input: &[u8]) -> Result<(), String> {
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or_else(|| "no stdin".to_string())?
        .write_all(input)
        .map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("exited with {:?}", status.code()))
    }
}
