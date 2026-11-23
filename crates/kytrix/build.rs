//! Embeds the Windows resources into the executable, and names this crate's one
//! feature/platform combination.
//!
//! The icon is a repository asset rather than a crate asset, so its path is
//! declared here as well: once a build script emits `rerun-if-changed`, Cargo
//! watches only the paths named, and embed-resource does not name the script it
//! compiles.
//!
//! `ai_translation` is the local translation engine being part of the build: the `ai`
//! feature is on **and** the target has a llama.cpp build in this workspace (Windows ARM64
//! has none, so the dependency is absent there and no `feature = "ai"` can link it).

fn main() {
    println!("cargo::rerun-if-changed=app.rc");
    println!("cargo::rerun-if-changed=../../assets/branding/kytrix.ico");
    println!("cargo::rerun-if-changed=../../assets/branding/kytrix_bordered.ico");
    println!("cargo::rustc-check-cfg=cfg(ai_translation)");

    let windows_arm64 = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64");
    if std::env::var_os("CARGO_FEATURE_AI").is_some() && !windows_arm64 {
        println!("cargo::rustc-cfg=ai_translation");
    }

    // Required rather than optional: `.manifest_optional()` also accepts
    // "compilation not attempted" (no windres/llvm-rc on the non-MSVC paths) as
    // success, which would silently produce a Windows binary without the icon.
    // Non-Windows targets succeed either way.
    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed the Windows icon resource");
}
