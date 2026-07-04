//! Start-at-login support via the XDG autostart spec.
//!
//! The ShadowPlay workflow people actually want is "always on from login, press
//! a hotkey when something cool happens." On Linux the usual advice is to add
//! OBS to your session's startup apps by hand; Rewind does it for you:
//!
//! ```sh
//! rewind --install-autostart      # write ~/.config/autostart/rewind.desktop
//! rewind --uninstall-autostart    # remove it
//! ```
//!
//! Works on any XDG-compliant desktop (GNOME, KDE, wlroots + dex, ...).

use std::path::PathBuf;

fn autostart_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("autostart")
}

fn desktop_file() -> PathBuf {
    autostart_dir().join("rewind.desktop")
}

/// Write the XDG autostart entry pointing at the current binary.
pub fn install() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = autostart_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Rewind\n\
         Comment=Privacy-first clip recorder — rolling replay buffer\n\
         Exec={}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         StartupNotify=false\n",
        exe.display()
    );

    let path = desktop_file();
    std::fs::write(&path, entry).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Remove the autostart entry (idempotent).
pub fn uninstall() -> Result<PathBuf, String> {
    let path = desktop_file();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

/// Handle `--install-autostart` / `--uninstall-autostart`. Returns true if an
/// autostart flag was consumed (caller should exit afterwards).
pub fn handle_cli_args() -> bool {
    let mut handled = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--install-autostart" => {
                match install() {
                    Ok(p) => println!("autostart installed: {}", p.display()),
                    Err(e) => eprintln!("autostart install failed: {e}"),
                }
                handled = true;
            }
            "--uninstall-autostart" => {
                match uninstall() {
                    Ok(p) => println!("autostart removed: {}", p.display()),
                    Err(e) => eprintln!("autostart uninstall failed: {e}"),
                }
                handled = true;
            }
            _ => {}
        }
    }
    handled
}
