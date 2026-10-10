//! COM-расширка контекстного меню RAZE для современного меню Windows 11.
//!
//! ВАЖНО: этот DLL загружается в процесс Проводника (`explorer.exe`).
//! Любой panic, ушедший через границу ABI, перезапускает Проводник, поэтому
//! здесь нет ни `unwrap`, ни `expect`, ни паник в общих путях. Крейт
//! собирается с `panic = "abort"` и зависит только от `windows` — тянуть сюда
//! egui и остальной RAZE нельзя.
//!
//! ## Как это работает
//!
//! Пакет `RAZE.ShellExtension` (sparse, ставится через `Add-AppxPackage`)
//! говорит Проводнику: «для этого CLSID есть DLL, зови `DllGetClassObject`».
//! Проводник создаёт объект, спрашивает `IExplorerCommand` и на каждый пункт
//! меню зовёт `GetTitle` / `GetState`, а по клику — `Invoke`.
//!
//! Объект у нас пустой (ZST): всё состояние приходит параметрами
//! (`IShellItemArray` — это и есть выделение пользователя), поэтому один и тот
//! же объект переиспользуется для всех пунктов меню без синхронизации.
//!
//! `Invoke` **не удаляет ничего сам**. Он только запускает `raze.exe` с
//! выбранными путями и возвращает управление: удаление занимает секунды,
//! а Проводник не должен висеть на нашем модальном окне.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::core::{implement, BOOL, GUID, HRESULT, Interface, PCWSTR, PWSTR, Ref};
use windows::Win32::Foundation::{
    CloseHandle, CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOTIMPL, E_NOINTERFACE,
    E_OUTOFMEMORY, HMODULE,
};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::System::Com::{
    CoTaskMemAlloc, CoTaskMemFree, IBindCtx, IClassFactory, IClassFactory_Impl,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::Ole::{IObjectWithSite, IObjectWithSite_Impl};
use windows::Win32::System::Threading::{
    CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::Shell::{
    IEnumExplorerCommand, IExplorerCommand, IExplorerCommand_Impl, IShellItemArray,
    ECF_DEFAULT, ECS_ENABLED, SIGDN_FILESYSPATH,
};

/// Тот же CLSID, что в `src/shell/install.rs` и в AppxManifest.xml.
/// Менять надо сразу в трёх местах, иначе Проводник не найдёт DLL
/// для зарегистрированного CLSID и команда молча исчезнет из меню.
const COMMAND_CLSID: GUID = GUID::from_values(
    0x8E5B_2C41,
    0x6D3A,
    0x4F17,
    [0x9C, 0x0B, 0x51, 0xD2, 0xA7, 0xE4, 0xF1, 0x08],
);

/// Каноническое имя команды (`GetCanonicalName`). Проводник помнит его
/// между сессиями, поэтому оно задано константой, а не генерируется.
const COMMAND_GUID: GUID = GUID::from_values(
    0x8E5B_2C41,
    0x6D3A,
    0x4F17,
    [0x2C, 0x77, 0x9E, 0x4B, 0x6D, 0x1A, 0xC0, 0x31],
);

/// Подпись в меню: что именно делает пункт. Язык берётся из языка
/// интерфейса Windows (`LANG_RUSSIAN` — основной язык 0x19), как и у
/// остальных пунктов Проводника.
fn title() -> &'static str {
    const PRIMARY_LANGUAGE_MASK: u16 = 0x3ff;
    const LANG_RUSSIAN: u16 = 0x19;
    if unsafe { GetUserDefaultUILanguage() } & PRIMARY_LANGUAGE_MASK == LANG_RUSSIAN {
        "Удалить с помощью RAZE"
    } else {
        "Delete with RAZE"
    }
}

/// Всплывающая подсказка — почему вообще «RAZE», а не «Удалить».
const TOOLTIP: &str = "Remove with RAZE";

/// Исполняемый файл рядом с этой DLL.
const RAZE_EXE: &str = "raze.exe";

/// Идентификатор иконки в `.rc` (`1 ICON "..."`). В ссылке на иконку
/// ресурс пишется со знаком минус: `raze.exe,-1`.
const ICON_RESOURCE_ID: i32 = -1;

/// Сколько COM-объектов сейчас живёт. Пока счётчик не ноль,
/// `DllCanUnloadNow` обязан вернуть S_FALSE: vtable живёт в коде DLL,
/// и выгрузка при живых ссылках — это падение Проводника без
/// предупреждения. `#[implement]` сам освобождает бокс при нулевом
/// счётчике ссылок, а `Drop` на теле объекта — единственное место,
/// где мы об этом узнаём.
static LIVE_OBJECTS: AtomicUsize = AtomicUsize::new(0);

/// Команда меню «Удалить с помощью RAZE».
#[implement(IExplorerCommand, IObjectWithSite)]
struct RazeCommand;

impl Drop for RazeCommand {
    fn drop(&mut self) {
        LIVE_OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IExplorerCommand_Impl for RazeCommand_Impl {
    fn GetTitle(&self, _items: Ref<IShellItemArray>) -> windows::core::Result<PWSTR> {
        task_string(title())
    }

    fn GetIcon(&self, _items: Ref<IShellItemArray>) -> windows::core::Result<PWSTR> {
        match icon_reference() {
            Some(reference) => task_string(&reference),
            // Иконка — украшение, а не условие работы команды:
            // без неё пункт меню всё равно остаётся рабочим.
            None => Err(E_NOTIMPL.into()),
        }
    }

    fn GetToolTip(&self, _items: Ref<IShellItemArray>) -> windows::core::Result<PWSTR> {
        task_string(TOOLTIP)
    }

    fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
        Ok(COMMAND_GUID)
    }

    fn GetState(
        &self,
        _items: Ref<IShellItemArray>,
        _may_be_slow: BOOL,
    ) -> windows::core::Result<u32> {
        // Проверку «можно ли это удалять» делает raze.exe. Здесь она
        // означала бы обход дерева прямо в процессе Проводника —
        // то есть замороженное меню на каждом клике.
        Ok(ECS_ENABLED.0 as u32)
    }

    fn Invoke(
        &self,
        items: Ref<IShellItemArray>,
        _bind: Ref<IBindCtx>,
    ) -> windows::core::Result<()> {
        let Some(items) = items.as_ref() else {
            return Ok(());
        };
        let paths = match selected_paths(items) {
            Ok(paths) => paths,
            // Ошибку не показываем: всплывашка поверх меню выглядит
            // как падение Проводника, а raze.exe сам скажет, что не так.
            Err(_) => return Ok(()),
        };
        if paths.is_empty() {
            return Ok(());
        }
        spawn_raze(&paths);
        Ok(())
    }

    fn GetFlags(&self) -> windows::core::Result<u32> {
        Ok(ECF_DEFAULT.0 as u32)
    }

    fn EnumSubCommands(&self) -> windows::core::Result<IEnumExplorerCommand> {
        Err(E_NOTIMPL.into())
    }
}

/// Сайт нам не нужен: выделение приходит в `Invoke` отдельным
/// параметром. Но интерфейс Проводник запрашивает всегда, поэтому
/// отвечаем корректно, а не ошибкой.
impl IObjectWithSite_Impl for RazeCommand_Impl {
    fn SetSite(&self, _site: Ref<windows::core::IUnknown>) -> windows::core::Result<()> {
        Ok(())
    }

    fn GetSite(
        &self,
        _riid: *const GUID,
        _site: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[implement(IClassFactory)]
struct RazeFactory;

impl Drop for RazeFactory {
    fn drop(&mut self) {
        LIVE_OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
}

impl IClassFactory_Impl for RazeFactory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<windows::core::IUnknown>,
        riid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        unsafe {
            *object = ptr::null_mut();
            // Агрегация нам не нужна: команда не должна становиться
            // частью чужого объекта.
            if !outer.is_null() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }
            if riid.is_null() {
                return Err(E_NOINTERFACE.into());
            }
            let wanted = *riid;
            if wanted != windows::core::IUnknown::IID
                && wanted != IExplorerCommand::IID
                && wanted != IObjectWithSite::IID
            {
                return Err(E_NOINTERFACE.into());
            }

            LIVE_OBJECTS.fetch_add(1, Ordering::SeqCst);
            let command: windows::core::IUnknown = RazeCommand.into();
            // `query` сам разыщет нужный интерфейс и добавит ссылку;
            // наш лишний экземпляр `IUnknown` освободится при выходе.
            let status = command.query(&wanted, object);
            if status.is_err() {
                *object = ptr::null_mut();
                return Err(status.into());
            }
            Ok(())
        }
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Точка входа `DllGetClassObject`. Проводник вызывает её с CLSID из
/// манифеста пакета и IID `IClassFactory`.
#[no_mangle]
pub extern "system" fn DllGetClassObject(
    class: *const GUID,
    interface: *const GUID,
    object: *mut *mut c_void,
) -> HRESULT {
    unsafe {
        *object = ptr::null_mut();
        if class.is_null() || interface.is_null() {
            return E_NOINTERFACE;
        }
        if *class != COMMAND_CLSID {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        LIVE_OBJECTS.fetch_add(1, Ordering::SeqCst);
        let factory: windows::core::IUnknown = RazeFactory.into();
        let status = factory.query(interface, object);
        if status.is_err() {
            *object = ptr::null_mut();
        }
        status
    }
}

/// Пока жив хоть один объект, DLL выгружать нельзя — иначе Проводник
/// вызовет освобождённый код. `S_FALSE` означает «пока нет», и нас
/// спросят ещё раз.
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if LIVE_OBJECTS.load(Ordering::SeqCst) == 0 {
        HRESULT(0)
    } else {
        HRESULT(1)
    }
}

/// Строка для Проводника. COM отдаёт такие указатели через
/// `CoTaskMemAlloc`, и вызывающая сторона освобождает память сама —
/// поэтому выделять ровно так, а не через `String`.
fn task_string(text: &str) -> windows::core::Result<PWSTR> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let bytes = (units.len() + 1) * std::mem::size_of::<u16>();
    let buffer = unsafe { CoTaskMemAlloc(bytes) } as *mut u16;
    if buffer.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    unsafe {
        ptr::copy_nonoverlapping(units.as_ptr(), buffer, units.len());
        *buffer.add(units.len()) = 0;
    }
    Ok(PWSTR(buffer))
}

/// Пути выбранного — ровно то, что человек отметил в Проводнике.
/// Всё остальное (существует ли объект, что он из себя представляет,
/// можно ли его удалить) проверяет raze.exe.
fn selected_paths(items: &IShellItemArray) -> windows::core::Result<Vec<PathBuf>> {
    let count = unsafe { items.GetCount()? };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let item = match unsafe { items.GetItemAt(index) } {
            Ok(item) => item,
            Err(_) => continue,
        };
        let name = match unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) } {
            Ok(name) => name,
            // Не файловая система (элемент панели, виртуальный узел) —
            // RAZE такое удалять не умеет.
            Err(_) => continue,
        };
        let text = unsafe { wide_slice(&name) };
        let path = PathBuf::from(String::from_utf16_lossy(text));
        unsafe { CoTaskMemFree(Some(name.0.cast::<c_void>())) };
        if path.as_os_str().is_empty() {
            continue;
        }
        paths.push(path);
    }
    Ok(paths)
}

/// Содержимое `PWSTR` до первого NUL. Память принадлежит вызывающему
/// коду: читать её можно только до `CoTaskMemFree`.
unsafe fn wide_slice(text: &PWSTR) -> &[u16] {
    if text.0.is_null() {
        return &[];
    }
    let length = (0..)
        .find(|index| unsafe { *text.0.add(*index) } == 0)
        .unwrap_or(0);
    unsafe { std::slice::from_raw_parts(text.0, length) }
}

/// Ссылка на иконку для Проводника: `путь,-1` (минус — идентификатор
/// ресурса, а не индекс). Берём иконку самого `raze.exe`: она есть
/// всегда, даже если PE-ресурс DLL собран не так, как ожидалось.
fn icon_reference() -> Option<String> {
    let exe = module_path()?.with_file_name(RAZE_EXE);
    Some(format!("{},{}", exe.display(), ICON_RESOURCE_ID))
}

/// Путь к этой DLL — оттуда берётся каталог программы.
fn module_path() -> Option<PathBuf> {
    const MAX_PATH_CHARS: usize = 32768;
    // `GetModuleFileNameW(None, ..)` назвал бы процесс-хозяин (explorer.exe
    // или dllhost.exe), а не эту DLL, и `raze.exe` искался бы не там. Поэтому
    // модуль находится по адресу собственной функции.
    let mut module = HMODULE::default();
    let anchor = module_path as usize as *const u16;
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(anchor),
            &mut module,
        )
    }
    .ok()?;
    let mut buffer = vec![0u16; MAX_PATH_CHARS];
    let length = unsafe { GetModuleFileNameW(Some(module), &mut buffer) };
    if length == 0 || length as usize >= MAX_PATH_CHARS {
        return None;
    }
    buffer.truncate(length as usize);
    let path = PathBuf::from(String::from_utf16_lossy(&buffer));
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    }
}

/// Запуск raze.exe. Вызывается прямо из обработчика меню внутри
/// `explorer.exe`, поэтому процесс отсоединяется, а ручки закрываются
/// на месте: забытая пара ручек — это утечка на каждый клик по меню.
fn spawn_raze(paths: &[PathBuf]) {
    let Some(exe) = module_path().map(|dll| dll.with_file_name(RAZE_EXE)) else {
        return;
    };

    let mut command: Vec<u16> = Vec::new();
    push_quoted(&mut command, &exe);
    for unit in " --from-shell".encode_utf16() {
        command.push(unit);
    }
    for path in paths {
        for unit in " --mode ".encode_utf16() {
            command.push(unit);
        }
        push_quoted(&mut command, path);
    }
    command.push(0);

    let mut startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();

    let created = unsafe {
        CreateProcessW(
            None,
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            windows::Win32::System::Threading::DETACHED_PROCESS,
            None,
            None,
            &mut startup,
            &mut process,
        )
    };
    if created.is_err() {
        return;
    }
    let _ = unsafe { CloseHandle(process.hThread) };
    let _ = unsafe { CloseHandle(process.hProcess) };
}

/// Правила кавычек `CreateProcessW`. Обратная черта перед кавычкой и
/// хвост из обратных черт перед закрывающей кавычкой удваиваются, сама
/// кавычка экранируется. Без этого путь с кавычкой в имени (NT такое
/// имя разрешает) разорвал бы командную строку.
fn push_quoted(out: &mut Vec<u16>, argument: &Path) {
    out.push(b'"' as u16);
    let mut backslashes = 0usize;
    for unit in argument.as_os_str().encode_wide() {
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        if unit == b'"' as u16 {
            // k черт перед кавычкой становятся 2k+1: кавычка
            // экранируется, черты удваиваются.
            out.resize(out.len() + backslashes + 1, b'\\' as u16);
        } else {
            out.resize(out.len() + backslashes, b'\\' as u16);
        }
        out.push(unit);
        backslashes = 0;
    }
    // Хвостовые черты ещё не выведены, а перед закрывающей кавычкой
    // их должно быть вдвое больше: `"C:\tmp\` → `"C:\tmp\\"`.
    out.resize(out.len() + backslashes * 2, b'\\' as u16);
    out.push(b'"' as u16);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_matches_the_registry_value() {
        // Тот же GUID, что в `src/shell/install.rs` и AppxManifest.xml.
        assert_eq!(COMMAND_CLSID.data1, 0x8E5B_2C41);
        assert_eq!(COMMAND_CLSID.data2, 0x6D3A);
        assert_eq!(COMMAND_CLSID.data3, 0x4F17);
        assert_eq!(
            COMMAND_CLSID.data4,
            [0x9C, 0x0B, 0x51, 0xD2, 0xA7, 0xE4, 0xF1, 0x08]
        );
        assert_ne!(COMMAND_CLSID, COMMAND_GUID, "каноническое имя — другой GUID");
    }

    #[test]
    fn quoted_argument_survives_spaces() {
        let mut command = Vec::new();
        push_quoted(&mut command, Path::new(r"C:\tmp\a b.txt"));
        assert_eq!(String::from_utf16_lossy(&command), r#""C:\tmp\a b.txt""#);
    }

    #[test]
    fn quoted_argument_escapes_inner_quotes() {
        let mut command = Vec::new();
        push_quoted(&mut command, Path::new(r#"C:\tmp\we"ird.txt"#));
        assert_eq!(
            String::from_utf16_lossy(&command),
            r##""C:\tmp\we\"ird.txt""##
        );
    }

    #[test]
    fn trailing_backslashes_are_doubled() {
        let mut command = Vec::new();
        push_quoted(&mut command, Path::new(r"C:\tmp\"));
        assert_eq!(String::from_utf16_lossy(&command), r#""C:\tmp\\""#);

        let mut command = Vec::new();
        push_quoted(&mut command, Path::new(r"C:\we\"));
        assert_eq!(String::from_utf16_lossy(&command), r#""C:\we\\""#);
    }

    #[test]
    fn module_path_is_not_empty() {
        let path = module_path().expect("путь к текущему модулю");
        assert!(!path.as_os_str().is_empty());
    }

    #[test]
    fn wide_slice_stops_at_the_terminator() {
        let mut units: Vec<u16> = "NUL".encode_utf16().collect();
        units.push(0);
        assert_eq!(
            unsafe { wide_slice(&PWSTR(units.as_mut_ptr())) },
            &[b'N' as u16, b'U' as u16, b'L' as u16]
        );
        assert!(unsafe { wide_slice(&PWSTR(ptr::null_mut())) }.is_empty());
    }

    #[test]
    fn task_string_ends_with_nul_and_is_readable() {
        let pointer = task_string("NUL").expect("allocation");
        // Читать можно только до освобождения — копия строки снимается
        // здесь, а `CoTaskMemFree` вызывается уже после неё.
        let text = String::from_utf16_lossy(unsafe { wide_slice(&pointer) });
        unsafe { CoTaskMemFree(Some(pointer.0.cast::<c_void>())) };
        assert_eq!(text, "NUL");
    }
}