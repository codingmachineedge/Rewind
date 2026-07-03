//! Native Linux GUI — GTK4 + libadwaita (via gtk4-rs).
//!
//! A real control surface wired to the live [`Pipeline`]: the toggle starts and
//! stops capture, the button flushes the rolling buffer to a clip, the settings
//! rows feed [`Config`], and pipeline events are marshalled back onto the GTK
//! main thread to update the status line.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gtk4 as gtk;
use libadwaita as adw;

// `adw::prelude` re-exports the gtk4 prelude, so we don't import it separately.
use adw::prelude::*;

use gtk::{Align, Button, Entry, Label, Orientation, SpinButton, ToggleButton};

use crate::config::Config;
use crate::pipeline::{EventSink, Pipeline, PipelineEvent};

const APP_ID: &str = "org.rewind.Rewind";

/// Launch the application. Blocks until the window is closed.
pub fn run() {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    let _ = app.run();
}

fn build_ui(app: &adw::Application) {
    // Channel to marshal pipeline events (from worker threads) to the UI thread.
    let (tx, rx): (Sender<PipelineEvent>, Receiver<PipelineEvent>) = std::sync::mpsc::channel();
    // The event callback must be Send + Sync; wrap the Sender in a Mutex.
    let tx = Mutex::new(tx);
    let events: EventSink = Arc::new(move |event: PipelineEvent| {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(event);
        }
    });

    let pipeline = Rc::new(RefCell::new(Pipeline::new(Config::default(), events)));
    let core = pipeline.borrow().core();

    // --- Widgets --------------------------------------------------------------

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

    let status = Label::new(Some("Idle — not capturing."));
    status.set_halign(Align::Start);
    status.set_wrap(true);
    status.add_css_class("title-4");

    let controls = gtk::Box::new(Orientation::Horizontal, 12);
    let toggle = ToggleButton::with_label("Start Capture");
    toggle.add_css_class("suggested-action");
    let save_btn = Button::with_label("Save last N seconds");
    save_btn.set_sensitive(false);
    controls.append(&toggle);
    controls.append(&save_btn);

    let group = adw::PreferencesGroup::builder().title("Settings").build();

    let buffer_spin = SpinButton::with_range(5.0, 600.0, 5.0);
    buffer_spin.set_value(Config::default().buffer_seconds as f64);
    buffer_spin.set_valign(Align::Center);
    let buffer_row = adw::ActionRow::builder()
        .title("Buffer length (seconds)")
        .subtitle("How much recent gameplay to keep in memory")
        .build();
    buffer_row.add_suffix(&buffer_spin);
    group.add(&buffer_row);

    let folder_entry = Entry::new();
    folder_entry.set_text(&Config::default().output_dir.display().to_string());
    folder_entry.set_hexpand(true);
    folder_entry.set_valign(Align::Center);
    let folder_row = adw::ActionRow::builder().title("Output folder").build();
    folder_row.add_suffix(&folder_entry);
    group.add(&folder_row);

    let hotkey_entry = Entry::new();
    hotkey_entry.set_text(&Config::default().save_hotkey);
    hotkey_entry.set_valign(Align::Center);
    let hotkey_row = adw::ActionRow::builder().title("Save hotkey").build();
    hotkey_row.add_suffix(&hotkey_entry);
    group.add(&hotkey_row);

    let audio_row = adw::SwitchRow::builder()
        .title("Capture audio")
        .subtitle("Mux system audio into the clip")
        .active(true)
        .build();
    group.add(&audio_row);

    let convert_row = adw::SwitchRow::builder()
        .title("Auto-convert after save")
        .subtitle("Transcode to a shareable H.264/AAC MP4")
        .active(true)
        .build();
    group.add(&convert_row);

    let clipboard_row = adw::SwitchRow::builder()
        .title("Copy clip to clipboard")
        .subtitle("Paste the saved clip straight into chat")
        .active(false)
        .build();
    group.add(&clipboard_row);

    content.append(&status);
    content.append(&controls);
    content.append(&group);
    root.append(&content);

    // --- Event pump: drain pipeline events on the UI thread -------------------
    {
        let status = status.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(120), move || {
            while let Ok(event) = rx.try_recv() {
                match event {
                    PipelineEvent::Status(s) => status.set_text(&s),
                    PipelineEvent::ClipSaved(p) => {
                        status.set_text(&format!("Saved clip → {}", p.display()))
                    }
                    PipelineEvent::ClipConverted(p) => {
                        status.set_text(&format!("Shareable clip ready → {}", p.display()))
                    }
                    PipelineEvent::Error(e) => status.set_text(&format!("⚠ {e}")),
                }
            }
            gtk::glib::ControlFlow::Continue
        });
    }

    // --- Wiring ---------------------------------------------------------------

    // Push the current settings into the pipeline's config before (re)starting.
    let apply_settings = {
        let core = core.clone();
        let buffer_spin = buffer_spin.clone();
        let folder_entry = folder_entry.clone();
        let hotkey_entry = hotkey_entry.clone();
        let audio_row = audio_row.clone();
        let convert_row = convert_row.clone();
        let clipboard_row = clipboard_row.clone();
        move || {
            let mut cfg = core.config.lock().unwrap();
            cfg.buffer_seconds = buffer_spin.value() as u32;
            cfg.output_dir = PathBuf::from(folder_entry.text().as_str());
            cfg.save_hotkey = hotkey_entry.text().to_string();
            cfg.capture_audio = audio_row.is_active();
            cfg.auto_convert = convert_row.is_active();
            cfg.copy_to_clipboard = clipboard_row.is_active();
        }
    };

    // Start/stop capture.
    {
        let pipeline = pipeline.clone();
        let status = status.clone();
        let save_btn = save_btn.clone();
        let apply_settings = apply_settings.clone();
        toggle.connect_toggled(move |btn| {
            if btn.is_active() {
                apply_settings();
                match pipeline.borrow_mut().start() {
                    Ok(()) => {
                        btn.set_label("Stop Capture");
                        save_btn.set_sensitive(true);
                    }
                    Err(e) => {
                        status.set_text(&format!("⚠ could not start capture: {e}"));
                        btn.set_active(false);
                    }
                }
            } else {
                pipeline.borrow_mut().stop();
                btn.set_label("Start Capture");
                save_btn.set_sensitive(false);
                status.set_text("Idle — not capturing.");
            }
        });
    }

    // Save last N seconds — flush the rolling buffer to a clip.
    {
        let core = core.clone();
        save_btn.connect_clicked(move |_| {
            core.save_last_n();
        });
    }

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Rewind")
        .default_width(460)
        .default_height(400)
        .content(&root)
        .build();

    // Ensure the pipeline is stopped when the window closes.
    {
        let pipeline = pipeline.clone();
        window.connect_close_request(move |_| {
            pipeline.borrow_mut().stop();
            gtk::glib::Propagation::Proceed
        });
    }

    window.present();
}
