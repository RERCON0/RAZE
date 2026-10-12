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
pub const ERROR_PENDING: u32 = windows::Win32::Foundation::ERROR_IO_PENDING.0;
pub const ERROR_DELETE_PENDING: u32 = windows::Win32::Foundation::ERROR_DELETE_PENDING.0;
pub const ERROR_BUSY: u32 = 170;
pub const ERROR_USER_MAPPED_FILE: u32 = 1224;
pub const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;
pub const ERROR_FILE_IS_OFFLINE: u32 = 4350;
/// Application-defined code (bit 29): the object's location is outside the
/// allowed deletion tree, protected, or cannot be verified safely.
pub const ERROR_OUTSIDE_TREE: u32 = 0xE0A5_0001;

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
        self.0 == ERROR_PENDING || self.0 == ERROR_DELETE_PENDING || self.0 == ERROR_BUSY
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
            ERROR_FILE_IS_OFFLINE => Some(tr!(
                "file is not present on the disk",
                "файл отсутствует на диске"
            )),
            ERROR_PENDING | ERROR_BUSY => Some(tr!("operation is busy", "операция занята")),
            ERROR_DELETE_PENDING => Some(tr!("deletion is still pending", "удаление ещё не завершено")),
            ERROR_OUTSIDE_TREE => Some(tr!(
                "object is outside the allowed deletion tree or its safe location cannot be verified",
                "объект вне разрешённого дерева удаления или его безопасное расположение нельзя проверить"
            )),
            _ => None,
        };
        match known {
            Some(text) => text.to_string(),
            None => system_message(self.0),
        }
    }
}

impl From<windows::core::Error> for NtError {
    fn from(error: windows::core::Error) -> Self {
        let code = error.code().0 as u32;
        // HRESULT_FROM_WIN32 retains the original Win32 code in its low word.
        // COM-specific HRESULTs must remain intact for FormatMessageW.
        if code & 0xffff_0000 == 0x8007_0000 {
            Self(code & 0xffff)
        } else {
            Self(code)
        }
    }
}

impl fmt::Display for NtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message(), self.0)
    }
}

impl std::error::Error for NtError {}

/// Uses the error captured by the Windows binding, not a later thread-local
/// `GetLastError` value (COM and DWORD-returning APIs do not set it).
pub fn nt<T>(result: windows::core::Result<T>) -> Result<T, NtError> {
    result.map_err(NtError::from)
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
        assert!(NtError(windows::Win32::Foundation::ERROR_DELETE_PENDING.0).pending());
        assert!(NtError(windows::Win32::Foundation::ERROR_IO_PENDING.0).retryable());
        assert!(!NtError(windows::Win32::Foundation::ERROR_BAD_PATHNAME.0).retryable());
    }

    #[test]
    fn captured_errors_do_not_depend_on_last_error() {
        unsafe { windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0)) };
        let error = windows::core::Error::from_hresult(windows::core::HRESULT(0x80070005u32 as i32));
        assert_eq!(nt::<()>(Err(error)), Err(NtError(ERROR_ACCESS_DENIED)));
        let error = windows::core::Error::from_hresult(windows::core::HRESULT(0x80004005u32 as i32));
        assert_eq!(NtError::from(error).code(), 0x80004005);
    }

}