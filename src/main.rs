// Put this file at src/main.rs next to Cargo.toml.
// VM Typer: types text into a VM console, one keystroke at a time.
// Abort any time: click Stop, or slam the mouse into a screen corner (fail-safe).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use enigo::{Direction, Enigo, Key, Keyboard, Mouse, Settings};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

struct Shared {
    status: String,
    progress: f32,
    running: bool,
}

struct VmTyper {
    text: String,
    delay_s: u64,
    key_ms: u64,
    clean: bool,
    strip_indent: bool,
    final_enter: bool,
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
            delay_s: 5,
            key_ms: 40,
            clean: true,
            strip_indent: false,
            final_enter: false,
            shared: Arc::new(Mutex::new(Shared {
                status: "Paste or type your text, then press Start.".into(),
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
}

impl eframe::App for VmTyper {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (status, progress, running) = {
            let s = self.shared.lock().unwrap();
            (s.status.clone(), s.progress, s.running)
        };

        egui::CentralPanel::default().show(ctx, |ui| {
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
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!running, egui::Button::new("Start typing")).clicked() {
                    self.start(ctx);
                }
                if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                    self.stop.store(true, Ordering::SeqCst);
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
            .with_inner_size([520.0, 580.0])
            .with_min_inner_size([440.0, 480.0])
            .with_always_on_top(), // stays visible over the VM window
        ..Default::default()
    };
    eframe::run_native(
        "VM Typer",
        options,
        Box::new(|_cc| Ok(Box::new(VmTyper::new()))),
    )
}