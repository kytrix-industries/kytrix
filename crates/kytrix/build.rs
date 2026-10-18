//! Embeds the Windows resources into the executable.
//!
//! The icon is a repository asset rather than a crate asset, so its path is
//! declared here as well: once a build script emits `rerun-if-changed`, Cargo
//! watches only the paths named, and embed-resource does not name the script it
//! compiles.

fn main() {
    println!("cargo::rerun-if-changed=app.rc");
    println!("cargo::rerun-if-changed=../../assets/branding/kytrix.ico");
    println!("cargo::rerun-if-changed=../../assets/branding/kytrix_bordered.ico");

    // Required rather than optional: `.manifest_optional()` also accepts
    // "compilation not attempted" (no windres/llvm-rc on the non-MSVC paths) as
    // success, which would silently produce a Windows binary without the icon.
    // Non-Windows targets succeed either way.
    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed the Windows icon resource");
}
