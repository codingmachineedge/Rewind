//! Native Linux GUI — GTK4 + libadwaita (via gtk4-rs).
//!
//! A minimal but real control surface wired to the existing [`Config`] and
//! [`ClipBuffer`] stubs. The capture pipeline isn't implemented yet, so toggling
//! capture and saving update in-memory state and the status text rather than
//! moving real frames — the widgets and wiring are real and ready for the
//! pipeline to be dropped in behind them.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;

use adw::prelude::*;
use gtk::prelude::*;

use gtk::{Align, Button, Entry, Label, Orientation, SpinButton, ToggleButton};

use crate::buffer::ClipBuffer;
use crate::config::Config;

const APP_ID: &str = "org.rewind.Rewind";

/// Shared runtime state the widgets read and mutate.
struct AppState {
    config: Config,
    buffer: ClipBuffer,
    capturing: bool,
}

/// Launch the application. Blocks until the window is closed.
pub fn run() {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    let _ = app.run();
}

fn build_ui(app: &adw::Application) {
    let config = Config::default();
    let buffer = ClipBuffer::new(config.buffer_seconds, config.target_fps);
    let state = Rc::new(RefCell::new(AppState {
        config: config.clone(),
        buffer,
        capturing: false,
    }));

    // Root: header bar on top, content below.
    let root = gtk::Box::new(Orientation::Vertical, 0);

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(
        "Rewind",
        "Privacy-first clip recorder",
    )));
    root.append(&header);

    let content = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();

    // Status line.
    let status = Label::new(Some("Idle — not capturing."));
    status.set_halign(Align::Start);
    status.add_css_class("title-4");

    // Capture toggle + save button.
    let controls = gtk::Box::new(Orientation::Horizontal, 12);
    let toggle = ToggleButton::with_label("Start Capture");
    toggle.add_css_class("suggested-action");
    let save_btn = Button::with_label("Save last N seconds");
    save_btn.set_sensitive(false);
    controls.append(&toggle);
    controls.append(&save_btn);

    // Settings, as libadwaita preference rows.
    let group = adw::PreferencesGroup::builder().title("Settings").build();

    let buffer_spin = SpinButton::with_range(5.0, 600.0, 5.0);
    buffer_spin.set_value(config.buffer_seconds as f64);
    buffer_spin.set_valign(Align::Center);
    let buffer_row = adw::ActionRow::builder()
        .title("Buffer length (seconds)")
        .subtitle("How much recent gameplay to keep in memory")
        .build();
    buffer_row.add_suffix(&buffer_spin);
    group.add(&buffer_row);

    let folder_entry = Entry::new();
    folder_entry.set_text(&config.output_dir.display().to_string());
    folder_entry.set_hexpand(true);
    folder_entry.set_valign(Align::Center);
    let folder_row = adw::ActionRow::builder().title("Output folder").build();
    folder_row.add_suffix(&folder_entry);
    group.add(&folder_row);

    let hotkey_entry = Entry::new();
    hotkey_entry.set_text(&config.save_hotkey);
    hotkey_entry.set_valign(Align::Center);
    let hotkey_row = adw::ActionRow::builder().title("Save hotkey").build();
    hotkey_row.add_suffix(&hotkey_entry);
    group.add(&hotkey_row);

    content.append(&status);
    content.append(&controls);
    content.append(&group);
    root.append(&content);

    // --- Wiring ---------------------------------------------------------------

    // Start/stop capture. Resizes the buffer to the current setting on start.
    {
        let state = state.clone();
        let status = status.clone();
        let save_btn = save_btn.clone();
        let buffer_spin = buffer_spin.clone();
        toggle.connect_toggled(move |btn| {
            let mut st = state.borrow_mut();
            st.capturing = btn.is_active();
            if st.capturing {
                let seconds = buffer_spin.value() as u32;
                st.config.buffer_seconds = seconds;
                st.buffer = ClipBuffer::new(seconds, st.config.target_fps);
                btn.set_label("Stop Capture");
                status.set_text(&format!(
                    "Capturing… buffering last {seconds}s ({} frames).",
                    st.buffer.capacity()
                ));
                save_btn.set_sensitive(true);
            } else {
                btn.set_label("Start Capture");
                status.set_text("Idle — not capturing.");
                save_btn.set_sensitive(false);
            }
        });
    }

    // Save last N seconds: flush the buffer to a clip in the chosen folder.
    {
        let state = state.clone();
        let status = status.clone();
        let folder_entry = folder_entry.clone();
        save_btn.connect_clicked(move |_| {
            let mut st = state.borrow_mut();
            // Stub: push a placeholder frame so there is something to flush.
            st.buffer.push_frame_placeholder();
            let out = PathBuf::from(folder_entry.text().as_str());
            match st.buffer.flush_to_clip(&out) {
                Ok(path) => status.set_text(&format!("Saved clip → {path}")),
                Err(e) => status.set_text(&format!("Save failed: {e}")),
            }
        });
    }

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Rewind")
        .default_width(440)
        .default_height(380)
        .content(&root)
        .build();

    window.present();
}
