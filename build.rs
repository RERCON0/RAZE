fn main() {
    println!("cargo:rerun-if-changed=icons/icon.rc");
    println!("cargo:rerun-if-changed=icons/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Both executables use the same resource ID and artwork.
        // The runtime window icon uses the matching icons/icon.png.
        embed_resource::compile_for("icons/icon.rc", ["raze", "raze-app"], embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
