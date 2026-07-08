//! Embed the brand icon into the Windows exe (taskbar, Start menu, Explorer). The runtime
//! window icon comes from BRAND_ICONS in rewynd-config; this resource is what the shell shows
//! for the .exe itself.

fn main() {
    // The build-dep is host-gated to Windows; this env check gates on the *target* so a
    // cross-build never tries to embed a Windows resource.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let ico = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("../../packaging/rewynd.ico");
        println!("cargo:rerun-if-changed={}", ico.display());
        winresource::WindowsResource::new()
            .set_icon(ico.to_str().expect("utf-8 icon path"))
            .compile()
            .expect("embedding the Windows icon resource");
    }
}
