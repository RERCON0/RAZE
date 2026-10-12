fn main() {
    println!("cargo:rerun-if-changed=../icons/icon-shell.rc");
    println!("cargo:rerun-if-changed=../icons/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Иконка нужна самому меню: IExplorerCommand::GetIcon отдаёт
        // Проводнику ссылку на ресурс из этой DLL (точнее — на
        // иконку raze.exe, но она обязана существовать и здесь).
        // Крейт — cdylib, поэтому `compile`, а не `compile_for`:
        // последний ищет одноимённый bin-таргет.
        embed_resource::compile("../icons/icon-shell.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
    }
}
