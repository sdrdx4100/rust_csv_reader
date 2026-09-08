//! Build script: embed the Windows application icon into the `.exe` files so
//! they show a proper icon in Explorer and the taskbar. No-op on other targets.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        // Best-effort: a missing resource compiler shouldn't fail the build.
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {e}");
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
