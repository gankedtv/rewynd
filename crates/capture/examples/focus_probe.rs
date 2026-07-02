//! Focus-watcher probe: print the focused fullscreen game as it changes, proving the
//! compositor backend works. Run it, focus a fullscreen window, alt-tab around.

#[cfg(target_os = "linux")]
fn main() {
    tracing_subscriber::fmt::init();
    let watcher = match rewynd_capture::linux::FocusWatcher::spawn(None) {
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

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("this probe is Linux-only");
}
