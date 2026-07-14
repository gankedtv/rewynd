//! Focus-watcher probe: print the focused fullscreen game as it changes, proving the
//! platform backend works. Run it, focus a fullscreen window, alt-tab around.

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() {
    tracing_subscriber::fmt::init();
    #[cfg(target_os = "linux")]
    use rewynd_capture::linux::FocusWatcher;
    #[cfg(target_os = "macos")]
    use rewynd_capture::macos::FocusWatcher;
    let watcher = match FocusWatcher::spawn(None) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("focus watcher unavailable: {e}");
            std::process::exit(1);
        }
    };
    println!("backend: {}", watcher.backend());
    println!("watching for 30s; focus/unfocus a fullscreen window...");
    let mut last: Option<rewynd_capture::game::GameInfo> = None;
    for _ in 0..120 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let current = watcher.current_game();
        if current != last {
            match &current {
                Some(game) => println!(
                    "GAME: app_id={:?} pid={:?} name={:?}",
                    game.app_id,
                    game.pid,
                    game.display_name()
                ),
                None => println!("no fullscreen game focused"),
            }
            last = current;
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {
    eprintln!("this probe runs on Linux and macOS only");
}
