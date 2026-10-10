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
pub mod scrollbar;
pub mod shell;

/// The wordmark, set in the same FIGlet face as SNATCH ("Larry 3D"). The
/// window and the CLI banner draw this one block, so they cannot drift apart.
pub const WORDMARK: &str = r#"
 ____    ______  ________   ____
/\  _`\ /\  _  \/\_____  \ /\  _`\
\ \ \L\ \ \ \L\ \/____//'/'\ \ \L\_\
 \ \ ,  /\ \  __ \   //'/'  \ \  _\L
  \ \ \\ \\ \ \/\ \ //'/'___ \ \ \L\ \
   \ \_\ \_\ \_\ \_\/\_______\\ \____/
    \/_/\/ /\/_/\/_/\/_______/ \/___/
"#;

/// Tagline under the wordmark, in the interface language.
pub fn tagline() -> &'static str {
    tr!("stubborn files, clean folders", "сложные файлы. чистые папки.")
}

/// Wordmark plus the tagline centred under it, for the CLI.
pub fn banner() -> String {
    let art = WORDMARK.trim_matches(['\r', '\n']);
    let width = art.lines().map(|line| line.chars().count()).max().unwrap_or(0);
    let tagline = tagline();
    let pad = width.saturating_sub(tagline.chars().count()) / 2;
    format!("{art}\n{}{tagline}\n", " ".repeat(pad))
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordmark_is_seven_rows_and_the_banner_carries_the_tagline_in_both_languages() {
        assert_eq!(WORDMARK.trim_matches(['\r', '\n']).lines().count(), 7);
        for (language, expected) in [
            (i18n::Language::En, "stubborn files, clean folders"),
            (i18n::Language::Ru, "сложные файлы. чистые папки."),
        ] {
            i18n::with_language(language, || assert!(banner().contains(expected)));
        }
    }
}
