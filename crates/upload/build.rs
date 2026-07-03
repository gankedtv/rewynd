//! Forward the compile-time YouTube OAuth client vars to the crate's compilation as *tracked*
//! rustc-env values. `youtube.rs` reads them via `option_env!`; without this, cargo doesn't notice
//! when the ambient vars change, so a rebuild after setting them would keep the stale (empty) build.
//! Re-emitting them as `rustc-env` (plus `rerun-if-env-changed`) makes cargo recompile on change.
fn main() {
    for var in ["REWYND_YT_CLIENT_ID", "REWYND_YT_CLIENT_SECRET"] {
        println!("cargo:rerun-if-env-changed={var}");
        if let Ok(value) = std::env::var(var) {
            println!("cargo:rustc-env={var}={value}");
        }
    }
}
