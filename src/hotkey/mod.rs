//! Global hotkey handling.
//!
//! On Wayland an application cannot grab global keys directly; the sanctioned
//! path is the xdg-desktop-portal **GlobalShortcuts** interface, with a raw
//! **evdev** reader as a fallback (needs `input` group / udev access). Both live
//! behind the `hotkey` feature.

use std::error::Error;
use std::fmt;

#[cfg(feature = "hotkey")]
pub mod portal;

#[derive(Debug)]
pub enum HotkeyError {
    Unsupported(String),
    Backend(String),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HotkeyError::Unsupported(s) => write!(f, "hotkey unsupported: {s}"),
            HotkeyError::Backend(s) => write!(f, "hotkey backend error: {s}"),
        }
    }
}

impl Error for HotkeyError {}

/// Registers a global "save clip" shortcut and invokes a callback when pressed.
pub trait HotkeyManager: Send {
    fn name(&self) -> &str;

    /// Register the save shortcut. `accelerator` is a human string such as
    /// `"Ctrl+Alt+S"`. `on_trigger` is called (on some internal thread) each
    /// time the shortcut fires.
    fn register_save(
        &mut self,
        accelerator: &str,
        on_trigger: Box<dyn FnMut() + Send>,
    ) -> Result<(), HotkeyError>;

    /// Tear down the shortcut registration.
    fn stop(&mut self);
}

/// Create the hotkey backend, if one is built in.
pub fn create_manager() -> Result<Box<dyn HotkeyManager>, HotkeyError> {
    #[cfg(feature = "hotkey")]
    {
        portal::manager()
    }
    #[cfg(not(feature = "hotkey"))]
    {
        Err(HotkeyError::Unsupported(
            "no hotkey backend built (enable feature `hotkey`)".into(),
        ))
    }
}
