//! Diagnostic probe for the game detector: every 2 s, print how the current
//! foreground window scores against the fullscreen-game heuristic (process, rect,
//! covers-monitor, verdict). Alt-tab into the game while it runs; the output shows
//! exactly which step accepts or rejects it.
//!
//! `cargo run -p rewynd-capture --example game_probe`

#[cfg(target_os = "windows")]
fn main() {
    println!("probing the foreground window every 2s; Ctrl+C to stop");
    loop {
        println!("{}", rewynd_capture::windows::describe_foreground());
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("Windows only");
}
