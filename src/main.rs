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
    File2,
}

const PREVIEW_CHARS: usize = 600;
const NORMAL_SIZE: egui::Vec2 = egui::Vec2::new(520.0, 680.0);
const COMPACT_SIZE: egui::Vec2 = egui::Vec2::new(230.0, 95.0);
const FAST_KEY_MS: u64 = 5;
const FAST_CHUNK: u64 = 20;

struct Shared {
    status: String,
    progress: f32,
    running: bool,
}

struct QueuedScript {
    path: PathBuf,
    text: String,
    preview: String,
}

struct TypeJob {
    label: String,
    text: String,
}

// --- DuckyScript Parser (Dedicated to File2) --------------------------------
#[derive(Debug, Clone)]
enum DuckyCommand {
    String(String),
    StringLn(String),
    Delay(u64),
    Keys(Vec<String>),
}

fn parse_duckyscript(content: &str) -> Vec<DuckyCommand> {
    let mut commands = Vec::new();
    let mut in_rem_block = false;

    for line in content.lines() {
        let line_trim = line.trim();
        if line_trim.is_empty() {
            continue;
        }
        if line_trim.starts_with("REM_BLOCK") {
            in_rem_block = true;
            continue;
        }
        if line_trim.starts_with("END_REM") {
            in_rem_block = false;
            continue;
        }
        if in_rem_block || line_trim.starts_with("//") || line_trim.starts_with("REM") {
            continue;
        }

        if let Some(rest) = line_trim.strip_prefix("STRINGLN ") {
            commands.push(DuckyCommand::StringLn(rest.to_string()));
        } else if let Some(rest) = line_trim.strip_prefix("STRING ") {
            commands.push(DuckyCommand::String(rest.to_string()));
        } else if let Some(rest) = line_trim.strip_prefix("DELAY ") {
            if let Ok(ms) = rest.trim().parse::<u64>() {
                commands.push(DuckyCommand::Delay(ms));
            }
        } else {
            let parts: Vec<String> = line_trim.split_whitespace().map(|s| s.to_uppercase()).collect();
            if !parts.is_empty() {
                commands.push(DuckyCommand::Keys(parts));
            }
        }
    }
    commands
}

fn map_key_name(name: &str) -> Option<Key> {
    match name {
        "ENTER" => Some(Key::Return),
        "TAB" => Some(Key::Tab),
        "SPACE" => Some(Key::Space),
        "BACKSPACE" => Some(Key::Backspace),
        "ESC" | "ESCAPE" => Some(Key::Escape),
        "DELETE" => Some(Key::Delete),
        "CTRL" | "RCTRL" => Some(Key::Control),
        "SHIFT" | "RSHIFT" => Some(Key::Shift),
        "ALT" | "RALT" => Some(Key::Alt),
        "WINDOWS" | "RWINDOWS" | "GUI" | "COMMAND" | "RCOMMAND" => Some(Key::Meta),
        other => {
            if other.chars().count() == 1 {
                let c = other.chars().next()?.to_ascii_uppercase();
                match c {
                    'A' => Some(Key::A), 'B' => Some(Key::B), 'C' => Some(Key::C),
                    'D' => Some(Key::D), 'E' => Some(Key::E), 'F' => Some(Key::F),
                    'G' => Some(Key::G), 'H' => Some(Key::H), 'I' => Some(Key::I),
                    'J' => Some(Key::J), 'K' => Some(Key::K), 'L' => Some(Key::L),
                    'M' => Some(Key::M), 'N' => Some(Key::N), 'O' => Some(Key::O),
                    'P' => Some(Key::P), 'Q' => Some(Key::Q), 'R' => Some(Key::R),
                    'S' => Some(Key::S), 'T' => Some(Key::T), 'U' => Some(Key::U),
                    'V' => Some(Key::V), 'W' => Some(Key::W), 'X' => Some(Key::X),
                    'Y' => Some(Key::Y), 'Z' => Some(Key::Z),
                    _ => None,
                }
            } else {
                None
            }
        }
    }
}
// ---------------------------------------------------------------------------

struct VmTyper {
    text: String,
    input_mode: InputMode,
    file_queue: Vec<QueuedScript>,
    preview_idx: Option<usize>,
    file_queue_b: Vec<QueuedScript>,
    preview_idx_b: Option<usize>,

    delay_s: u64,
    key_ms: u64,
    chunk_size: u64,
    queue_pause_s: u64,
    clean: bool,
    strip_indent: bool,
    paste_safe: bool,
    final_enter: bool,
    always_on_top: bool,
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

fn type_ducky_char(enigo: &mut Enigo, ch: char) {
    // One key at a time. Do not tap Alt, Ctrl, or Win — a bare Alt key-up
    // opens Notepad's menu and the following letters jump around the window.
    // Only Shift is released, because enigo leaves it down after '!'.
    let _ = enigo.text(&ch.to_string());
    let _ = enigo.key(Key::Unicode(ch), Direction::Release);
    let _ = enigo.key(Key::Shift, Direction::Release);
    thread::sleep(Duration::from_millis(15));
}

fn type_ducky_string(enigo: &mut Enigo, s: &str, stop: &AtomicBool) -> bool {
    for ch in s.chars() {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        match ch {
            '\n' | '\r' => {
                let _ = enigo.key(Key::Return, Direction::Click);
                thread::sleep(Duration::from_millis(15));
            }
            '\t' => {
                let _ = enigo.key(Key::Tab, Direction::Click);
                thread::sleep(Duration::from_millis(15));
            }
            _ => type_ducky_char(enigo, ch),
        }
    }
    true
}

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
        format!("{truncated}\n\n… (preview truncated)")
    }
}

fn run_queue(
    jobs: Vec<TypeJob>,
    delay: u64,
    interval: Duration,
    chunk_size: u64,
    queue_pause: Duration,
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

    let total_jobs = jobs.len();
    for (idx, job) in jobs.iter().enumerate() {
        let chars: Vec<char> = job.text.chars().collect();
        let _total = chars.len();
        let mut buf = String::new();

        for (_i, &ch) in chars.iter().enumerate() {
            if stop.load(Ordering::SeqCst) {
                return finish("Stopped.".into());
            }
            if ch == '\n' {
                if !buf.is_empty() {
                    let _ = enigo.text(&buf);
                    buf.clear();
                }
                let _ = enigo.key(Key::Return, Direction::Click);
            } else {
                buf.push(ch);
                if buf.chars().count() >= chunk_size as usize {
                    let _ = enigo.text(&buf);
                    buf.clear();
                }
            }
            if !interval.is_zero() {
                thread::sleep(interval);
            }
        }
        if !buf.is_empty() {
            let _ = enigo.text(&buf);
        }

        let is_last = idx + 1 == total_jobs;
        if !is_last && !queue_pause.is_zero() {
            thread::sleep(queue_pause);
        }
    }

    if final_enter {
        let _ = enigo.key(Key::Return, Direction::Click);
    }
    finish("Done.".to_string())
}

fn run_duckyscript_queue(
    scripts: Vec<(String, String)>,
    delay: u64,
    queue_pause: Duration,
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

    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => return finish(format!("Could not init input simulation: {e}")),
    };

    for i in (1..=delay).rev() {
        set(&shared, |s| {
            s.status = format!("Click inside the VM console. DuckyScript starts in {i}s...")
        });
        ctx.request_repaint();
        for _ in 0..10 {
            if stop.load(Ordering::SeqCst) {
                return finish("Stopped before execution started.".into());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    let total_scripts = scripts.len();

    for (idx, (label, content)) in scripts.into_iter().enumerate() {
        let commands = parse_duckyscript(&content);
        let cmd_total = commands.len();

        for (c_idx, cmd) in commands.into_iter().enumerate() {
            if stop.load(Ordering::SeqCst) {
                return finish(format!("Stopped at script '{label}'."));
            }
            if in_corner(&enigo) {
                return finish("Aborted by fail-safe.".into());
            }

            match cmd {
                DuckyCommand::String(s) => {
                    if !type_ducky_string(&mut enigo, &s, &stop) {
                        return finish(format!("Stopped at script '{label}'."));
                    }
                }
                DuckyCommand::StringLn(s) => {
                    if !type_ducky_string(&mut enigo, &s, &stop) {
                        return finish(format!("Stopped at script '{label}'."));
                    }
                    let _ = enigo.key(Key::Return, Direction::Click);
                }
                DuckyCommand::Delay(ms) => {
                    thread::sleep(Duration::from_millis(ms));
                }
                DuckyCommand::Keys(keys) => {
                    let mut mapped_keys = Vec::new();
                    for k in &keys {
                        if let Some(vk) = map_key_name(k) {
                            mapped_keys.push(vk);
                        }
                    }
                    for &vk in &mapped_keys {
                        let _ = enigo.key(vk, Direction::Press);
                    }
                    thread::sleep(Duration::from_millis(40));
                    for &vk in mapped_keys.iter().rev() {
                        let _ = enigo.key(vk, Direction::Release);
                    }
                }
            }

            set(&shared, |s| {
                s.progress = (c_idx + 1) as f32 / cmd_total.max(1) as f32;
                s.status = format!("[{}/{}] {label}: command {} / {cmd_total}", idx + 1, total_scripts, c_idx + 1);
            });
            ctx.request_repaint();
        }

        let is_last = idx + 1 == total_scripts;
        if !is_last && !queue_pause.is_zero() {
            thread::sleep(queue_pause);
        }
    }

    finish("DuckyScript execution complete.".into());
}

impl VmTyper {
    fn new() -> Self {
        Self {
            text: String::new(),
            input_mode: InputMode::Text,
            file_queue: Vec::new(),
            preview_idx: None,
            file_queue_b: Vec::new(),
            preview_idx_b: None,
            delay_s: 5,
            key_ms: 40,
            chunk_size: 1,
            queue_pause_s: 2,
            clean: true,
            strip_indent: false,
            paste_safe: false,
            final_enter: false,
            always_on_top: true,
            compact_mode: false,
            shared: Arc::new(Mutex::new(Shared {
                status: "Paste text, or switch to File mode and drop scripts in.".into(),
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

    fn queue_file(&mut self, path: PathBuf) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let preview = make_preview(&content);
            self.file_queue.push(QueuedScript { path, text: content, preview });
            self.preview_idx = Some(self.file_queue.len() - 1);
            set(&self.shared, |s| s.status = format!("Queued: {name}"));
        }
    }

    fn remove_from_queue(&mut self, idx: usize) {
        if idx < self.file_queue.len() { self.file_queue.remove(idx); }
        self.preview_idx = None;
    }

    fn clear_queue(&mut self) {
        self.file_queue.clear();
        self.preview_idx = None;
    }

    fn queue_file_b(&mut self, path: PathBuf) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let preview = make_preview(&content);
            self.file_queue_b.push(QueuedScript { path, text: content, preview });
            self.preview_idx_b = Some(self.file_queue_b.len() - 1);
            set(&self.shared, |s| s.status = format!("Queued (B): {name}"));
        }
    }

    fn remove_from_queue_b(&mut self, idx: usize) {
        if idx < self.file_queue_b.len() { self.file_queue_b.remove(idx); }
        self.preview_idx_b = None;
    }

    fn clear_queue_b(&mut self) {
        self.file_queue_b.clear();
        self.preview_idx_b = None;
    }

    fn prepare_text(&self, raw: &str) -> String {
        let mut t = raw.replace("\r\n", "\n").replace('\r', "\n");
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
        if self.paste_safe {
            t = t
                .chars()
                .filter(|&c| c == '\n' || c == '\t' || c == ' ' || c.is_ascii_graphic())
                .collect();
        }
        t
    }

    fn start(&mut self, ctx: &egui::Context) {
        let delay_s = self.delay_s;
        let key_ms = self.key_ms;
        let chunk_size = self.chunk_size;
        let queue_pause_s = self.queue_pause_s;
        let final_enter = self.final_enter;

        match self.input_mode {
            InputMode::Text => {
                let text = self.prepare_text(&self.text);
                let jobs = vec![TypeJob { label: "Text".into(), text }];
                self.stop.store(false, Ordering::SeqCst);
                set(&self.shared, |s| { s.running = true; s.progress = 0.0; });
                let (shared, stop, ctx) = (self.shared.clone(), self.stop.clone(), ctx.clone());
                thread::spawn(move || run_queue(jobs, delay_s, Duration::from_millis(key_ms), chunk_size, Duration::ZERO, final_enter, shared, stop, ctx));
            }
            InputMode::File => {
                let jobs = self.file_queue.iter().map(|q| TypeJob { label: q.path.file_name().unwrap_or_default().to_string_lossy().into(), text: self.prepare_text(&q.text) }).collect();
                self.stop.store(false, Ordering::SeqCst);
                set(&self.shared, |s| { s.running = true; s.progress = 0.0; });
                let (shared, stop, ctx) = (self.shared.clone(), self.stop.clone(), ctx.clone());
                thread::spawn(move || run_queue(jobs, delay_s, Duration::from_millis(key_ms), chunk_size, Duration::from_secs(queue_pause_s), final_enter, shared, stop, ctx));
            }
            InputMode::File2 => {
                let scripts = self.file_queue_b.iter().map(|q| (q.path.file_name().unwrap_or_default().to_string_lossy().into(), q.text.clone())).collect();
                self.stop.store(false, Ordering::SeqCst);
                set(&self.shared, |s| { s.running = true; s.progress = 0.0; });
                let (shared, stop, ctx) = (self.shared.clone(), self.stop.clone(), ctx.clone());
                thread::spawn(move || run_duckyscript_queue(scripts, delay_s, Duration::from_secs(queue_pause_s), shared, stop, ctx));
            }
        }
    }

    fn apply_fast_preset(&mut self) {
        self.key_ms = FAST_KEY_MS;
        self.chunk_size = FAST_CHUNK;
    }

    fn set_compact(&mut self, ctx: &egui::Context, compact: bool) {
        self.compact_mode = compact;
        let size = if compact { COMPACT_SIZE } else { NORMAL_SIZE };
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        ctx.send_viewport_cmd(egui::ViewportCommand::Resizable(!compact));
    }

    fn apply_window_level(&self, ctx: &egui::Context) {
        let level = if self.always_on_top { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal };
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
                ui.horizontal(|ui| {
                    if ui.add_enabled(!running, egui::Button::new("Start")).clicked() { self.start(ctx); }
                    if ui.add_enabled(running, egui::Button::new("Stop")).clicked() { self.stop.store(true, Ordering::SeqCst); }
                    if ui.button("Return").clicked() { self.set_compact(ctx, false); }
                });
                ui.add(egui::ProgressBar::new(progress).show_percentage());
                ui.label(&status);
                return;
            }

            ui.horizontal(|ui| {
                ui.label("Mode:");
                ui.selectable_value(&mut self.input_mode, InputMode::Text, "Text");
                ui.selectable_value(&mut self.input_mode, InputMode::File, "File queue");
                ui.selectable_value(&mut self.input_mode, InputMode::File2, "DuckyScript");
            });
            ui.separator();

            match self.input_mode {
                InputMode::Text => {
                    ui.horizontal(|ui| {
                        if ui.button("Paste clipboard").clicked() {
                            self.paste_clipboard();
                        }
                        if ui.button("Clear").clicked() {
                            self.text.clear();
                        }
                    });
                    ui.add(
                        egui::TextEdit::multiline(&mut self.text)
                            .desired_rows(12)
                            .desired_width(f32::INFINITY)
                            .hint_text("Text to type into the VM console…"),
                    );
                }
                InputMode::File => {
                    ui.label("Drop text files onto this window to queue them.");
                    let dropped: Vec<PathBuf> = ctx.input(|i| {
                        i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
                    });
                    for path in dropped {
                        self.queue_file(path);
                    }
                    if ui.button("Clear queue").clicked() {
                        self.clear_queue();
                    }
                    let mut remove = None;
                    egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                        for (idx, q) in self.file_queue.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let name = q.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "(unnamed)".into());
                                if ui.selectable_label(self.preview_idx == Some(idx), name).clicked() {
                                    self.preview_idx = Some(idx);
                                }
                                if ui.small_button("x").clicked() {
                                    remove = Some(idx);
                                }
                            });
                        }
                    });
                    if let Some(idx) = remove {
                        self.remove_from_queue(idx);
                    }
                }
                InputMode::File2 => {
                    ui.label("Drop DuckyScript files. Typed one key at a time (chunk size ignored).");
                    let dropped: Vec<PathBuf> = ctx.input(|i| {
                        i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
                    });
                    for path in dropped {
                        self.queue_file_b(path);
                    }
                    if ui.button("Clear queue").clicked() {
                        self.clear_queue_b();
                    }
                    let mut remove = None;
                    egui::ScrollArea::vertical().max_height(140.0).show(ui, |ui| {
                        for (idx, q) in self.file_queue_b.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let name = q.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "(unnamed)".into());
                                if ui.selectable_label(self.preview_idx_b == Some(idx), name).clicked() {
                                    self.preview_idx_b = Some(idx);
                                }
                                if ui.small_button("x").clicked() {
                                    remove = Some(idx);
                                }
                            });
                        }
                    });
                    if let Some(idx) = remove {
                        self.remove_from_queue_b(idx);
                    }
                }
            }

            ui.separator();
            egui::CollapsingHeader::new("Settings").default_open(true).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Start delay (s)");
                    ui.add(egui::DragValue::new(&mut self.delay_s).range(0..=60));
                });
                ui.horizontal(|ui| {
                    ui.label("Key interval (ms)");
                    ui.add(egui::DragValue::new(&mut self.key_ms).range(0..=500));
                });
                ui.horizontal(|ui| {
                    ui.label("Chunk size (Text/File only)");
                    ui.add(egui::DragValue::new(&mut self.chunk_size).range(1..=200));
                });
                ui.horizontal(|ui| {
                    ui.label("Pause between files (s)");
                    ui.add(egui::DragValue::new(&mut self.queue_pause_s).range(0..=120));
                });
                if ui.button("Fast preset").clicked() {
                    self.apply_fast_preset();
                }
                ui.checkbox(&mut self.clean, "Clean smart quotes");
                ui.checkbox(&mut self.strip_indent, "Strip leading indent");
                ui.checkbox(&mut self.paste_safe, "ASCII paste-safe only");
                ui.checkbox(&mut self.final_enter, "Press Enter at the end");
                if ui.checkbox(&mut self.always_on_top, "Always on top").changed() {
                    self.apply_window_level(ctx);
                }
            });

            ui.separator();
            ui.horizontal(|ui| {
                if ui.add_enabled(!running, egui::Button::new("Start")).clicked() {
                    self.start(ctx);
                }
                if ui.add_enabled(running, egui::Button::new("Stop")).clicked() {
                    self.stop.store(true, Ordering::SeqCst);
                }
                if ui.button("Compact").clicked() {
                    self.set_compact(ctx, true);
                }
            });
            ui.add(egui::ProgressBar::new(progress).show_percentage());
            ui.label(&status);
        });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(NORMAL_SIZE)
            .with_always_on_top(),
        ..Default::default()
    };
    eframe::run_native(
        "VM Typer",
        options,
        Box::new(|_cc| Ok(Box::new(VmTyper::new()))),
    )
}
