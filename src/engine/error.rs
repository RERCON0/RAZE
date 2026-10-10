//! Ошибки Windows: код плюс то, что человек реально может сделать.

use std::fmt;

use windows::Win32::Foundation::GetLastError;

/// Код ошибки Windows в обёртке, которая умеет отвечать на главный вопрос
/// удалялки: «это лечится повтором, правами или уже нет».
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NtError(pub u32);

pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
pub const ERROR_ACCESS_DENIED: u32 = 5;
pub const ERROR_SHARING_VIOLATION: u32 = 32;
pub const ERROR_LOCK_VIOLATION: u32 = 33;
pub const ERROR_DIR_NOT_EMPTY: u32 = 145;
pub const ERROR_INVALID_PARAMETER: u32 = 87;
pub const ERROR_NOT_SUPPORTED: u32 = 50;
pub const ERROR_INVALID_FUNCTION: u32 = 1;
pub const ERROR_PENDING: u32 = 161;
pub const ERROR_BUSY: u32 = 170;
pub const ERROR_CANNOT_DELETE: u32 = 1011;
pub const ERROR_USER_MAPPED_FILE: u32 = 1224;
pub const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;
pub const ERROR_FILE_IS_OFFLINE: u32 = 4350;

impl NtError {
    pub fn last() -> Self {
        Self(unsafe { GetLastError() }.0)
    }

    pub fn code(self) -> u32 {
        self.0
    }

    pub fn access_denied(self) -> bool {
        self.0 == ERROR_ACCESS_DENIED
    }

    /// Файл открыт кем-то без `FILE_SHARE_DELETE` — лечится закрытием
    /// держателя, а не правами.
    pub fn sharing_violation(self) -> bool {
        self.0 == ERROR_SHARING_VIOLATION || self.0 == ERROR_LOCK_VIOLATION
    }

    /// «Висящее» состояние: операция ещё не завершена, есть смысл повторить.
    pub fn pending(self) -> bool {
        self.0 == ERROR_PENDING || self.0 == ERROR_BUSY
    }

    pub fn not_found(self) -> bool {
        self.0 == ERROR_FILE_NOT_FOUND || self.0 == ERROR_PATH_NOT_FOUND
    }

    pub fn directory_not_empty(self) -> bool {
        self.0 == ERROR_DIR_NOT_EMPTY
    }

    /// Файловая система не знает нужной операции (старое NTFS, FAT, сетевая
    /// шара через драйвер) — пробуем запасной путь.
    pub fn unsupported(self) -> bool {
        self.0 == ERROR_NOT_SUPPORTED
            || self.0 == ERROR_INVALID_PARAMETER
            || self.0 == ERROR_INVALID_FUNCTION
    }

    pub fn offline(self) -> bool {
        self.0 == ERROR_FILE_IS_OFFLINE
    }

    /// Нужен ли для этой ошибки UAC-перезапуск.
    pub fn needs_elevation(self) -> bool {
        self.0 == ERROR_ACCESS_DENIED || self.0 == ERROR_PRIVILEGE_NOT_HELD
    }

    /// Можно ли вообще что-то сделать без вмешательства пользователя.
    pub fn retryable(self) -> bool {
        self.sharing_violation() || self.pending() || self.0 == ERROR_BUSY
    }

    /// Понятная человеку формулировка. Сначала наш собственный словарь —
    /// он одинаков в EN и RU; FormatMessageW остаётся запасным вариантом для
    /// кодов, которых в словаре нет.
    pub fn message(self) -> String {
        use crate::tr;
        let known = match self.0 {
            ERROR_ACCESS_DENIED => Some(tr!("access denied", "нет доступа")),
            ERROR_SHARING_VIOLATION => Some(tr!(
                "file is open in another program",
                "файл открыт в другой программе"
            )),
            ERROR_LOCK_VIOLATION => Some(tr!("file is locked", "файл заблокирован")),
            ERROR_DIR_NOT_EMPTY => Some(tr!("folder is not empty", "папка не пуста")),
            ERROR_FILE_NOT_FOUND => Some(tr!("file not found", "файл не найден")),
            ERROR_PATH_NOT_FOUND => Some(tr!("path not found", "путь не найден")),
            ERROR_PRIVILEGE_NOT_HELD => Some(tr!(
                "required privilege is not held",
                "не хватает привилегии"
            )),
            ERROR_USER_MAPPED_FILE => Some(tr!(
                "file is mapped into memory of another process",
                "файл отображён в память другого процесса"
            )),
            ERROR_CANNOT_DELETE => Some(tr!("cannot delete", "невозможно удалить")),
            ERROR_FILE_IS_OFFLINE => Some(tr!(
                "file is not present on the disk",
                "файл отсутствует на диске"
            )),
            ERROR_PENDING | ERROR_BUSY => Some(tr!("operation is busy", "операция занята")),
            _ => None,
        };
        match known {
            Some(text) => text.to_string(),
            None => system_message(self.0),
        }
    }
}

impl fmt::Display for NtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message(), self.0)
    }
}

impl std::error::Error for NtError {}

/// Превращает `windows::core::Result` в наш тип с уже подтянутым
/// `GetLastError` — иначе код достаётся не оттуда.
pub fn nt<T>(result: windows::core::Result<T>) -> Result<T, NtError> {
    result.map_err(|_| NtError::last())
}

fn system_message(code: u32) -> String {
    use windows::Win32::System::Diagnostics::Debug::{
        FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
    };
    let mut buffer = [0u16; 512];
    let written = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            None,
            code,
            0,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            buffer.len() as u32,
            None,
        )
    };
    if written == 0 {
        return format!("Windows error {code}");
    }
    let text = String::from_utf16_lossy(&buffer[..written as usize]);
    let text = text.trim_end_matches(['\r', '\n', ' ', '.']);
    if text.is_empty() {
        format!("Windows error {code}")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Language, with_language};

    #[test]
    fn classification_matches_the_codes_we_act_on() {
        assert!(NtError(ERROR_ACCESS_DENIED).access_denied());
        assert!(NtError(ERROR_SHARING_VIOLATION).sharing_violation());
        assert!(NtError(ERROR_LOCK_VIOLATION).retryable());
        assert!(NtError(ERROR_NOT_SUPPORTED).unsupported());
        assert!(NtError(ERROR_DIR_NOT_EMPTY).directory_not_empty());
        assert!(NtError(ERROR_ACCESS_DENIED).needs_elevation());
        assert!(!NtError(ERROR_SHARING_VIOLATION).needs_elevation());
        assert!(!NtError(ERROR_FILE_NOT_FOUND).access_denied());
    }

    #[test]
    fn messages_are_localized_and_never_empty() {
        for language in [Language::En, Language::Ru] {
            with_language(language, || {
                assert_eq!(NtError(ERROR_ACCESS_DENIED).message(), match language {
                    Language::En => "access denied",
                    Language::Ru => "нет доступа",
                });
                assert!(!NtError(999_999).message().is_empty());
            });
        }
    }
}