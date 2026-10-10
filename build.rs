fn main() {
    println!("cargo:rerun-if-changed=icons/icon-cli.rc");
    println!("cargo:rerun-if-changed=icons/icon-cli.ico");
    println!("cargo:rerun-if-changed=icons/icon-app.rc");
    println!("cargo:rerun-if-changed=icons/icon-app.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // egui::IconData влияет только на иконку окна во время работы. Иконку
        // самого .exe (то, что читают Проводник, ярлыки и Alt-Tab до запуска)
        // задаёт PE-ресурс, поэтому встраиваем иконки здесь же.
        embed_resource::compile_for("icons/icon-cli.rc", ["raze"], embed_resource::NONE)
            .manifest_optional()
            .unwrap();
        embed_resource::compile_for("icons/icon-app.rc", ["raze-app"], embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}