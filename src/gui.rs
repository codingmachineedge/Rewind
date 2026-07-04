//! Native Linux GUI — GTK4 + libadwaita (via gtk4-rs).
//!
//! Modern libadwaita layout: a flat-header `ToolbarView`, a hero status card
//! with a pulsing REC indicator and pill action buttons, Adwaita preference
//! rows (`SpinRow`/`EntryRow`/`SwitchRow`) for settings, and toast
//! notifications for saved/converted clips. All wired to the live [`Pipeline`];
//! events are marshalled back onto the GTK main thread.

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

use gtk::{Align, Button, Label, Orientation, ToggleButton};

use crate::config::Config;
use crate::media::CaptureTarget;
use crate::pipeline::{EventSink, Pipeline, PipelineEvent};

const APP_ID: &str = "org.rewind.Rewind";

/// The capture-target options offered in the GUI, in dropdown order. The index
/// maps to [`CaptureTarget`] via [`target_from_index`]/[`index_from_target`].
const CAPTURE_TARGETS: &[(&str, CaptureTarget)] = &[
    ("Whole screen", CaptureTarget::Monitor),
    ("Specific window", CaptureTarget::Window),
    ("Active window", CaptureTarget::ActiveWindow),
];

fn target_from_index(i: u32) -> CaptureTarget {
    CAPTURE_TARGETS
        .get(i as usize)
        .map(|(_, t)| *t)
        .unwrap_or(CaptureTarget::Monitor)
}

fn index_from_target(target: CaptureTarget) -> u32 {
    CAPTURE_TARGETS
        .iter()
        .position(|(_, t)| *t == target)
        .unwrap_or(0) as u32
}

const APP_CSS: &str = "
.status-card {
    padding: 28px 24px 24px 24px;
    border-radius: 18px;
}
.rec-dot {
    min-width: 13px;
    min-height: 13px;
    border-radius: 999px;
    background-color: alpha(#e01b24, 0.35);
}
.rec-dot.live {
    background-color: #e01b24;
    animation: rewind-pulse 1.5s ease-in-out infinite;
}
@keyframes rewind-pulse {
    0%   { opacity: 1; }
    50%  { opacity: 0.35; }
    100% { opacity: 1; }
}
.status-title {
    font-size: 18px;
    font-weight: 800;
}
.status-sub {
    font-size: 13px;
}
.action-row-box > button {
    min-width: 150px;
}
";

/// Launch the application. Blocks until the window is closed.
pub fn run() {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(APP_CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });
    app.connect_activate(build_ui);
    let _ = app.run();
}

fn build_ui(app: &adw::Application) {
    // Channel to marshal pipeline events (from worker threads) to the UI thread.
    let (tx, rx): (Sender<PipelineEvent>, Receiver<PipelineEvent>) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    let events: EventSink = Arc::new(move |event: PipelineEvent| {
        if let Ok(tx) = tx.lock() {
            let _ = tx.send(event);
        }
    });

    let pipeline = Rc::new(RefCell::new(Pipeline::new(Config::default(), events)));
    let core = pipeline.borrow().core();
    let defaults = Config::default();

    // --- Hero status card -------------------------------------------------

    let rec_dot = gtk::Box::new(Orientation::Horizontal, 0);
    rec_dot.add_css_class("rec-dot");
    rec_dot.set_valign(Align::Center);

    let status_title = Label::new(Some("Idle"));
    status_title.add_css_class("status-title");

    let title_row = gtk::Box::new(Orientation::Horizontal, 10);
    title_row.set_halign(Align::Center);
    title_row.append(&rec_dot);
    title_row.append(&status_title);

    let status_sub = Label::new(Some("Not capturing — your screen stays private."));
    status_sub.add_css_class("dim-label");
    status_sub.add_css_class("status-sub");
    status_sub.set_wrap(true);
    status_sub.set_justify(gtk::Justification::Center);
    status_sub.set_halign(Align::Center);

    let toggle = ToggleButton::with_label("Start Capture");
    toggle.add_css_class("pill");
    toggle.add_css_class("suggested-action");

    let save_btn = Button::with_label("Save Clip");
    save_btn.add_css_class("pill");
    save_btn.set_sensitive(false);
    save_btn.set_tooltip_text(Some("Flush the last N seconds to a clip"));

    let button_row = gtk::Box::new(Orientation::Horizontal, 12);
    button_row.add_css_class("action-row-box");
    button_row.set_halign(Align::Center);
    button_row.append(&toggle);
    button_row.append(&save_btn);

    let card = gtk::Box::new(Orientation::Vertical, 10);
    card.add_css_class("card");
    card.add_css_class("status-card");
    card.append(&title_row);
    card.append(&status_sub);
    card.append(&{
        let spacer = gtk::Box::new(Orientation::Vertical, 0);
        spacer.set_margin_top(8);
        spacer
    });
    card.append(&button_row);

    // --- Settings rows -----------------------------------------------------

    let group = adw::PreferencesGroup::builder().title("Settings").build();

    let buffer_row = adw::SpinRow::with_range(5.0, 600.0, 5.0);
    buffer_row.set_title("Buffer length");
    buffer_row.set_subtitle("Seconds of recent gameplay kept in memory");
    buffer_row.set_value(defaults.buffer_seconds as f64);
    group.add(&buffer_row);

    let folder_row = adw::EntryRow::builder().title("Output folder").build();
    folder_row.set_text(&defaults.output_dir.display().to_string());
    group.add(&folder_row);

    let hotkey_row = adw::EntryRow::builder().title("Save hotkey").build();
    hotkey_row.set_text(&defaults.save_hotkey);
    group.add(&hotkey_row);

    let target_model = gtk::StringList::new(
        &CAPTURE_TARGETS.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
    );
    let target_row = adw::ComboRow::builder()
        .title("Capture target")
        .subtitle("Whole screen, a specific window (re-attached across launches), or the active window")
        .model(&target_model)
        .build();
    target_row.set_selected(index_from_target(defaults.capture_target));
    group.add(&target_row);

    let audio_row = adw::SwitchRow::builder()
        .title("Capture audio")
        .subtitle("Mux system audio into the clip")
        .active(defaults.capture_audio)
        .build();
    group.add(&audio_row);

    let convert_row = adw::SwitchRow::builder()
        .title("Auto-convert after save")
        .subtitle("Transcode to a shareable H.264/AAC MP4")
        .active(defaults.auto_convert)
        .build();
    group.add(&convert_row);

    let clipboard_row = adw::SwitchRow::builder()
        .title("Copy clip to clipboard")
        .subtitle("Paste the saved clip straight into chat")
        .active(defaults.copy_to_clipboard)
        .build();
    group.add(&clipboard_row);

    // --- Layout: ToastOverlay > ToolbarView > Clamp -------------------------

    let content = gtk::Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(24)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(20)
        .margin_end(20)
        .build();
    content.append(&card);
    content.append(&group);

    let clamp = adw::Clamp::builder()
        .maximum_size(480)
        .tightening_threshold(400)
        .child(&content)
        .build();

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();

    let header = adw::HeaderBar::new();
    header.add_css_class("flat");
    header.set_title_widget(Some(&adw::WindowTitle::new("Rewind", "")));

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&scroller));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar_view));

    // --- Event pump: pipeline events -> status card + toasts ----------------
    {
        let status_title = status_title.clone();
        let status_sub = status_sub.clone();
        let toasts = toasts.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(120), move || {
            while let Ok(event) = rx.try_recv() {
                match event {
                    PipelineEvent::Status(s) => status_sub.set_text(&s),
                    PipelineEvent::ClipSaved(p) => {
                        status_title.set_text("Clip saved");
                        toasts.add_toast(
                            adw::Toast::builder()
                                .title(format!("Saved → {}", p.display()))
                                .timeout(4)
                                .build(),
                        );
                    }
                    PipelineEvent::ClipConverted(p) => {
                        toasts.add_toast(
                            adw::Toast::builder()
                                .title(format!("Shareable copy ready → {}", p.display()))
                                .timeout(4)
                                .build(),
                        );
                    }
                    PipelineEvent::Error(e) => {
                        toasts.add_toast(
                            adw::Toast::builder().title(format!("⚠ {e}")).timeout(6).build(),
                        );
                    }
                }
            }
            gtk::glib::ControlFlow::Continue
        });
    }

    // --- Wiring --------------------------------------------------------------

    // Push the current settings into the pipeline's config before (re)starting.
    let apply_settings = {
        let core = core.clone();
        let buffer_row = buffer_row.clone();
        let folder_row = folder_row.clone();
        let hotkey_row = hotkey_row.clone();
        let target_row = target_row.clone();
        let audio_row = audio_row.clone();
        let convert_row = convert_row.clone();
        let clipboard_row = clipboard_row.clone();
        move || {
            let mut cfg = core.config.lock().unwrap();
            cfg.buffer_seconds = buffer_row.value() as u32;
            cfg.output_dir = PathBuf::from(folder_row.text().as_str());
            cfg.save_hotkey = hotkey_row.text().to_string();
            cfg.capture_target = target_from_index(target_row.selected());
            cfg.capture_audio = audio_row.is_active();
            cfg.auto_convert = convert_row.is_active();
            cfg.copy_to_clipboard = clipboard_row.is_active();
        }
    };

    // Start/stop capture.
    {
        let pipeline = pipeline.clone();
        let rec_dot = rec_dot.clone();
        let status_title = status_title.clone();
        let status_sub = status_sub.clone();
        let save_btn = save_btn.clone();
        let toasts = toasts.clone();
        let apply_settings = apply_settings.clone();
        toggle.connect_toggled(move |btn| {
            if btn.is_active() {
                apply_settings();
                // IMPORTANT: end the RefCell borrow before touching `btn`.
                // `set_active(false)` re-enters this handler synchronously; if the
                // `RefMut` from a `match pipeline.borrow_mut().start()` scrutinee
                // were still alive, the re-entrant `borrow_mut()` would panic
                // (BorrowMutError) and crash the app whenever start() fails.
                let result = pipeline.borrow_mut().start();
                match result {
                    Ok(()) => {
                        btn.set_label("Stop Capture");
                        btn.remove_css_class("suggested-action");
                        btn.add_css_class("destructive-action");
                        rec_dot.add_css_class("live");
                        status_title.set_text("Recording");
                        save_btn.set_sensitive(true);
                    }
                    Err(e) => {
                        toasts.add_toast(
                            adw::Toast::builder()
                                .title(format!("⚠ Could not start capture: {e}"))
                                .timeout(6)
                                .build(),
                        );
                        btn.set_active(false);
                    }
                }
            } else {
                pipeline.borrow_mut().stop();
                btn.set_label("Start Capture");
                btn.remove_css_class("destructive-action");
                btn.add_css_class("suggested-action");
                rec_dot.remove_css_class("live");
                status_title.set_text("Idle");
                status_sub.set_text("Not capturing — your screen stays private.");
                save_btn.set_sensitive(false);
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
        .default_width(440)
        .default_height(560)
        .content(&toasts)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_index_round_trips() {
        for (i, (_, target)) in CAPTURE_TARGETS.iter().enumerate() {
            assert_eq!(index_from_target(*target), i as u32);
            assert_eq!(target_from_index(i as u32), *target);
        }
    }

    #[test]
    fn out_of_range_index_falls_back_to_monitor() {
        assert_eq!(target_from_index(99), CaptureTarget::Monitor);
    }

    #[test]
    fn default_target_is_selectable() {
        // The default config target must have a dropdown row.
        let idx = index_from_target(Config::default().capture_target);
        assert!((idx as usize) < CAPTURE_TARGETS.len());
    }
}
