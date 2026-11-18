// Put this file at src/main.rs next to Cargo.toml.
// VM Typer: types text into a VM console, one keystroke at a time.
// Abort any time: click Stop, or slam the mouse into a screen corner (fail-safe).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use enigo::{Direction, Enigo, Key, Keyboard, Mouse, Settings};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(PartialEq, Eq)]
enum InputMode {
    Text,
    File,
}

// How much of a dropped file to show as a preview, so a huge file
// doesn't get rendered into the UI in full.
const PREVIEW_CHARS: usize = 600;

// Window sizes for the normal and compact layouts.
const NORMAL_SIZE: egui::Vec2 = egui::Vec2::new(520.0, 620.0);
const COMPACT_SIZE: egui::Vec2 = egui::Vec2::new(230.0, 95.0);

struct Shared {
    status: String,
    progress: f32,
    running: bool,
}

struct VmTyper {
    text: String,
    input_mode: InputMode,
    dropped_file: Option<PathBuf>,
    preview: String,

    // --- Settings -------------------------------------------------------
    // Add new settings fields here as you add more toggles below.
    delay_s: u64,
    key_ms: u64,
    clean: bool,
    strip_indent: bool,
    final_enter: bool,
    always_on_top: bool,

    // Whether the window is shrunk to just Start/Stop/Return + progress,
    // so the full editor doesn't sit on top of the VM window.
    compact_mode: bool,

    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
}

fn clean_smart(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '\u{2018}' | '\u{2019}' => "'".chars().collect::<Vec<_>>(),
            '\u{201c}' | '\u{201d}' => "\"".chars().collect(),
            '\u{2013}' | '\u{2014}' => "-".chars().collect(),
            '\u{2026}' => "...".chars().collect(),
            '\u{00a0}' => " ".chars().collect(),
            other => vec![other],
        })
        .collect()
}

fn set(shared: &Arc<Mutex<Shared>>, f: impl FnOnce(&mut Shared)) {
    f(&mut shared.lock().unwrap());
}

// Mouse in any screen corner = abort (like pyautogui.FAILSAFE).
fn in_corner(enigo: &Enigo) -> bool {
    match (enigo.main_display(), enigo.location()) {
        (Ok((w, h)), Ok((x, y))) => (x <= 0 || x >= w - 1) && (y <= 0 || y >= h - 1),
        _ => false,
    }
}

fn make_preview(text: &str) -> String {
    if text.chars().count() <= PREVIEW_CHARS {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(PREVIEW_CHARS).collect();
        format!("{truncated}\n\n… (preview truncated, full text will still be typed)")
    }
}

fn run(
    text: String,
    delay: u64,
    interval: Duration,
    final_enter: bool,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    ctx: egui::Context,
) {
    let finish = |msg: String| {
        set(&shared, |s| {
            s.status = msg;
            s.running = false;
        });
        ctx.request_repaint();
    };

    // Enigo is created on this thread (it isn't Send on every platform).
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => return finish(format!("Could not init input simulation: {e}")),
    };

    for i in (1..=delay).rev() {
        set(&shared, |s| {
            s.status = format!("Click inside the VM console now. Typing starts in {i}s...")
        });
        ctx.request_repaint();
        for _ in 0..10 {
            if stop.load(Ordering::SeqCst) {
                return finish("Stopped before typing started.".into());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let mut skipped = std::collections::BTreeSet::new();

    for (i, ch) in chars.iter().enumerate() {
        let idx = i + 1;
        if stop.load(Ordering::SeqCst) {
            return finish(format!("Stopped at {i} of {total} characters."));
        }
        if in_corner(&enigo) {
            return finish("Aborted by fail-safe (mouse hit a screen corner).".into());
        }

        let res = match ch {
            '\n' => enigo.key(Key::Return, Direction::Click),
            '\t' => enigo.key(Key::Tab, Direction::Click),
            c if !c.is_ascii() => {
                skipped.insert(*c);
                Ok(())
            }
            c => enigo.text(&c.to_string()),
        };
        if let Err(e) = res {
            return finish(format!("Input error at character {idx}: {e}"));
        }

        if !interval.is_zero() {
            thread::sleep(interval);
        }
        if idx % 10 == 0 || idx == total {
            set(&shared, |s| {
                s.progress = idx as f32 / total as f32;
                s.status = format!("Typing... {idx} / {total}");
            });
            ctx.request_repaint();
        }
    }

    if final_enter {
        let _ = enigo.key(Key::Return, Direction::Click);
    }
    let mut msg = String::from("Done.");
    if !skipped.is_empty() {
        let list: String = skipped.into_iter().collect();
        msg.push_str(&format!(" Skipped non-ASCII characters: {list}"));
    }
    finish(msg);
}

impl VmTyper {
    fn new() -> Self {
        Self {
            text: String::new(),
            input_mode: InputMode::Text,
            dropped_file: None,
            preview: String::new(),
            delay_s: 5,
            key_ms: 40,
            clean: true,
            strip_indent: false,
            final_enter: false,
            always_on_top: true,
            compact_mode: false,
            shared: Arc::new(Mutex::new(Shared {
                status: "Paste text, or switch to File mode and drop a file in.".into(),
                progress: 0.0,
                running: false,
            })),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    fn paste_clipboard(&mut self) {
        if let Ok(t) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            self.text = t;
        }
    }

    // Reads a dropped file's contents into self.text and builds the preview.
    fn load_file(&mut self, path: PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                self.preview = make_preview(&content);
                self.text = content;
                self.dropped_file = Some(path);
                set(&self.shared, |s| s.status = format!("Loaded file: {name}"));
            }
            Err(e) => {
                set(&self.shared, |s| s.status = format!("Could not read file: {e}"));
            }
        }
    }

    fn clear_file(&mut self) {
        self.text.clear();
        self.preview.clear();
        self.dropped_file = None;
        set(&self.shared, |s| s.progress = 0.0);
    }

    fn prepare(&self) -> String {
        let mut t = self.text.replace("\r\n", "\n").replace('\r', "\n");
        if self.clean {
            t = clean_smart(&t);
        }
        if self.strip_indent {
            t = t
                .split('\n')
                .map(|l| l.trim_start_matches([' ', '\t']))
                .collect::<Vec<_>>()
                .join("\n");
        }
        t
    }

    fn start(&mut self, ctx: &egui::Context) {
        let text = self.prepare();
        if text.trim().is_empty() {
            set(&self.shared, |s| s.status = "Nothing to type. Paste some text first.".into());
            return;
        }
        self.stop.store(false, Ordering::SeqCst);
        set(&self.shared, |s| {
            s.running = true;
            s.progress = 0.0;
        });
        let (shared, stop, ctx) = (self.shared.clone(), self.stop.clone(), ctx.clone());
        let (delay, interval, fin) = (
            self.delay_s.max(1),
            Duration::from_millis(self.key_ms),
            self.final_enter,
        );
        thread::spawn(move || run(text, delay, interval, fin, shared, stop, ctx));
    }

    // Shrinks or restores the window and flips compact_mode.
    fn set_compact(&mut self, ctx: &egui::Context, compact: bool) {
        self.compact_mode = compact;
        let size = if compact { COMPACT_SIZE } else { NORMAL_SIZE };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        // Compact mode has a fixed small size; let the normal layout be
        // resized freely again once we return to it.
        ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(!compact));
    }

    // Applies the always-on-top setting to the actual OS window.
    // Called once per frame; sending the command is cheap, so no change
    // tracking is needed.
    fn apply_window_level(&self, ctx: &egui::Context) {
        let level = if self.always_on_top {
            egui::WindowLevel::AlwaysOnTop
        } else {
            egui::WindowLevel::Normal
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
    }
}

impl eframe::App for VmTyper {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (status, progress, running) = {
            let s = self.shared.lock().unwrap();
            (s.status.clone(), s.progress, s.running)
        };

        self.apply_window_level(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.compact_mode {
                // Minimal layout: just the controls someone needs while
                // the VM window has focus. "Return" undoes the shrink.
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!running, egui::Button::new("Start")).clicked() {
                        self.start(ctx);
                    }
                    if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                        self.stop.store(true, Ordering::SeqCst);
                    }
                    if ui.button("Return").clicked() {
                        self.set_compact(ctx, false);
                    }
                });
                ui.add_space(6.0);
                ui.add(egui::ProgressBar::new(progress));
                return;
            }

            match self.input_mode {
                InputMode::Text => {
                    ui.horizontal(|ui| {
                        ui.label("Text to type");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(format!("{} characters", self.text.chars().count()));
                        });
                    });

                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        ui.add_enabled(
                            !running,
                            egui::TextEdit::multiline(&mut self.text)
                                .desired_width(f32::INFINITY)
                                .desired_rows(12),
                        );
                    });

                    ui.horizontal(|ui| {
                        if ui.add_enabled(!running, egui::Button::new("Paste clipboard")).clicked() {
                            self.paste_clipboard();
                        }
                        if ui.add_enabled(!running, egui::Button::new("Clear")).clicked() {
                            self.text.clear();
                            set(&self.shared, |s| s.progress = 0.0);
                        }
                    });
                }
                InputMode::File => {
                    ui.horizontal(|ui| {
                        ui.label("Drop a text file below");
                        if self.dropped_file.is_some() {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(format!("{} characters", self.text.chars().count()));
                            });
                        }
                    });

                    // Drop zone: a bordered area that reacts to files being
                    // dragged over it, and shows a short preview once a
                    // file is loaded.
                    let hovering_file = ctx.input(|i| !i.raw.hovered_files.is_empty());
                    let frame = egui::Frame::group(ui.style())
                        .fill(if hovering_file {
                            ui.visuals().widgets.hovered.weak_bg_fill
                        } else {
                            ui.visuals().extreme_bg_color
                        })
                        .inner_margin(10.0);

                    frame.show(ui, |ui| {
                        ui.set_min_height(240.0);
                        ui.set_width(ui.available_width());

                        if let Some(path) = &self.dropped_file {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.display().to_string());
                            ui.strong(format!("Loaded: {name}"));
                            ui.add_space(4.0);
                            egui::ScrollArea::vertical().max_height(190.0).show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut self.preview.as_str())
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(10),
                                );
                            });
                        } else {
                            ui.vertical_centered(|ui| {
                                ui.add_space(80.0);
                                ui.label("Drag and drop a text file here");
                                ui.weak("(the file's text will be typed, not re-read at start)");
                            });
                        }
                    });

                    ui.horizontal(|ui| {
                        if ui.add_enabled(!running, egui::Button::new("Clear file")).clicked() {
                            self.clear_file();
                        }
                    });

                    // Actually handle a drop that happened this frame.
                    let dropped = ctx.input(|i| i.raw.dropped_files.clone());
                    if !running {
                        if let Some(file) = dropped.into_iter().find_map(|f| f.path) {
                            self.load_file(file);
                        }
                    }
                }
            }

            ui.separator();

            // Mode toggle, separate from the Settings group below since it
            // changes what the main panel shows, not a typing behavior.
            ui.horizontal(|ui| {
                ui.label("Input source:");
                if ui
                    .selectable_label(matches!(self.input_mode, InputMode::Text), "Text / clipboard")
                    .clicked()
                {
                    self.input_mode = InputMode::Text;
                }
                if ui
                    .selectable_label(matches!(self.input_mode, InputMode::File), "File drop")
                    .clicked()
                {
                    self.input_mode = InputMode::File;
                }
            });

            ui.separator();
            ui.group(|ui| {
                ui.set_width(ui.available_width()); // fill the window width
                ui.strong("Settings");
                ui.add_space(4.0);

                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([12.0, 6.0]) // [horizontal, vertical] gap
                    .show(ui, |ui| {
                        ui.label("Start delay (s)");
                        ui.add_sized([70.0, 20.0], egui::DragValue::new(&mut self.delay_s).range(1..=60));
                        ui.end_row();
                        ui.label("Per-key delay (ms)");
                        ui.add_sized([70.0, 20.0], egui::DragValue::new(&mut self.key_ms).range(0..=500).speed(5));
                        ui.end_row();
                    });
                ui.add_space(4.0);
                ui.checkbox(&mut self.clean, "Replace smart quotes and dashes with plain ASCII");
                ui.checkbox(&mut self.strip_indent, "Strip leading indentation (for editors that auto-indent)");
                ui.checkbox(&mut self.final_enter, "Press Enter when finished");
                ui.checkbox(&mut self.always_on_top, "Keep window always on top");

                // --- Add more settings here -------------------------------
                // Simple on/off: ui.checkbox(&mut self.your_field, "Label");
                // A number: add a row to the Grid above, same pattern as
                //   "Start delay" / "Per-key delay".
                // Don't forget to add the matching field to VmTyper's
                // struct definition and to VmTyper::new()'s initializer.
                // -----------------------------------------------------------
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!running, egui::Button::new("Start typing")).clicked() {
                    self.start(ctx);
                }
                if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                    self.stop.store(true, Ordering::SeqCst);
                }
                if ui.button("Shrink").clicked() {
                    self.set_compact(ctx, true);
                }
            });

            ui.add(egui::ProgressBar::new(progress));
            ui.label(status);
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 620.0])
            .with_min_inner_size([440.0, 520.0])
            .with_always_on_top(), // initial state; toggled at runtime via the Settings checkbox
        ..Default::default()
    };
    eframe::run_native(
        "VM Typer",
        options,
        Box::new(|_cc| Ok(Box::new(VmTyper::new()))),
    )
}
