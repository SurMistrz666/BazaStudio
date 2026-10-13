//! Embeds the application icon and product name into the Windows executable.
//!
//! The resource compiler only exists on Windows, so the dependency and the code are limited
//! to Windows hosts; every other build runs an empty script.

const ICON: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/brand/baza-studio.ico"
);

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={ICON}");
    #[cfg(windows)]
    embed_windows_resources();
}

#[cfg(windows)]
fn embed_windows_resources() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let mut resources = winresource::WindowsResource::new();
    resources
        .set_icon(ICON)
        .set("ProductName", "Baza Studio")
        .set("FileDescription", "Baza Studio")
        .set(
            "LegalCopyright",
            "Copyright (C) 2026 SurMistrz666. GNU GPL version 3 or later.",
        );
    resources
        .compile()
        .expect("embedding the Windows application icon failed");
}
