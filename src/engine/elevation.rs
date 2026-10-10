//! Перезапуск с правами администратора.
//!
//! Не «просим права заранее»: сначала пробуем удалить как есть, и только
//! когда упёрлись в `ACCESS_DENIED` — один раз предлагаем UAC. Иначе девять
//! из десяти запусков требовали бы подтверждения без всякой причины.

use std::path::Path;

use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::error::NtError;
use super::path::Wide;
pub use super::privs::is_elevated;

/// Запускает текущий исполняемый файл с теми же аргументами и verb `runas`.
/// `Ok(true)` — процесс запущен и этот экземпляр должен завершиться.
pub fn relaunch_elevated(exe: &Path, arguments: &str) -> Result<bool, NtError> {
    let file = Wide::from_path(exe);
    let verb = Wide::new("runas");
    let params = Wide::new(arguments);
    let result = unsafe {
        ShellExecuteW(
            None,
            verb.pc(),
            file.pc(),
            params.pc(),
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute возвращает HINSTANCE; значения меньше 32 — код ошибки,
    // среди которых 5 (отказ пользователя) и 740 (нужны права).
    let code = result.0 as isize;
    Ok(code > 32)
}

/// Запускает `exe` с правами администратора, ждёт его завершения и отдаёт код
/// выхода. Окно скрыто: что произошло, процесс сообщает сам (файлом отчёта).
/// `Err` — запуск не удался; отказ в UAC даёт код 1223.
pub fn run_elevated_and_wait(
    exe: &Path,
    arguments: &str,
    timeout: std::time::Duration,
) -> Result<u32, NtError> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    struct Process(HANDLE);
    impl Drop for Process {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    const WAIT_TIMEOUT: u32 = 258;
    let file = Wide::from_path(exe);
    let verb = Wide::new("runas");
    let params = Wide::new(arguments);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: verb.pc(),
        lpFile: file.pc(),
        lpParameters: params.pc(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.map_err(|_| NtError::last())?;
    if info.hProcess.is_invalid() {
        return Err(NtError::last());
    }
    let process = Process(info.hProcess);
    let millis = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    if unsafe { WaitForSingleObject(process.0, millis) } != WAIT_OBJECT_0 {
        return Err(NtError(WAIT_TIMEOUT));
    }
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(process.0, &mut code) }.map_err(|_| NtError::last())?;
    Ok(code)
}

pub fn needs_elevation() -> bool {
    !is_elevated()
}

/// Собирает командную строку из аргументов, экранируя пробелы и кавычки по
/// правилам `CommandLineToArgvW`, иначе путь с пробелами развалится.
pub fn quote(argument: &str) -> String {
    let needs_quotes = argument.is_empty() || argument.chars().any(|c| c.is_whitespace() || c == '"');
    if !needs_quotes {
        return argument.to_string();
    }
    // Правило CommandLineToArgvW: обратные черты удваиваются только перед
    // кавычкой и перед закрывающей кавычкой. Без этого хвостовая черта пути
    // с пробелом экранирует закрывающую кавычку, и аргумент съедает соседний.
    let mut out = String::with_capacity(argument.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in argument.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

pub fn build_arguments(args: &[String]) -> String {
    args.iter()
        .map(|a| quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_follows_windows_command_line_rules() {
        assert_eq!(quote("simple"), "simple");
        assert_eq!(quote("with space"), "\"with space\"");
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn backslashes_before_a_quote_follow_the_argv_rules() {
        // Хвостовая черта в пути с пробелом удваивается: иначе закрывающая
        // кавычка оказалась бы экранированной.
        assert_eq!(quote("C:\\my folder\\"), "\"C:\\my folder\\\\\"");
        assert_eq!(quote("a\\\"b"), "\"a\\\\\\\"b\"");
        // Без пробелов и кавычек строка остаётся как есть, черты тоже.
        assert_eq!(quote("C:\\a\\"), "C:\\a\\");
    }

    #[test]
    fn arguments_are_joined_with_quotes() {
        let joined = build_arguments(&["--dry-run".into(), "C:\\Program Files\\x".into()]);
        assert_eq!(joined, "--dry-run \"C:\\Program Files\\x\"");
    }
}