//! Embeds the Mac receiver's app icon and the version info in the exe. The
//! resource compiler (`rc.exe`) ships with the Windows SDK, so this only runs
//! when building on Windows; `cargo check` from Linux or macOS skips it.

fn main() {
    println!("cargo:rerun-if-changed=assets/OpenDisplay Receiver.ico");
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    embed();
}

#[cfg(windows)]
fn embed() {
    let mut res = winresource::WindowsResource::new();
    // Resource id 1: the window class loads it for the title bar and taskbar.
    res.set_icon("assets/OpenDisplay Receiver.ico");
    res.set("ProductName", "OpenDisplay Receiver");
    res.set("FileDescription", "OpenDisplay Receiver");
    res.set("OriginalFilename", "OpenDisplay Receiver.exe");
    res.set("InternalName", "OpenDisplay Receiver");
    res.set("LegalCopyright", "GPL-3.0-only");
    if let Err(e) = res.compile() {
        // A missing resource compiler must not stop the build: the exe only
        // loses its icon.
        println!("cargo:warning=could not embed the icon and version info: {e}");
    }
}
