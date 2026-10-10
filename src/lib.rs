//! RAZE — точечное удаление застрявших файлов и папок в Windows.
//!
//! Ключевая идея: с Windows API работаем только через **дескрипторы** и
//! **расширенные пути** (`\\?\...`). Обычный путь `C:\dir\NUL` система
//! превращает в устройство `NUL` ещё на этапе разбора имени, и никакая
//! корзина такой файл уже не тронет. С префиксом `\\?\` разбор имени
//! отключается — `NUL` становится обычным именем файла.

pub mod engine;
pub mod i18n;
pub mod job;
pub mod protect;
pub mod report;
pub mod shell;

pub const BANNER: &str = r#"
 ____                    __  __
/\  _`\                  |  \/  |
\ \ \L\ \    __  __ __  __| |\/| | __ __ _ __  __ _
 \ \  __ \  / _`/` _` \/ _` | |  | / _` | '_ \/ _` |
  \ \ \L\ \  \__, \__, /\__,_|_|  |_\__, | | | \__, |
   \ \____/  |___/|___/    /_/\_\____/_| |_| |___/
    \/___/   stubborn files, clean folders
"#;

pub const TELEGRAM_URL: &str = "https://t.me/rercon";

/// Текущая версия приложения — из `CARGO_PKG_VERSION`, чтобы установщик
/// меню Windows и `--version` не расходились.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn outln(s: impl AsRef<str>) {
    println!("{}", s.as_ref());
}

pub fn errln(s: impl AsRef<str>) {
    eprintln!("{}", s.as_ref());
}