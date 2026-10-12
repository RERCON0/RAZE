//! Перезапуск с правами администратора.
//!
//! Не «просим права заранее»: сначала пробуем удалить как есть, и только
//! когда упёрлись в `ACCESS_DENIED` — один раз предлагаем UAC. Иначе девять
//! из десяти запусков требовали бы подтверждения без всякой причины.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::error::NtError;
use super::path::Wide;
pub use super::privs::is_elevated;

/// Запускает текущий исполняемый файл с теми же аргументами и verb `runas`.
/// `Ok(true)` — процесс запущен и этот экземпляр должен завершиться.
pub fn relaunch_elevated(exe: &Path, arguments: &OsStr) -> Result<bool, NtError> {
    let file = Wide::from_path(exe);
    let verb = Wide::new("runas");
    let params = Wide::from_path(Path::new(arguments));
    let result =
        unsafe { ShellExecuteW(None, verb.pc(), file.pc(), params.pc(), None, SW_SHOWNORMAL) };
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
    arguments: &OsStr,
    timeout: std::time::Duration,
) -> Result<u32, NtError> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
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
    let params = Wide::from_path(Path::new(arguments));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: verb.pc(),
        lpFile: file.pc(),
        lpParameters: params.pc(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.map_err(NtError::from)?;
    if info.hProcess.is_invalid() {
        return Err(NtError(6)); // ERROR_INVALID_HANDLE: no waitable child was returned.
    }
    let process = Process(info.hProcess);
    let millis = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    match unsafe { WaitForSingleObject(process.0, millis) } {
        WAIT_OBJECT_0 => {}
        result if result.0 == WAIT_TIMEOUT => return Err(NtError(WAIT_TIMEOUT)),
        _ => return Err(NtError::last()),
    }
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(process.0, &mut code) }.map_err(NtError::from)?;
    Ok(code)
}

pub fn needs_elevation() -> bool {
    !is_elevated()
}

/// Собирает командную строку из аргументов, экранируя пробелы и кавычки по
/// правилам `CommandLineToArgvW`, иначе путь с пробелами развалится.
pub fn quote(argument: &OsStr) -> OsString {
    let mut out = Vec::with_capacity(argument.len() + 2);
    quote_into(argument, &mut out);
    OsString::from_wide(&out)
}

fn quote_into(argument: &OsStr, out: &mut Vec<u16>) {
    let needs_quotes = argument.is_empty()
        || argument
            .encode_wide()
            .any(|c| c == b' ' as u16 || c == b'\t' as u16 || c == b'"' as u16);
    if !needs_quotes {
        out.extend(argument.encode_wide());
        return;
    }
    // Work on UTF-16 units, not lossy Unicode: NT filenames may contain lone
    // surrogates. Backslashes double only before quotes and the closing quote.
    out.push(b'"' as u16);
    let mut backslashes = 0usize;
    for unit in argument.encode_wide() {
        if unit == b'\\' as u16 {
            backslashes += 1;
        } else if unit == b'"' as u16 {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            out.push(unit);
            backslashes = 0;
        } else {
            out.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            out.push(unit);
            backslashes = 0;
        }
    }
    out.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    out.push(b'"' as u16);
}

pub fn build_arguments(args: &[OsString]) -> OsString {
    let mut out = Vec::with_capacity(args.iter().map(|a| a.len()).sum::<usize>() + args.len());
    for (index, argument) in args.iter().enumerate() {
        if index != 0 {
            out.push(b' ' as u16);
        }
        quote_into(argument, &mut out);
    }
    OsString::from_wide(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_follows_windows_command_line_rules() {
        assert_eq!(quote(OsStr::new("simple")), "simple");
        assert_eq!(quote(OsStr::new("with space")), "\"with space\"");
        assert_eq!(quote(OsStr::new("")), "\"\"");
        assert_eq!(quote(OsStr::new("a\"b")), "\"a\\\"b\"");
    }

    #[test]
    fn backslashes_before_a_quote_follow_the_argv_rules() {
        // Хвостовая черта в пути с пробелом удваивается: иначе закрывающая
        // кавычка оказалась бы экранированной.
        assert_eq!(
            quote(OsStr::new("C:\\my folder\\")),
            "\"C:\\my folder\\\\\""
        );
        assert_eq!(quote(OsStr::new("a\\\"b")), "\"a\\\\\\\"b\"");
        // Без пробелов и кавычек строка остаётся как есть, черты тоже.
        assert_eq!(quote(OsStr::new("C:\\a\\")), "C:\\a\\");
    }

    #[test]
    fn arguments_are_joined_with_quotes() {
        let joined = build_arguments(&["--dry-run".into(), "C:\\Program Files\\x".into()]);
        assert_eq!(joined, "--dry-run \"C:\\Program Files\\x\"");
    }

    #[test]
    fn elevation_arguments_preserve_lone_surrogates_through_the_windows_parser() {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::UI::Shell::CommandLineToArgvW;
        let argument = OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
            b' ' as u16,
            b'\\' as u16,
        ]);
        let line = build_arguments(&[
            "raze.exe".into(),
            "--".into(),
            argument.clone(),
            "next".into(),
        ]);
        let wide = Wide::from_path(Path::new(&line));
        let mut count = 0i32;
        let argv = unsafe { CommandLineToArgvW(wide.pc(), &mut count) };
        assert!(!argv.is_null());
        let parsed = unsafe {
            std::slice::from_raw_parts(argv, count as usize)
                .iter()
                .map(|arg| OsString::from_wide(arg.as_wide()))
                .collect::<Vec<_>>()
        };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(argv.cast())));
        }
        assert_eq!(
            parsed,
            vec![
                OsString::from("raze.exe"),
                OsString::from("--"),
                argument,
                OsString::from("next")
            ]
        );
    }
}
