//! Resolve the compile-time YouTube OAuth client vars and forward them to the crate's compilation
//! as *tracked* rustc-env values (`youtube.rs` reads them via `option_env!`). Without this, cargo
//! wouldn't notice a change, so a rebuild after setting them would keep the stale (empty) build.
//!
//! Values come from a real environment variable first (that's what CI uses), else from a gitignored
//! `.env` at the repo root — so a local dev can keep the id/secret in a file instead of exporting
//! them into every shell. `rerun-if-changed`/`rerun-if-env-changed` make cargo recompile on change.
use std::collections::HashMap;
use std::path::PathBuf;

const VARS: [&str; 2] = ["REWYND_YT_CLIENT_ID", "REWYND_YT_CLIENT_SECRET"];

fn main() {
    // This crate is <root>/crates/upload; the repo root is two levels up.
    let dotenv = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join(".env"));

    let from_file = if let Some(path) = &dotenv {
        println!("cargo:rerun-if-changed={}", path.display());
        std::fs::read_to_string(path)
            .map(parse_dotenv)
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    for var in VARS {
        println!("cargo:rerun-if-env-changed={var}");
        let value = std::env::var(var)
            .ok()
            .or_else(|| from_file.get(var).cloned());
        if let Some(value) = value {
            println!("cargo:rustc-env={var}={value}");
        }
    }
}

/// A tiny `KEY=value` reader (optional `export`, `#` comments, surrounding quotes). Only the two
/// keys above are used; anything else is ignored.
fn parse_dotenv(contents: String) -> HashMap<String, String> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line);
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let value = value.trim().trim_matches(['"', '\'']);
            Some((key.trim().to_owned(), value.to_owned()))
        })
        .collect()
}
