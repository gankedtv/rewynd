//! Stage the Setup.exe payload and embed the Windows exe icon.
//!
//! The release build points `REWYND_SETUP_EXE` at the freshly packed Velopack Setup.exe, which
//! is copied into `OUT_DIR` for `include_bytes!`. Dev builds leave the variable unset and get an
//! empty payload — the installer then falls back to a Setup.exe sitting beside it at runtime.

fn main() {
    let out =
        std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("setup-payload.bin");
    println!("cargo:rerun-if-env-changed=REWYND_SETUP_EXE");
    match std::env::var_os("REWYND_SETUP_EXE") {
        Some(setup) => {
            let setup = std::path::PathBuf::from(setup);
            println!("cargo:rerun-if-changed={}", setup.display());
            std::fs::copy(&setup, &out).expect("copying the Setup.exe payload into OUT_DIR");
        }
        None => std::fs::write(&out, []).expect("writing the empty dev payload"),
    }

    // The build-dep is host-gated to Windows; this env check gates on the *target* so a
    // cross-build never tries to embed a Windows resource.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let ico = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("../../packaging/rewynd.ico");
        println!("cargo:rerun-if-changed={}", ico.display());
        // The manifest matters here more than for the app exes: Windows' installer-detection
        // heuristic sees "installer" in the name and demands elevation unless the exe declares
        // `asInvoker` itself — the per-user, no-admin install is the whole point.
        winresource::WindowsResource::new()
            .set_icon(ico.to_str().expect("utf-8 icon path"))
            .set_manifest(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>"#,
            )
            .compile()
            .expect("embedding the Windows icon resource");
    }
}
