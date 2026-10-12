//! Удаление при следующей перезагрузке.
//!
//! Windows исполняет `PendingFileRenameOperations` сеансного менеджера до
//! запуска служб и загрузки профилей — до того, как файлы успеет открыть
//! хоть кто-нибудь. RAZE добавляет туда пары «удалить» в порядке «потомки
//! раньше предков»: к моменту удаления папки она уже пуста.
//!
//! Запись значения требует прав администратора; без них ключ не открывается,
//! процесс получает `ACCESS_DENIED`, и обычный UAC-перезапуск CLI/GUI делает
//! всё в повышенной копии. Каждый каталог перед обходом проверяется по
//! дескриптору на принадлежность цели: подменённый junction'ом каталог не
//! уводит планирование в чужое дерево. Само исполнение отложено до загрузки,
//! поэтому окно между проверкой и записью в реестр — один проход, а запись
//! подтверждается повторным чтением.

use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_MULTI_SZ};
use winreg::{RegKey, RegValue};

use super::dir;
use super::error::{
    NtError, ERROR_FILE_TOO_LARGE, ERROR_INVALID_DATA, ERROR_INVALID_PARAMETER, ERROR_OUTSIDE_TREE,
};
use super::handle::{open_for_read, Containment};
use super::path::NtPath;
use super::privs;
use super::remove::{classify_root, kind_of, Note, Outcome, RemoveOptions};
use super::scan::EntryKind;
use super::security;

const SESSION_MANAGER: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager";
const VALUE_NAME: &str = "PendingFileRenameOperations";
/// Значение реестра ограничено; больше — честный отказ целиком, а не половина
/// плана: частично назначенное дерево удалялось бы при загрузке непредсказуемо.
const MAX_VALUE_BYTES: usize = 512 * 1024;
const ERROR_WRITE_FAULT: u32 = 29;
const MAX_DEPTH: usize = 512;

/// Планирует удаление дерева при следующей перезагрузке.
///
/// Весь прогон — один проход по дереву и одна запись в реестр: либо
/// запланировано всё, что удалось обойти, либо (при нехватке прав) не
/// запланировано ничего, а отчёт говорит, что нужен UAC-перезапуск.
pub fn schedule_tree(root: &NtPath, options: &RemoveOptions) -> Outcome {
    let key = if options.dry_run {
        None
    } else {
        match RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(SESSION_MANAGER, KEY_READ | KEY_WRITE)
        {
            Ok(key) => Some(key),
            Err(error) => {
                let mut outcome = Outcome::default();
                outcome.fail(root, registry_error(error), Vec::new());
                return outcome;
            }
        }
    };
    schedule_with(root, options, key.as_ref())
}

fn schedule_with(root: &NtPath, options: &RemoveOptions, key: Option<&RegKey>) -> Outcome {
    let mut outcome = Outcome {
        dry_run: options.dry_run,
        ..Default::default()
    };
    if !options.dry_run {
        privs::enable_backup_restore();
    }
    // Пробный прогон ничего не меняет, и защищать в нём нечего.
    let guard = if options.dry_run {
        None
    } else {
        match Containment::new(root) {
            Ok(guard) => Some(guard),
            Err(error) if error.not_found() => {
                outcome.notes.push(Note::Vanished {
                    path: root.to_string_lossy(),
                });
                return outcome;
            }
            Err(error) => {
                outcome.fail(root, error, Vec::new());
                return outcome;
            }
        }
    };
    let guard = guard.as_ref();

    let planned = collect(root, options, guard, &mut outcome);
    if options.dry_run {
        count_planned(&mut outcome, &planned);
        return outcome;
    }
    let Some(key) = key else {
        outcome.fail(root, NtError(ERROR_INVALID_PARAMETER), Vec::new());
        return outcome;
    };
    if planned.is_empty() {
        // Обходить было нечего (например, корень не перечислился): ошибки уже
        // в отчёте, а пустую запись в реестр добавлять не за чем.
        return outcome;
    }
    let mut sources = Vec::with_capacity(planned.len());
    for item in &planned {
        match nt_source(&item.path) {
            Ok(source) => sources.push(source),
            Err(error) => {
                outcome.fail(root, error, Vec::new());
                return outcome;
            }
        }
    }
    match append_pending(key, &sources) {
        Ok(()) => count_planned(&mut outcome, &planned),
        Err(error) => outcome.fail(root, error, Vec::new()),
    }
    outcome
}

/// Объект в плане отложенного удаления. `count` — учитывать ли его в
/// счётчиках отчёта: корень-папка не считается (пользователь и так видит
/// выбранную цель), корень-файл считается, как при обычном удалении.
struct Planned {
    path: NtPath,
    kind: EntryKind,
    size: u64,
    count: bool,
}

fn count_planned(outcome: &mut Outcome, planned: &[Planned]) {
    for item in planned {
        if !item.count {
            continue;
        }
        match item.kind {
            EntryKind::File => {
                outcome.removed_files += 1;
                // Байты освободятся при перезагрузке; это размер, а не
                // измерение занятого места.
                outcome.freed_bytes += item.size;
            }
            EntryKind::Directory => outcome.removed_directories += 1,
            EntryKind::Link => outcome.removed_links += 1,
        }
    }
}

/// Собирает дерево в порядке удаления: сначала файлы и ссылки, затем папки —
/// каждая после своего содержимого, корень последним.
fn collect(
    root: &NtPath,
    options: &RemoveOptions,
    guard: Option<&Containment>,
    outcome: &mut Outcome,
) -> Vec<Planned> {
    let mut planned = Vec::new();
    if let Some((kind, size)) = classify_root(root) {
        if kind != EntryKind::Directory {
            planned.push(Planned {
                path: root.clone(),
                kind,
                size,
                count: true,
            });
            return planned;
        }
    }

    let mut stack: Vec<Frame> = Vec::new();
    match Frame::enter(root, options, guard, outcome) {
        Entered::Frame(frame) => stack.push(frame),
        Entered::Gone | Entered::Failed => return planned,
    }

    while let Some(top) = stack.len().checked_sub(1) {
        let Some(child) = stack[top].children.next() else {
            let finished = stack.pop().expect("the frame was just seen");
            let is_root = stack.is_empty();
            // Неудача вложенной папки означает, что предок не опустеет:
            // планировать и считать его удалённым нельзя.
            if finished.failed {
                if let Some(parent) = stack.last_mut() {
                    parent.failed = true;
                }
            }
            if !finished.failed {
                planned.push(Planned {
                    path: finished.dir,
                    kind: EntryKind::Directory,
                    size: 0,
                    count: !is_root,
                });
            }
            continue;
        };

        let path = stack[top].dir.child_raw(&child.name);
        let kind = kind_of(child.attributes);
        if kind == EntryKind::Directory {
            if stack.len() > MAX_DEPTH {
                outcome.fail(&path, NtError(206), Vec::new());
                stack[top].failed = true;
                continue;
            }
            match Frame::enter(&path, options, guard, outcome) {
                Entered::Frame(frame) => stack.push(frame),
                Entered::Gone => {}
                Entered::Failed => stack[top].failed = true,
            }
        } else {
            planned.push(Planned {
                path,
                kind,
                size: child.size,
                count: true,
            });
        }
    }
    planned
}

struct Frame {
    dir: NtPath,
    children: std::vec::IntoIter<dir::Child>,
    failed: bool,
}

enum Entered {
    Frame(Frame),
    /// Каталог исчез между перечислением родителя и заходом в него.
    Gone,
    Failed,
}

impl Frame {
    fn enter(
        dir: &NtPath,
        options: &RemoveOptions,
        guard: Option<&Containment>,
        outcome: &mut Outcome,
    ) -> Entered {
        let mut listing = dir::children(dir);
        // ACL-стена на папке: без владения содержимого не увидеть. Ровно тот
        // же приём, что при обычном удалении.
        if let Err(error) = &listing {
            if error.access_denied() && options.take_ownership && !options.dry_run {
                if security::take_ownership(dir, guard).is_ok() {
                    outcome.notes.push(Note::OwnershipTaken {
                        path: dir.to_string_lossy(),
                    });
                }
                if security::grant_full_control(dir, guard).is_ok() {
                    outcome.notes.push(Note::AccessGranted {
                        path: dir.to_string_lossy(),
                    });
                }
                listing = dir::children(dir);
            }
        }
        let children = match listing {
            Ok(children) => children,
            Err(error) if error.not_found() => {
                outcome.notes.push(Note::Vanished {
                    path: dir.to_string_lossy(),
                });
                return Entered::Gone;
            }
            Err(error) => {
                outcome.fail(dir, error, Vec::new());
                return Entered::Failed;
            }
        };
        // Каталог обязан действительно лежать внутри цели: подменённый
        // junction'ом, он не должен увести планирование в чужое дерево.
        // Список выше мог быть получен через ссылку — поэтому проверка идёт
        // до того, как содержимое попадёт в план.
        if let Some(guard) = guard {
            match open_for_read(dir) {
                Ok(handle) if guard.holds(&handle) => {}
                Ok(_) => {
                    outcome.fail(dir, NtError(ERROR_OUTSIDE_TREE), Vec::new());
                    return Entered::Failed;
                }
                Err(error) if error.not_found() => {
                    outcome.notes.push(Note::Vanished {
                        path: dir.to_string_lossy(),
                    });
                    return Entered::Gone;
                }
                Err(error) => {
                    outcome.fail(dir, error, Vec::new());
                    return Entered::Failed;
                }
            }
        }
        Entered::Frame(Self {
            dir: dir.clone(),
            children: children.into_iter(),
            failed: false,
        })
    }
}

/// `\\?\C:\...` (и `\\?\UNC\...`) в NT-форму `\??\C:\...` для
/// `PendingFileRenameOperations`: префикс той же длины, меняются две единицы.
fn nt_source(path: &NtPath) -> Result<Vec<u16>, NtError> {
    let mut wide = path.ext().as_slice().to_vec();
    if wide.len() >= 4
        && wide[0] == u16::from(b'\\')
        && wide[1] == u16::from(b'\\')
        && wide[2] == u16::from(b'?')
        && wide[3] == u16::from(b'\\')
    {
        wide[1] = u16::from(b'?');
        wide[2] = u16::from(b'?');
        Ok(wide)
    } else {
        Err(NtError(ERROR_INVALID_PARAMETER))
    }
}

fn registry_error(error: std::io::Error) -> NtError {
    match error.raw_os_error() {
        Some(code) => NtError(code as u32),
        None => NtError(ERROR_INVALID_DATA),
    }
}

/// `REG_MULTI_SZ` в список строк. Одиночный завершающий ноль — терминатор
/// значения, а не строка; пустые строки внутри списка значимы: у пары
/// «удалить» цель как раз пустая.
fn parse_multi_sz(bytes: &[u8]) -> Result<Vec<Vec<u16>>, NtError> {
    if bytes.len() % 2 != 0 {
        return Err(NtError(ERROR_INVALID_DATA));
    }
    let mut items: Vec<Vec<u16>> = Vec::new();
    let mut current: Vec<u16> = Vec::new();
    for chunk in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            items.push(std::mem::take(&mut current));
        } else {
            current.push(unit);
        }
    }
    if !current.is_empty() {
        // Строка без завершающего нуля — значение не multi-sz.
        return Err(NtError(ERROR_INVALID_DATA));
    }
    items.pop();
    // Записи идут парами «источник, цель»: нечётное число строк — значение
    // не того вида, и дописывание сдвинуло бы пары (чужой источник получил бы
    // нашу цель и был бы переименован, а не удалён).
    if items.len() % 2 != 0 {
        return Err(NtError(ERROR_INVALID_DATA));
    }
    Ok(items)
}

fn build_multi_sz(items: &[Vec<u16>]) -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    for item in items {
        units.extend_from_slice(item);
        units.push(0);
    }
    units.push(0);
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

/// Добавляет пары «удалить» в конец значения и проверяет запись повторным
/// чтением: последний добавленный источник обязан читаться обратно.
fn append_pending(key: &RegKey, sources: &[Vec<u16>]) -> Result<(), NtError> {
    let mut items = match key.get_raw_value(VALUE_NAME) {
        Ok(value) => {
            if value.vtype != REG_MULTI_SZ {
                return Err(NtError(ERROR_INVALID_DATA));
            }
            parse_multi_sz(&value.bytes)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(registry_error(error)),
    };
    for source in sources {
        items.push(source.clone());
        items.push(Vec::new());
    }
    let bytes = build_multi_sz(&items);
    if bytes.len() > MAX_VALUE_BYTES {
        return Err(NtError(ERROR_FILE_TOO_LARGE));
    }
    key.set_raw_value(
        VALUE_NAME,
        &RegValue {
            bytes: bytes.into(),
            vtype: REG_MULTI_SZ,
        },
    )
    .map_err(registry_error)?;
    let last = sources.last().expect("sources are not empty here");
    let confirmed = key.get_raw_value(VALUE_NAME).map_err(registry_error)?;
    if !parse_multi_sz(&confirmed.bytes)?
        .iter()
        .any(|item| item == last)
    {
        return Err(NtError(ERROR_WRITE_FAULT));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use winreg::enums::HKEY_CURRENT_USER;

    fn scratch(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "raze-reboot-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// Тестовый ключ в HKCU: запись не требует администратора и не трогает
    /// настоящее расписание загрузки.
    fn test_key() -> (RegKey, String) {
        let path = format!(
            r"Software\RAZE.Tests\pfor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let root = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = root.create_subkey(&path).unwrap();
        (key, path)
    }

    #[test]
    fn multi_sz_round_trips_and_keeps_empty_targets() {
        let items: Vec<Vec<u16>> = ["A", "", "B", ""]
            .iter()
            .map(|s| s.encode_utf16().collect())
            .collect();
        let bytes = build_multi_sz(&items);
        assert_eq!(parse_multi_sz(&bytes).unwrap(), items);

        // Существующая запись чужого установщика: пара «удалить» — имя X,
        // пустая цель и терминатор; восемь байт: 58 00 00 00 00 00 00 00.
        let parsed = parse_multi_sz(b"X\0\0\0\0\0\0\0").unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(String::from_utf16_lossy(&parsed[0]), "X");
        assert!(parsed[1].is_empty());

        assert!(parse_multi_sz(&[0x41, 0x00, 0x41]).is_err());
        // Источник без пары: дописывать в такое значение нельзя.
        assert!(parse_multi_sz(b"X\0\0\0\0\0").is_err());
    }

    #[test]
    fn appending_keeps_existing_entries_and_confirms_the_write() {
        let (key, path) = test_key();
        let root = RegKey::predef(HKEY_CURRENT_USER);
        let source: Vec<u16> = r"\??\C:\not\our\business".encode_utf16().collect();
        append_pending(&key, std::slice::from_ref(&source)).unwrap();
        let ours: Vec<u16> = r"\??\D:\raze-test\scheduled".encode_utf16().collect();
        append_pending(&key, std::slice::from_ref(&ours)).unwrap();

        let value = key.get_raw_value(VALUE_NAME).unwrap();
        let items = parse_multi_sz(&value.bytes).unwrap();
        assert_eq!(items, vec![source.clone(), Vec::new(), ours, Vec::new()]);
        drop(key);
        let _ = root.delete_subkey_all(&path);
    }

    #[test]
    fn a_too_large_batch_is_refused_whole() {
        let (key, path) = test_key();
        let root = RegKey::predef(HKEY_CURRENT_USER);
        let fill = "X".repeat(MAX_VALUE_BYTES / 32);
        let sources: Vec<Vec<u16>> = (0..64).map(|_| fill.encode_utf16().collect()).collect();
        let error = append_pending(&key, &sources).unwrap_err();
        assert_eq!(error.code(), ERROR_FILE_TOO_LARGE);
        // Ничего не записано: отказ — это отказ целиком.
        assert!(key.get_raw_value(VALUE_NAME).is_err());
        drop(key);
        let _ = root.delete_subkey_all(&path);
    }

    #[test]
    fn dry_run_counts_without_touching_the_registry() {
        let base = scratch("dry");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("one.bin"), b"12345").unwrap();
        std::fs::write(base.join("sub/two.bin"), b"x").unwrap();
        let options = RemoveOptions {
            dry_run: true,
            ..Default::default()
        };
        let outcome = schedule_tree(&NtPath::new(&base).unwrap(), &options);
        assert!(!outcome.failed(), "{:?}", outcome.failures);
        assert_eq!(outcome.removed_files, 2);
        assert_eq!(outcome.removed_directories, 1, "подпапка, но не корень");
        assert_eq!(outcome.freed_bytes, 6);
        assert!(base.join("one.bin").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_scheduled_tree_is_written_children_first() {
        let base = scratch("order");
        std::fs::create_dir_all(base.join("sub/deep")).unwrap();
        std::fs::write(base.join("sub/deep/file.txt"), b"x").unwrap();
        std::fs::write(base.join("top.txt"), b"y").unwrap();
        let (key, path) = test_key();
        let root = RegKey::predef(HKEY_CURRENT_USER);

        let options = RemoveOptions::default();
        let outcome = schedule_with(&NtPath::new(&base).unwrap(), &options, Some(&key));
        assert!(!outcome.failed(), "{:?}", outcome.failures);
        assert_eq!(outcome.removed_files, 2);
        assert_eq!(outcome.removed_directories, 2, "sub и deep, но не корень");

        let value = key.get_raw_value(VALUE_NAME).unwrap();
        let items = parse_multi_sz(&value.bytes).unwrap();
        let sources: Vec<String> = items
            .iter()
            .step_by(2)
            .map(|s| String::from_utf16_lossy(s))
            .collect();
        let position = |needle: &str| {
            sources
                .iter()
                .position(|s| s.ends_with(needle))
                .unwrap_or_else(|| panic!("{needle} отсутствует в {sources:?}"))
        };
        // Файлы раньше своих папок, папки — снизу вверх, корень последний.
        assert!(position("file.txt") < position(r"deep"));
        assert!(position(r"deep") < position(r"sub"));
        assert!(position(r"sub") < position("order"));
        // Пары «удалить»: за каждым источником идёт пустая цель.
        assert_eq!(items.len(), sources.len() * 2);
        assert!(items
            .iter()
            .skip(1)
            .step_by(2)
            .all(|target| target.is_empty()));

        drop(key);
        let _ = root.delete_subkey_all(&path);
        let _ = std::fs::remove_dir_all(&base);
    }
}
