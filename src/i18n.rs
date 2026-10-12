//! Встроенные строки EN/RU; никаких внешних каталогов и зависимостей.
//!
//! Устройство намеренно повторяет подход SNATCH: язык — это глобальный
//! `AtomicU8` плюс макросы `tr!` / `tr_format!`, которые выбирают вариант на
//! месте вызова. Так UI, CLI и отчёт об удалении всегда говорят на одном
//! языке и не могут «разъехаться».
use std::sync::atomic::{AtomicU8, Ordering};

use serde::Serialize;
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Ru,
}

impl Language {
    pub fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "en" => Ok(Self::En),
            "ru" => Ok(Self::Ru),
            _ => Err("expected en or ru"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Ru => "Русский",
        }
    }

    /// Значение для `--lang`. Отличается от [`Language::label`]: тот — название
    /// для людей («Русский»), и `--lang русский` не разбирается.
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ru => "ru",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::En => Self::Ru,
            Self::Ru => Self::En,
        }
    }
}

static LANGUAGE: AtomicU8 = AtomicU8::new(0);

const PREFERENCES_KEY: &str = r"Software\RAZE";

/// Saved per-user language; an absent preference uses English.
pub fn load_language() -> std::io::Result<Language> {
    read_language(&RegKey::predef(HKEY_CURRENT_USER), PREFERENCES_KEY)
}

/// Save only an explicit user choice, not a command-line override.
pub fn save_language(language: Language) -> std::io::Result<()> {
    write_language(&RegKey::predef(HKEY_CURRENT_USER), PREFERENCES_KEY, language)
}

fn read_language(root: &RegKey, path: &str) -> std::io::Result<Language> {
    let key = match root.open_subkey_with_flags(path, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Language::En),
        Err(error) => return Err(error),
    };
    let value: String = match key.get_value("Language") {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Language::En),
        Err(error) => return Err(error),
    };
    Language::parse(&value)
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidData, message))
}

fn write_language(root: &RegKey, path: &str, language: Language) -> std::io::Result<()> {
    let (key, _) = root.create_subkey(path)?;
    key.set_value("Language", &language.code())
}

pub fn set_language(language: Language) {
    LANGUAGE.store(u8::from(language == Language::Ru), Ordering::Relaxed);
}

pub fn language() -> Language {
    if is_russian() {
        Language::Ru
    } else {
        Language::En
    }
}

pub fn is_russian() -> bool {
    LANGUAGE.load(Ordering::Relaxed) == 1
}


#[macro_export]
macro_rules! tr {
    ($en:expr, $ru:expr) => {
        if $crate::i18n::is_russian() {
            $ru
        } else {
            $en
        }
    };
}

#[macro_export]
macro_rules! tr_format {
    // Хвост перехватывается целиком: так работают и позиционные аргументы,
    // и именованные (`verb = verb`), которые читаются в местах с длинными
    // русскими строками.
    ($en:literal, $ru:literal $($args:tt)*) => {
        if $crate::i18n::is_russian() {
            format!($ru $($args)*)
        } else {
            format!($en $($args)*)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_language_code_round_trips_through_parse() {
        for language in [Language::En, Language::Ru] {
            assert_eq!(Language::parse(language.code()), Ok(language));
        }
        // Название для людей — не код: на нём и спотыкалась передача языка
        // повышенному процессу.
        assert!(Language::parse(Language::Ru.label().to_lowercase().as_str()).is_err());
    }


    #[test]
    fn saved_language_survives_reopening_and_invalid_data_is_not_overwritten() {
        let root = RegKey::predef(HKEY_CURRENT_USER);
        let path = format!(r"Software\RAZE.Tests\language-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        assert_eq!(read_language(&root, &path).unwrap(), Language::En);
        for language in [Language::Ru, Language::En] {
            write_language(&root, &path, language).unwrap();
            assert_eq!(read_language(&RegKey::predef(HKEY_CURRENT_USER), &path).unwrap(), language);
        }
        let key = root.open_subkey_with_flags(&path, winreg::enums::KEY_WRITE | KEY_READ).unwrap();
        key.set_value("Language", &"unsupported").unwrap();
        assert_eq!(read_language(&root, &path).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(key.get_value::<String, _>("Language").unwrap(), "unsupported");
        drop(key);
        root.delete_subkey_all(&path).unwrap();
    }
}