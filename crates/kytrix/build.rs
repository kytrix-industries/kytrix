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

    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed the Windows icon resource");
}
