//! Diagnostic probe for #4: open the XDG ScreenCast portal, connect to its
//! PipeWire remote, negotiate a DMA-BUF video format, and log the PipeWire node
//! id, the negotiated format, and the first few frames' DMA-BUF descriptors
//! (fd / DRM fourcc / DRM modifier / size / stride / offset).
//!
//! Run it (Linux, live Wayland session, interactive share dialog) with:
//!
//! ```text
//! cargo run -p rewynd-capture --example linux_capture_probe
//! ```
//!
//! On non-Linux targets this compiles to a stub that just prints "Linux only".

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use rewynd_capture::linux::{open_portal, run_capture_probe};

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    // The portal flow is async (ashpd uses tokio). Keep the runtime alive for the
    // whole program: the PipeWire main loop below blocks the main thread, and the
    // portal Session must outlive it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let mut portal = runtime.block_on(open_portal())?;

    tracing::info!(
        node_id = portal.node_id,
        fd = portal.raw_fd(),
        size = ?portal.size,
        "portal session established"
    );

    // Hand the fd to PipeWire. `run_capture_probe` blocks on the PipeWire main
    // loop until enough DMA-BUF frames are logged. `portal` (and thus the
    // Session) and `runtime` stay alive until after it returns.
    let node_id = portal.node_id;
    let fd = portal.take_fd();
    run_capture_probe(node_id, fd)?;

    // Keep the portal session + runtime explicitly alive until here.
    drop(portal);
    drop(runtime);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("Linux only");
}
