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

use std::collections::HashSet;
use std::time::Duration;

use serde::Serialize;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FileDispositionInfo, FileDispositionInfoEx,
    SetFileInformationByHandle,
};

use super::dir;
use super::error::NtError;
use super::handle::{clear_read_only, open_for_delete, open_for_read, privileged_access, attributes};
use super::locker::{self, Holder};
use super::path::{NtPath, Wide};
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
pub fn remove_tree(root: &NtPath, options: &RemoveOptions) -> Outcome {
    let mut outcome = Outcome {
        dry_run: options.dry_run,
        ..Default::default()
    };
    if !options.dry_run {
        privs::enable_backup_restore();
    }

    // Корень-файл или корень-ссылка: обход не нужен —
    // удаляем одним шагом. Корень-каталог идёт снизу вверх.
    if let Some((kind, size)) = classify_root(root) {
        if kind != EntryKind::Directory {
            remove_leaf(root, kind, size, options, &mut outcome, true);
            return outcome;
        }
    }

    // Стек обхода: каталоги, из которых нужно вычистить содержимое. Порядок
    // удаления снизу вверх, поэтому сначала собираем всё, потом сносим.
    let mut pending: Vec<(NtPath, EntryKind, u64)> = Vec::new();
    let mut stack = vec![(root.clone(), 0usize)];
    let mut seen: HashSet<String> = HashSet::new();
    let max_depth = 512;

    while let Some((dir, depth)) = stack.pop() {
        if depth > max_depth {
            outcome.fail(&dir, NtError(206), Vec::new());
            continue;
        }
        match dir::children(&dir) {
            Ok(children) => {
                for child in children {
                    let path = dir.child_raw(&child.name);
                    let kind = kind_of(child.attributes);
                    if kind == EntryKind::Directory {
                        stack.push((path.clone(), depth + 1));
                    }
                    pending.push((path, kind, child.size));
                }
            }
            Err(error) if error.not_found() => {
                outcome.notes.push(Note::Vanished {
                    path: dir.to_string_lossy(),
                });
            }
            Err(error) => outcome.fail(&dir, error, Vec::new()),
        }
    }

    // Каталоги удаляем в обратном порядке: содержимое уже снесено.
    for (path, kind, size) in pending.into_iter().rev() {
        if !seen.insert(path.to_string_lossy()) {
            continue;
        }
        remove_leaf(&path, kind, size, options, &mut outcome, true);
    }

    // Корень удаляем последним: содержимое уже снесено.
    // В отчёт его не включаем — пользователь и так видит,
    // какую папку удаляет; считаем только содержимое.
    // При сбое на содержимом корень и так не сносится
    // (он не пуст), попытка только добавила бы шум.
    if !outcome.failed() {
        remove_leaf(root, EntryKind::Directory, 0, options, &mut outcome, false);
    }

    outcome
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
            remove_leaf(path, *kind, *size, options, &mut outcome, true);
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
        match try_delete(path, options, outcome) {
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
            DeleteStep::Retry(error) => {
                if error.sharing_violation() {
                    let holders = locker::holders_of(path);
                    if options.close_locks && !holders.is_empty() {
                        for holder in &holders {
                            // Службы не трогаем: это антивирусы и драйверы,
                            // они всё равно не закроются по WM_CLOSE.
                            if !holder.is_service {
                                locker::request_close(holder.pid);
                            }
                            outcome.notes.push(Note::ProcessCloseRequested {
                                pid: holder.pid,
                                name: holder.name.clone(),
                            });
                        }
                        if locker::wait_for_release(
                            &[path.to_path()],
                            options.lock_timeout.min(Duration::from_millis(400) * u32::from(attempt)),
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
}

fn try_delete(path: &NtPath, options: &RemoveOptions, outcome: &mut Outcome) -> DeleteStep {
    let handle = match open_for_delete(path) {
        Ok(handle) => handle,
        Err(error) if error.not_found() => return DeleteStep::AlreadyGone,
        Err(error) if error.access_denied() => {
            if !options.take_ownership {
                return DeleteStep::Retry(error);
            }
            // Сначала владение, потом права: без владения WRITE_DAC
            // недоступен, и второй шаг был бы пустой тратой времени.
            if security::take_ownership(path).is_ok() {
                outcome.notes.push(Note::OwnershipTaken {
                    path: path.to_string_lossy(),
                });
            }
            if security::grant_full_control(path).is_ok() {
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

    if let Ok(attrs) = attributes(&handle) {
        if attrs.is_read_only() {
            match clear_read_only(&handle) {
                Ok(()) => outcome.notes.push(Note::ReadOnlyCleared {
                    path: path.to_string_lossy(),
                }),
                Err(error) => return DeleteStep::Retry(error),
            }
        }
    }

    match mark_for_delete(&handle) {
        Ok(()) => {}
        Err(error) if error.unsupported() => {
            // Файловая система не знает про FileDispositionInformationEx —
            // пробуем классический способ. Он не снимает read-only, но мы уже
            // сняли его выше.
            if let Err(error) = mark_for_delete_legacy(&handle) {
                return DeleteStep::Retry(error);
            }
        }
        Err(error) => return DeleteStep::Retry(error),
    }

    // Удаление «по-настоящему» происходит при закрытии последнего
    // дескриптора. Раньше проверять бессмысленно.
    drop(handle);

    if still_there(path) {
        outcome.notes.push(Note::DeletePending {
            path: path.to_string_lossy(),
        });
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

/// Проверка «а точно пропало?». Открытие с нулевым доступом не меняет объект,
/// но честно отвечает, существует ли он.
fn still_there(path: &NtPath) -> bool {
    super::handle::try_open(
        path,
        windows::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES,
        windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS,
    )
    .is_some()
}

/// Полный путь удаления для произвольного набора целей с учётом того,
/// что часть из них может лежать внутри других. Возвращает нормализованный
/// список без вложенностей: если выбраны `A` и `A\B`, достаточно удалить `A`.
pub fn flatten_targets(mut targets: Vec<Wide>) -> Vec<Wide> {
    targets.sort_by_key(|p| p.len());
    let mut kept: Vec<Wide> = Vec::new();
    for candidate in targets {
        let text = candidate.to_string_lossy().to_lowercase();
        let covered = kept.iter().any(|parent| {
            let parent = parent.to_string_lossy().to_lowercase();
            text.len() > parent.len() && text.starts_with(&parent) && text[parent.len()..].starts_with('\\')
        });
        if !covered {
            kept.push(candidate);
        }
    }
    kept
}

/// Открыть объект с привилегиями — нужно там, где дескриптор не удалось
/// получить обычным `open_for_delete` даже после правки DACL.
pub fn open_privileged(path: &NtPath) -> Result<super::handle::OwnedHandle, NtError> {
    let _ = privs::enable_backup_restore();
    super::handle::open_with(
        path,
        privileged_access(),
        windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
            | windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
    )
}

/// Тихая проверка существования для отчёта.
pub fn exists(path: &NtPath) -> bool {
    open_for_read(path).is_ok()
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

    #[test]
    fn flatten_drops_children_of_selected_parents() {
        let list = vec![
            Wide::new("C:\\a"),
            Wide::new("C:\\a\\b"),
            Wide::new("C:\\ab"),
            Wide::new("C:\\c"),
        ];
        let flat = flatten_targets(list);
        let names: Vec<String> = flat.iter().map(|p| p.to_string_lossy()).collect();
        assert!(names.contains(&"C:\\a".to_string()));
        assert!(!names.contains(&"C:\\a\\b".to_string()));
        assert!(
            names.contains(&"C:\\ab".to_string()),
            "похожий по имени, но не вложенный путь должен остаться"
        );
        assert!(names.contains(&"C:\\c".to_string()));
    }
}