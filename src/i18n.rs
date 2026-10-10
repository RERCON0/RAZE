//! Встроенные строки EN/RU; никаких внешних каталогов и зависимостей.
//!
//! Устройство намеренно повторяет подход SNATCH: язык — это глобальный
//! `AtomicU8` плюс макросы `tr!` / `tr_format!`, которые выбирают вариант на
//! месте вызова. Так UI, CLI и отчёт об удалении всегда говорят на одном
//! языке и не могут «разъехаться».
use std::sync::atomic::{AtomicU8, Ordering};

use serde::Serialize;

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

    pub fn toggled(self) -> Self {
        match self {
            Self::En => Self::Ru,
            Self::Ru => Self::En,
        }
    }
}

static LANGUAGE: AtomicU8 = AtomicU8::new(0);

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
    #[cfg(test)]
    if let Some(language) = TEST_LANGUAGE.with(|slot| slot.get()) {
        return language == Language::Ru;
    }
    LANGUAGE.load(Ordering::Relaxed) == 1
}

#[cfg(test)]
thread_local! {
    static TEST_LANGUAGE: std::cell::Cell<Option<Language>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_language<T>(language: Language, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<Language>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LANGUAGE.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(TEST_LANGUAGE.with(|slot| slot.replace(Some(language))));
    action()
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
    fn both_languages_cover_every_removal_reason() {
        for language in [Language::En, Language::Ru] {
            with_language(language, || {
                let report = crate::report::Report::demo();
                let text = report.summary_line().to_lowercase();
                if language == Language::En {
                    assert!(text.contains("removed"), "{text}");
                } else {
                    assert!(text.contains("удалено"), "{text}");
                }
            });
        }
    }
}