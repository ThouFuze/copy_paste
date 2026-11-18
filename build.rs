// Runs at compile time, before main.rs, to embed a Windows .exe icon.
// Does nothing on non-Windows targets.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("resource/icon.ico");
        res.compile().unwrap();
    }
}