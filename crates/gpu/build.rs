fn main() {
    // `vulkan`: targets where gpu-video (Vulkan Video) builds — Windows + non-Apple
    // unixes. Must match the `[target.'cfg(...)']` gpu-video gating in Cargo.toml.
    cfg_aliases::cfg_aliases! {
        vulkan: {
            any(
                windows,
                all(
                    unix,
                    not(target_os = "macos"),
                    not(target_os = "ios"),
                    not(target_os = "emscripten")
                )
            )
        },
    }
}
