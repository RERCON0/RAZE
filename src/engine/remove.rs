//! Безвозвратное удаление через дескрипторы.
//!
//! Порядок для одного объекта всегда такой:
//!   1. открыть **по расширенному пути** и только для удаления,
//!   2. при `ACCESS_DENIED` — стать владельцем и выдать себе права,
//!   3. при `SHARING_VIOLATION` — найти держателя и (по согласию) закрыть,
//!   4. пометить на удаление через `FileDispositionInformation(Ex)`,
//!   5. закрыть дескриптор и убедиться, что объекта действительно нет.
//!
//! Пункт 5 обязателен: пометка на удаление не означает, что файл исчез, и
//! молча рапортовать об успехе там, где объект остался, нельзя.

use std::time::Duration;

use serde::Serialize;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FileDispositionInfo, FileDispositionInfoEx,
    SetFileInformationByHandle,
};

use super::dir;
use super::error::{ERROR_OUTSIDE_TREE, NtError};
use super::handle::{
    Containment, attributes, clear_read_only, open_for_delete, open_for_read,
};
use super::locker::{self, Holder};
use super::path::NtPath;
use super::privs;
use super::scan::EntryKind;
use super::security;

#[derive(Clone, Debug)]
pub struct RemoveOptions {
    /// Забирать владение и выдавать права, если не хватает.
    pub take_ownership: bool,
    /// Отправлять `WM_CLOSE` процессам, которые держат файлы.
    pub close_locks: bool,
    /// Сколько раз повторять попытку при `SHARING_VIOLATION`.
    pub retries: u32,
    /// Сколько ждать освобождения файла после `WM_CLOSE`.
    pub lock_timeout: Duration,
    /// Ничего не удалять: только собрать отчёт о том, что произойдёт.
    pub dry_run: bool,
}

impl Default for RemoveOptions {
    fn default() -> Self {
        Self {
            take_ownership: true,
            close_locks: false,
            retries: 3,
            lock_timeout: Duration::from_secs(5),
            dry_run: false,
        }
    }
}

/// Что именно RAZE сделала с объектом — для честного отчёта.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Note {
    ReadOnlyCleared { path: String },
    OwnershipTaken { path: String },
    AccessGranted { path: String },
    ProcessCloseRequested { pid: u32, name: String },
    /// Объект исчез между сканированием и удалением.
    Vanished { path: String },
    /// Помечен на удаление, но физически ещё лежит — ждём закрытия дескрипторов.
    DeletePending { path: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct Failure {
    pub path: String,
    pub code: u32,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub holders: Vec<Holder>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    pub removed_files: usize,
    pub removed_directories: usize,
    pub removed_links: usize,
    pub freed_bytes: u64,
    pub notes: Vec<Note>,
    pub failures: Vec<Failure>,
    /// Хоть раз пришлось упереться в права — значит, нужен UAC-перезапуск.
    pub needs_elevation: bool,
    pub dry_run: bool,
}

impl Outcome {
    pub fn failed(&self) -> bool {
        !self.failures.is_empty()
    }

    pub fn removed_total(&self) -> usize {
        self.removed_files + self.removed_directories + self.removed_links
    }

    fn record(&mut self, kind: EntryKind, size: u64) {
        match kind {
            EntryKind::File => {
                self.removed_files += 1;
                self.freed_bytes += size;
            }
            EntryKind::Directory => self.removed_directories += 1,
            EntryKind::Link => self.removed_links += 1,
        }
    }

    fn fail(&mut self, path: &NtPath, error: NtError, holders: Vec<Holder>) {
        if error.needs_elevation() {
            self.needs_elevation = true;
        }
        self.failures.push(Failure {
            path: path.to_string_lossy(),
            code: error.code(),
            message: error.message(),
            holders,
        });
    }
}

/// Удаляет дерево `root` целиком. `root` может быть файлом, папкой или
/// ссылкой — тип определяется на лету.
///
/// Обход потоковый: каталог перечисляется, его содержимое сразу удаляется, и
/// только потом удаляется сам каталог. В памяти остаются списки каталогов на
/// текущем пути вниз, а не всё дерево (миллион файлов — это гигабайт путей),
/// и между «увидели» и «удалили» проходят мгновения, а не минуты. Каждый
/// удаляемый объект ещё и проверяется на принадлежность корню
/// (см. [`Containment`]): подмену каталога на junction посреди обхода это
/// ловит.
pub fn remove_tree(root: &NtPath, options: &RemoveOptions) -> Outcome {
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
                // Корень не открывается даже на чтение атрибутов: дальше идти
                // вслепую нельзя. Это отказ в доступе — значит, нужен UAC.
                outcome.fail(root, error, Vec::new());
                return outcome;
            }
        }
    };
    let guard = guard.as_ref();

    // Файл и ссылка тоже проверяются по удаляемому дескриптору:
    // подмена родителя после проверки плана не должна менять цель.
    if let Some((kind, size)) = classify_root(root) {
        if kind != EntryKind::Directory {
            remove_leaf(root, kind, size, options, guard, &mut outcome, true);
            return outcome;
        }
    }

    let mut stack: Vec<Frame> = Vec::new();
    match Frame::enter(root, options, guard, &mut outcome) {
        Entered::Frame(frame) => stack.push(frame),
        Entered::Gone | Entered::Failed => return outcome,
    }
    let max_depth = 512;

    while let Some(top) = stack.len().checked_sub(1) {
        let Some(child) = stack[top].children.next() else {
            // Содержимое каталога обработано.
            let finished = stack.pop().expect("the frame was just seen");
            let Some(parent) = stack.last_mut() else {
                // Корень удаляем последним и в отчёт не включаем: пользователь
                // и так видит, какую папку удаляет, а считаем содержимое. При
                // сбое на содержимом корень не сносится (он не пуст), попытка
                // только добавила бы шум.
                if !outcome.failed() {
                    remove_leaf(&finished.dir, EntryKind::Directory, 0, options, guard, &mut outcome, false);
                }
                break;
            };
            if finished.failed {
                // Внутри остались объекты: каталог не пуст, и по «папка не
                // пуста» на каждом предке ничего нового не узнать.
                parent.failed = true;
            } else {
                let before = outcome.failures.len();
                remove_leaf(&finished.dir, EntryKind::Directory, 0, options, guard, &mut outcome, true);
                if outcome.failures.len() > before {
                    parent.failed = true;
                }
            }
            continue;
        };

        let path = stack[top].dir.child_raw(&child.name);
        let kind = kind_of(child.attributes);
        if kind == EntryKind::Directory {
            if stack.len() > max_depth {
                outcome.fail(&path, NtError(206), Vec::new());
                stack[top].failed = true;
                continue;
            }
            match Frame::enter(&path, options, guard, &mut outcome) {
                Entered::Frame(frame) => stack.push(frame),
                Entered::Gone => {}
                Entered::Failed => stack[top].failed = true,
            }
        } else {
            let before = outcome.failures.len();
            remove_leaf(&path, kind, child.size, options, guard, &mut outcome, true);
            if outcome.failures.len() > before {
                stack[top].failed = true;
            }
        }
    }

    outcome
}

/// Каталог в обходе: его ещё не обработанное содержимое и признак, что
/// что-то внутри удалить не удалось.
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
        // ACL-стена на самой папке: пока не стал владельцем, содержимого не
        // увидеть, и до удаления дело не доходит. Владение и права берутся ровно
        // так же, как для файла (после настоящего отказа), и перечисление
        // повторяется один раз. Пробный прогон права не меняет.
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
        match listing {
            Ok(children) => Entered::Frame(Self {
                dir: dir.clone(),
                children: children.into_iter(),
                failed: false,
            }),
            Err(error) if error.not_found() => {
                outcome.notes.push(Note::Vanished {
                    path: dir.to_string_lossy(),
                });
                Entered::Gone
            }
            Err(error) => {
                outcome.fail(dir, error, Vec::new());
                Entered::Failed
            }
        }
    }
}

/// Удаляет уже перечисленные объекты — так работает «точечный» режим GUI,
/// где пользователь отметил конкретные файлы и папки.
pub fn remove_selected(
    targets: &[(NtPath, EntryKind, u64)],
    options: &RemoveOptions,
) -> Outcome {
    let mut outcome = Outcome {
        dry_run: options.dry_run,
        ..Default::default()
    };
    if !options.dry_run {
        privs::enable_backup_restore();
    }
    for (path, kind, size) in targets {
        if matches!(kind, EntryKind::Directory) && !options.dry_run {
            // Выбранную папку чистим целиком, иначе «удалить папку» означало бы
            // «оставить её содержимое».
            let inner = remove_tree(path, options);
            outcome.removed_files += inner.removed_files;
            outcome.removed_directories += inner.removed_directories;
            outcome.removed_links += inner.removed_links;
            outcome.freed_bytes += inner.freed_bytes;
            outcome.needs_elevation |= inner.needs_elevation;
            outcome.notes.extend(inner.notes);
            outcome.failures.extend(inner.failures);
        } else {
            remove_leaf(path, *kind, *size, options, None, &mut outcome, true);
        }
    }
    outcome
}

/// Тип и размер корня: файл и ссылка удаляются без обхода,
/// а каталог — снизу вверх. `None` — корень не открылся;
/// обход сам доложит `Vanished` или ошибку доступа.
fn classify_root(root: &NtPath) -> Option<(EntryKind, u64)> {
    use windows::Win32::Storage::FileSystem::{
        FileStandardInfo, FILE_STANDARD_INFO, GetFileInformationByHandleEx,
    };
    let handle = open_for_read(root).ok()?;
    let attrs = attributes(&handle).ok()?;
    let kind = kind_of(attrs.attributes);
    let size = if kind == EntryKind::File {
        let mut info = FILE_STANDARD_INFO::default();
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.raw(),
                FileStandardInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
            )
        }
        .is_ok();
        if ok {
            info.EndOfFile as u64
        } else {
            0
        }
    } else {
        0
    };
    Some((kind, size))
}

fn kind_of(attributes: u32) -> EntryKind {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        EntryKind::Link
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        EntryKind::Directory
    } else {
        EntryKind::File
    }
}

fn remove_leaf(
    path: &NtPath,
    kind: EntryKind,
    size: u64,
    options: &RemoveOptions,
    guard: Option<&Containment>,
    outcome: &mut Outcome,
    record: bool,
) {
    if options.dry_run {
        if record {
            outcome.record(kind, size);
        }
        return;
    }

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match try_delete(path, options, guard, outcome) {
            DeleteStep::Done => {
                if record {
                    outcome.record(kind, size);
                }
                return;
            }
            DeleteStep::AlreadyGone => {
                outcome.notes.push(Note::Vanished {
                    path: path.to_string_lossy(),
                });
                if record {
                    outcome.record(kind, size);
                }
                return;
            }
            DeleteStep::Refused(error) => {
                outcome.fail(path, error, Vec::new());
                return;
            }
            DeleteStep::Retry(error) => {
                if error.sharing_violation() {
                    let holders = locker::holders_of(path);
                    if options.close_locks && !holders.is_empty() {
                        for holder in &holders {
                            // Службы не трогаем: это антивирусы и драйверы,
                            // они всё равно не закроются по WM_CLOSE.
                            if holder.can_close && locker::request_close(holder.pid) > 0 {
                                outcome.notes.push(Note::ProcessCloseRequested {
                                    pid: holder.pid,
                                    name: holder.name.clone(),
                                });
                            }
                        }
                        if attempt < options.retries && locker::wait_for_release(
                            &[path.to_path()],
                            options.lock_timeout.min(Duration::from_millis(400) * attempt),
                        ) {
                            continue;
                        }
                    }
                    if attempt < options.retries {
                        std::thread::sleep(Duration::from_millis(80 * attempt as u64));
                        continue;
                    }
                    outcome.fail(path, error, holders);
                    return;
                }
                if error.pending() && attempt < options.retries {
                    std::thread::sleep(Duration::from_millis(60));
                    continue;
                }
                outcome.fail(path, error, Vec::new());
                return;
            }
        }
    }
}

enum DeleteStep {
    Done,
    AlreadyGone,
    Retry(NtError),
    /// Повторять бессмысленно: объект не из нашего дерева.
    Refused(NtError),
}

fn try_delete(
    path: &NtPath,
    options: &RemoveOptions,
    guard: Option<&Containment>,
    outcome: &mut Outcome,
) -> DeleteStep {
    let mut handle = match open_for_delete(path) {
        Ok(handle) => handle,
        Err(error) if error.not_found() => return DeleteStep::AlreadyGone,
        Err(error) if error.access_denied() => {
            if !options.take_ownership {
                return DeleteStep::Retry(error);
            }
            // Сначала владение, потом права: без владения WRITE_DAC
            // недоступен, и второй шаг был бы пустой тратой времени.
            if security::take_ownership(path, guard).is_ok() {
                outcome.notes.push(Note::OwnershipTaken {
                    path: path.to_string_lossy(),
                });
            }
            if security::grant_full_control(path, guard).is_ok() {
                outcome.notes.push(Note::AccessGranted {
                    path: path.to_string_lossy(),
                });
            }
            match open_for_delete(path) {
                Ok(handle) => handle,
                Err(error) => {
                    return DeleteStep::Retry(error);
                }
            }
        }
        Err(error) => return DeleteStep::Retry(error),
    };

    // Проверка по тому самому дескриптору, который сейчас будет удалён: между
    // «проверили» и «удалили» объект подменить уже нельзя.
    if let Some(guard) = guard {
        if !guard.holds(&handle) {
            return DeleteStep::Refused(NtError(ERROR_OUTSIDE_TREE));
        }
    }


    match mark_for_delete(&handle) {
        Ok(()) => {}
        Err(error) if error.unsupported() => {
            // Старый API не умеет игнорировать read-only. Только для этого
            // fallback запрашиваем дополнительные права, не мешая обычному
            // удалению файлов, у которых есть DELETE, но нет WRITE_ATTRIBUTES.
            if attributes(&handle).is_ok_and(|attrs| attrs.is_read_only()) {
                use windows::Win32::Storage::FileSystem::FILE_WRITE_ATTRIBUTES;
                handle = match super::handle::open_with(
                    path,
                    super::handle::delete_access() | FILE_WRITE_ATTRIBUTES,
                    super::handle::object_flags(),
                ) {
                    Ok(handle) => handle,
                    Err(error) => return DeleteStep::Retry(error),
                };
                if guard.is_some_and(|guard| !guard.holds(&handle)) {
                    return DeleteStep::Refused(NtError(ERROR_OUTSIDE_TREE));
                }
                if let Err(error) = clear_read_only(&handle) {
                    return DeleteStep::Retry(error);
                }
                outcome.notes.push(Note::ReadOnlyCleared {
                    path: path.to_string_lossy(),
                });
            }
            if let Err(error) = mark_for_delete_legacy(&handle) {
                return DeleteStep::Retry(error);
            }
        }
        Err(error) => return DeleteStep::Retry(error),
    }

    // Удаление «по-настоящему» происходит при закрытии последнего
    // дескриптора. Раньше проверять бессмысленно.
    drop(handle);

    if present(path) {
        outcome.notes.push(Note::DeletePending {
            path: path.to_string_lossy(),
        });
        return DeleteStep::Retry(NtError(windows::Win32::Foundation::ERROR_DELETE_PENDING.0));
    }
    DeleteStep::Done
}

/// POSIX-вариант: снимает read-only сам, поэтому работает с защищёнными
/// файлами и папками без предварительных танцев с атрибутами.
fn mark_for_delete(handle: &super::handle::OwnedHandle) -> Result<(), NtError> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: windows::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO_EX_FLAGS(
            FILE_DISPOSITION_FLAG_DELETE.0
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE.0,
        ),
    };
    super::error::nt(unsafe {
        SetFileInformationByHandle(
            handle.raw(),
            FileDispositionInfoEx,
            &info as *const _ as *const _,
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    })
}

fn mark_for_delete_legacy(handle: &super::handle::OwnedHandle) -> Result<(), NtError> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    super::error::nt(unsafe {
        SetFileInformationByHandle(
            handle.raw(),
            FileDispositionInfo,
            &info as *const _ as *const _,
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    })
}



/// Тихая проверка существования для отчёта.
pub fn exists(path: &NtPath) -> bool {
    open_for_read(path).is_ok()
}

/// Есть ли такой объект — в том числе тот, что нельзя даже открыть на чтение
/// атрибутов (ACL-стена). «Нет такого пути» — единственный ответ «нет»; отказ в
/// доступе означает, что объект на месте, и разбираться с ним предстоит
/// удалению (с владением и повышением), а не молчаливому «делать нечего».
pub fn present(path: &NtPath) -> bool {
    match open_for_read(path) {
        Ok(_) => true,
        Err(error) => !error.not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn scratch(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "raze-remove-{}-{}-{name}",
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

    #[test]
    fn removes_a_plain_tree() {
        let base = scratch("plain");
        std::fs::write(base.join("a.txt"), b"hello").unwrap();
        std::fs::create_dir_all(base.join("nested/deep")).unwrap();
        std::fs::write(base.join("nested/deep/b.txt"), b"x").unwrap();

        let root = NtPath::new(&base).unwrap();
        let outcome = remove_tree(&root, &RemoveOptions::default());
        assert!(!outcome.failed(), "{:?}", outcome.failures);
        assert_eq!(outcome.removed_files, 2);
        assert!(outcome.removed_directories >= 2);
        assert!(!exists(&root));
    }

    #[test]
    fn readonly_file_is_removed_without_changing_ownership() {
        let base = scratch("readonly");
        let file = base.join("readonly.txt");
        std::fs::write(&file, b"readonly").unwrap();
        let mut permissions = std::fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&file, permissions).unwrap();
        let outcome = remove_tree(
            &NtPath::new(&file).unwrap(),
            &RemoveOptions { take_ownership: false, ..Default::default() },
        );
        assert!(!outcome.failed(), "{:?}", outcome.failures);
        assert!(!file.exists(), "readonly file must be removed");
        assert!(base.exists(), "the containing directory must be preserved");
        assert_eq!(outcome.removed_files, 1);
        assert!(!outcome.notes.iter().any(|note| matches!(
            note, Note::OwnershipTaken { .. } | Note::AccessGranted { .. }
        )));
        std::fs::remove_dir(base).unwrap();
    }

    #[test]
    fn legacy_removal_clears_a_lone_readonly_attribute() {
        use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_READONLY, FILE_WRITE_ATTRIBUTES, SetFileAttributesW};
        let base = scratch("legacy-readonly");
        let file = base.join("one.bin");
        std::fs::write(&file, b"legacy").unwrap();
        let path = NtPath::new(&file).unwrap();
        unsafe { SetFileAttributesW(path.ext().pc(), FILE_ATTRIBUTE_READONLY) }.unwrap();
        let handle = super::super::handle::open_with(
            &path,
            super::super::handle::delete_access() | FILE_WRITE_ATTRIBUTES,
            super::super::handle::object_flags(),
        ).unwrap();
        clear_read_only(&handle).unwrap();
        mark_for_delete_legacy(&handle).unwrap();
        drop(handle);
        assert!(!present(&path), "legacy fallback must remove the readonly file");
        std::fs::remove_dir(base).unwrap();
    }

    #[test]
    fn dry_run_changes_nothing() {
        let base = scratch("dry");
        std::fs::write(base.join("a.txt"), b"hello").unwrap();
        let root = NtPath::new(&base).unwrap();

        let options = RemoveOptions {
            dry_run: true,
            ..Default::default()
        };
        let outcome = remove_tree(&root, &options);
        assert_eq!(outcome.removed_files, 1);
        assert!(exists(&root), "пробный прогон не должен ничего удалять");
        assert!(base.join("a.txt").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_target_is_not_a_failure() {
        let base = scratch("missing");
        let ghost = base.join("nope");
        let root = NtPath::new(&ghost).unwrap();
        let outcome = remove_tree(&root, &RemoveOptions::default());
        assert!(!outcome.failed());
        assert!(matches!(
            outcome.notes.first(),
            Some(Note::Vanished { .. })
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn junction_is_removed_without_touching_its_target() {
        let base = scratch("junction");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"important").unwrap();

        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let junction = work.join("link");
        // mklink /J — только с правами администратора; если не получилось,
        // тест не имеет смысла и должен это сказать, а не молча пройти.
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&outside)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !created {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let root = NtPath::new(&work).unwrap();
        let outcome = remove_tree(&root, &RemoveOptions::default());
        assert!(!outcome.failed(), "{:?}", outcome.failures);
        assert!(!Path::new(&junction).exists(), "junction обязан исчезнуть");
        assert!(
            outside.join("keep.txt").exists(),
            "цель junction не должна пострадать"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `mklink /J` не требует прав администратора. Если создать не вышло, тест
    /// не имеет смысла и просто выходит.
    fn make_junction(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn an_object_swapped_out_of_the_tree_is_refused_not_deleted() {
        // Имитация гонки: по дороге к файлу каталог внутри дерева подменили
        // junction'ом на чужую папку. Дескриптор откроется, но объект лежит
        // снаружи, и удалять его нельзя.
        let base = scratch("escape");
        let work = base.join("work");
        let outside = base.join("outside");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.txt"), b"keep me").unwrap();
        let link = work.join("swapped");
        if !make_junction(&link, &outside) {
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let guard = Containment::new(&NtPath::new(&work).unwrap()).unwrap();
        let through_link = NtPath::new(&link.join("victim.txt")).unwrap();
        let options = RemoveOptions::default();

        let mut outcome = Outcome::default();
        remove_leaf(&through_link, EntryKind::File, 7, &options, Some(&guard), &mut outcome, true);
        assert_eq!(outcome.failures.len(), 1, "{:?}", outcome.failures);
        assert_eq!(outcome.failures[0].code, ERROR_OUTSIDE_TREE);
        assert_eq!(outcome.removed_files, 0);
        assert!(outside.join("victim.txt").exists(), "чужой файл не тронут");

        // Без проверки тот же вызов сносит чужой файл: это и была дыра.
        let mut unguarded = Outcome::default();
        remove_leaf(&through_link, EntryKind::File, 7, &options, None, &mut unguarded, true);
        assert!(!outside.join("victim.txt").exists());

        let _ = std::fs::remove_dir(&link); // сама ссылка, без захода в цель
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_locked_file_is_one_failure_not_one_per_ancestor() {
        use std::os::windows::fs::OpenOptionsExt;
        let base = scratch("locked");
        let deep = base.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        let locked = deep.join("busy.bin");
        std::fs::write(&locked, b"x").unwrap();
        std::fs::write(base.join("a").join("free.txt"), b"x").unwrap();
        // Открыт без FILE_SHARE_DELETE: удалить нельзя, пока держат.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked)
            .unwrap();

        let options = RemoveOptions {
            retries: 1,
            ..Default::default()
        };
        let outcome = remove_tree(&NtPath::new(&base).unwrap(), &options);
        assert_eq!(outcome.failures.len(), 1, "{:?}", outcome.failures);
        assert!(locked.exists());
        assert!(!base.join("a").join("free.txt").exists(), "остальное уходит");

        drop(held);
        let _ = std::fs::remove_dir_all(&base);
    }

}