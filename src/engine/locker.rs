//! Кто держит файл: Restart Manager плюс вежливое закрытие окон.
//!
//! Restart Manager — тот же механизм, которым пользуется сам Проводник,
//! когда показывает «Папка занята, закройте программу». Он даёт PID и имя
//! процесса, а не просто «доступ запрещён».

use std::cell::Cell;
use std::time::Duration;

use serde::Serialize;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::RestartManager::{
    RM_PROCESS_INFO, RmEndSession, RmGetList, RmRegisterResources,
    RmStartSession,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};

use super::path::{NtPath, Wide};

/// Процесс, у которого открыт дескриптор файла.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
    /// Служба Windows держит файл — её не закрываем, но показываем.
    pub is_service: bool,
}

const ERROR_MORE_DATA: u32 = 234;
const CCH_RM_MAX_KEY: usize = 33;

/// Процессы, мешающие удалить перечисленные пути.
pub fn processes_using(paths: &[std::path::PathBuf]) -> Vec<Holder> {
    if paths.is_empty() {
        return Vec::new();
    }
    let wide: Vec<Wide> = paths.iter().map(|p| Wide::from_path(p)).collect();
    let ptrs: Vec<windows::core::PCWSTR> = wide.iter().map(|p| p.pc()).collect();

    let mut key = vec![0u16; CCH_RM_MAX_KEY];
    let mut session: u32 = 0;
    // SAFETY: Restart Manager возвращает код, а не Result; все выходные
    // буферы живут до конца функции.
    unsafe {
        if RmStartSession(&mut session, None, windows::core::PWSTR(key.as_mut_ptr())).0 != 0 {
            return Vec::new();
        }
        let registered = RmRegisterResources(session, Some(&ptrs), None, None);
        let mut holders = Vec::new();
        if registered.0 == 0 {
            let mut needed = 0u32;
            let mut count = 0u32;
            let mut reasons = 0u32;
            // Первый вызов всегда «нужно больше данных» — так и узнаём размер.
            let _ = RmGetList(session, &mut needed, &mut count, None, &mut reasons);
            if needed > 0 {
                let mut buffer: Vec<RM_PROCESS_INFO> =
                    vec![RM_PROCESS_INFO::default(); needed as usize];
                if RmGetList(
                    session,
                    &mut needed,
                    &mut count,
                    Some(buffer.as_mut_ptr()),
                    &mut reasons,
                )
                .0
                    != ERROR_MORE_DATA
                    || count == 0
                {
                    holders = buffer
                        .iter()
                        .take(count as usize)
                        .map(|info| Holder {
                            pid: info.Process.dwProcessId,
                            name: wide_string(&info.strAppName),
                            is_service: info.ApplicationType.0 != 1,
                        })
                        .collect();
                }
            }
        }
        let _ = RmEndSession(session);
        holders
    }
}

/// Процесс по конкретному пути — используется при отказе `SHARING_VIOLATION`,
/// чтобы сказать пользователю, кто именно мешает.
pub fn holders_of(path: &NtPath) -> Vec<Holder> {
    processes_using(&[path.to_path()])
}

// Вежливо закрывает приложение: `WM_CLOSE` во все его верхние окна.
// Никаких `taskkill /f` — если программа спросит «сохранить изменения»,
// ответ остаётся за человеком.
thread_local! {
    /// Состояние обхода окон. `EnumWindows` синхронен и зовёт колбэк на том же
    /// потоке, поэтому thread-local безопаснее, чем передача указателя через
    /// `LPARAM` (там 8 байт, а структура больше).
    static CLOSE_STATE: Cell<(u32, usize)> = const { Cell::new((0, 0)) };
}

pub fn request_close(pid: u32) -> usize {
    CLOSE_STATE.with(|state| state.set((pid, 0)));

    unsafe extern "system" fn visit(hwnd: HWND, _param: LPARAM) -> windows::core::BOOL {
        let (target, sent) = CLOSE_STATE.with(Cell::get);
        let mut process = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
        if process == target {
            // Некоторые окна игнорируют WM_CLOSE. Счётчик покажет, сколько
            // сообщений действительно дошло.
            if unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok() {
                CLOSE_STATE.with(|state| state.set((target, sent + 1)));
            }
        }
        windows::core::BOOL(1)
    }

    // SAFETY: колбэк не выходит за пределы HWND и своего thread-local.
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(0));
    }
    CLOSE_STATE.with(|state| state.get().1)
}

/// Ждёт, пока держатель освободит файл. Короткая пауза с понятной паузой:
/// пользователь в этот момент как раз закрывает программу.
pub fn wait_for_release(paths: &[std::path::PathBuf], total: Duration) -> bool {
    let step = Duration::from_millis(150);
    let mut waited = Duration::ZERO;
    while waited < total {
        if processes_using(paths).is_empty() {
            return true;
        }
        std::thread::sleep(step);
        waited += step;
    }
    processes_using(paths).is_empty()
}

fn wide_string(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_paths_means_no_holders() {
        assert!(processes_using(&[]).is_empty());
    }

    #[test]
    fn current_directory_is_not_held_by_anything_we_can_close() {
        // Проверяем, что вызов не падает на обычном пути: RM просто ничего
        // не находит для незанятого файла.
        let temp = std::env::temp_dir();
        assert!(holders_of(&NtPath::new(&temp).unwrap()).is_empty());
    }

    #[test]
    fn closing_our_own_pid_is_a_no_op_for_counting() {
        // У консольного процесса нет верхних окон — счётчик остаётся нулевым,
        // зато мы проверяем, что EnumWindows не падает.
        assert_eq!(request_close(u32::MAX), 0);
    }
}