//! Обход дерева и классификация проблем.
//!
//! Обход идёт **только** по расширенным путям и **никогда** не заходит внутрь
//! reparse-точек. Это не оптимизация, а требование безопасности: junction
//! внутри удаляемой папки почти всегда указывает наружу, и обход внутрь
//! снёс бы то, что удалять не просили.

use crate::tr_format;
use std::path::PathBuf;

use serde::Serialize;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM,
};

use super::dir;
use super::handle::{Attributes, attributes, open_for_delete, open_for_read};
use super::locker::{self, Holder};
use super::path::{LONG_PATH_WARN, NtPath, is_reserved_component_wide};

/// Тег reparse-точки «точка монтирования» — обычный junction.
pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA0_00_00_03;
/// Тег символической ссылки.
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA0_00_00_0C;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    /// Junction, символическая ссылка, точка облака — удаляется сам объект
    /// ссылки, её цель не трогаем.
    Link,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Blocker {
    /// Компонент пути — зарезервированное имя устройства (`NUL`, `CON`, ...).
    /// Из-за него `DeleteFileW` и Проводник не видят объект вообще.
    ReservedName { component: String },
    /// Путь длиннее, чем комфортно для Проводника и Корзины.
    LongPath { length: usize },
    /// Файл открыт другим процессом без `FILE_SHARE_DELETE`.
    InUse { holders: Vec<Holder> },
    /// Нет прав на удаление.
    AccessDenied,
    /// Атрибут «только чтение» — снимается автоматически, но помечаем.
    ReadOnly,
    System,
    Hidden,
    ReparsePoint { tag: u32 },
    /// Не удалось даже прочитать содержимое каталога.
    Unreadable { code: u32 },
    /// Объект нельзя отправить в Корзину (удаление будет безвозвратным).
    NotRecyclable { reason: String },
}

impl Blocker {
    /// Помечает ли проблема объект как «застрявший» — то есть такой, ради
    /// которого обычная корзина не подходит.
    pub fn is_trouble(&self) -> bool {
        matches!(
            self,
            Blocker::ReservedName { .. }
                | Blocker::LongPath { .. }
                | Blocker::InUse { .. }
                | Blocker::AccessDenied
                | Blocker::Unreadable { .. }
                | Blocker::NotRecyclable { .. }
        )
    }

    pub fn label(&self) -> String {
        use crate::tr;
        match self {
            Blocker::ReservedName { component } => {
                tr_format!("reserved name {component}", "зарезервированное имя {component}")
            }
            Blocker::LongPath { length } => {
                tr_format!("long path ({length} chars)", "длинный путь ({length} симв.)")
            }
            Blocker::InUse { holders } => tr_format!(
                "open in {} process(es)",
                "занят процессами ({})",
                holders.len()
            ),
            Blocker::AccessDenied => tr!("access denied", "нет доступа").to_string(),
            Blocker::ReadOnly => tr!("read-only", "только чтение").to_string(),
            Blocker::System => tr!("system attribute", "системный атрибут").to_string(),
            Blocker::Hidden => tr!("hidden attribute", "скрытый атрибут").to_string(),
            Blocker::ReparsePoint { .. } => tr!("link or junction", "ссылка или junction").to_string(),
            Blocker::Unreadable { .. } => tr!("cannot be read", "не читается").to_string(),
            Blocker::NotRecyclable { reason } => reason.clone(),
        }
    }
}

/// Всё, что мы знаем об одном объекте в дереве.
#[derive(Clone, Debug, Serialize)]
pub struct Entry {
    pub path: PathBuf,
    #[serde(skip)]
    pub nt_path: NtPath,
    pub name: String,
    pub depth: usize,
    pub kind: EntryKind,
    pub size: u64,
    pub attributes: u32,
    pub reparse_tag: u32,
    pub blockers: Vec<Blocker>,
}

impl Entry {
    pub fn has_trouble(&self) -> bool {
        self.blockers.iter().any(Blocker::is_trouble)
    }

    pub fn is_recyclable(&self) -> bool {
        !self
            .blockers
            .iter()
            .any(|b| matches!(b, Blocker::NotRecyclable { .. }))
    }
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Предохранитель от бесконечного дерева (точнее — от очень глубокого).
    pub max_depth: usize,
    /// Ограничение на число объектов, чтобы GUI не завис на «syscall`-дереве».
    pub max_entries: usize,
    /// Проверять доступность удаления (открыть дескриптор на каждый объект).
    pub probe_access: bool,
    /// Искать держателей файлов через Restart Manager. Дорого: открывает
    /// сессию RM на каждый объект, поэтому включается только по запросу.
    pub probe_locks: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_depth: 256,
            max_entries: 200_000,
            probe_access: true,
            probe_locks: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanSummary {
    pub files: usize,
    pub directories: usize,
    pub links: usize,
    pub bytes: u64,
    pub troubled: usize,
    /// Обход остановился: упёрся в лимит или в нечитаемый каталог.
    pub truncated: bool,
}

/// Рекурсивный обход без рекурсии в стеке: глубина в Windows ограничена
/// только стеком, а нам не хочется его исчерпывать на пути в 200 компонентов.
pub fn scan(root: &NtPath, options: &ScanOptions) -> (Vec<Entry>, ScanSummary) {
    // Цель-файл или цель-ссылка: каталога нет, читать нечего. Раньше такая
    // цель читалась как каталог и выглядела «нечитаемой папкой» из нуля
    // объектов, поэтому удаление одиночного файла молча не запускалось.
    if let Some((attributes, size)) = root_leaf(root) {
        return scan_leaf(root, attributes, size, options);
    }
    let mut entries = Vec::new();
    let mut summary = ScanSummary::default();
    let mut stack = vec![(root.clone(), 0usize)];

    while let Some((dir, depth)) = stack.pop() {
        if depth > options.max_depth {
            summary.truncated = true;
            continue;
        }
        let children = match dir::children(&dir) {
            Ok(children) => children,
            Err(error) if error.not_found() => {
                summary.truncated = true;
                continue;
            }
            Err(error) => {
                // Каталог есть, а прочитать его не дали: показываем это
                // отдельной записью, иначе папка выглядит пустой и «здоровой».
                summary.truncated = true;
                entries.push(unreadable(dir.clone(), depth, error.code()));
                continue;
            }
        };
        for child in children {
            let path = dir.child_raw(&child.name);
            let is_dir = matches!(classify(child.attributes), EntryKind::Directory);
            let entry = make_entry(
                path.clone(),
                String::from_utf16_lossy(&child.name),
                depth,
                child.attributes,
                child.size,
                options,
            );
            match entry.kind {
                EntryKind::File => summary.files += 1,
                EntryKind::Directory => summary.directories += 1,
                EntryKind::Link => summary.links += 1,
            }
            summary.bytes += entry.size;
            if entry.has_trouble() {
                summary.troubled += 1;
            }
            entries.push(entry);

            if entries.len() >= options.max_entries {
                summary.truncated = true;
                break;
            }
            // Каталоги кладём в стек только если они настоящие: в link не идём.
            if is_dir {
                stack.push((path, depth + 1));
            }
        }
        if entries.len() >= options.max_entries {
            break;
        }
    }

    (entries, summary)
}

/// Запись об одном объекте со всеми пометками. Одна и та же для детей
/// каталога и для цели-файла, чтобы они не расходились.
fn make_entry(
    path: NtPath,
    name: String,
    depth: usize,
    attributes: u32,
    size: u64,
    options: &ScanOptions,
) -> Entry {
    let kind = classify(attributes);
    let size = if matches!(kind, EntryKind::File) { size } else { 0 };
    let mut blockers = Vec::new();
    if let Some(component) = reserved_component(&path) {
        blockers.push(Blocker::ReservedName { component });
    }
    if path.plain_len() >= LONG_PATH_WARN {
        blockers.push(Blocker::LongPath {
            length: path.plain_len(),
        });
    }
    if attributes & FILE_ATTRIBUTE_READONLY.0 != 0 {
        blockers.push(Blocker::ReadOnly);
    }
    if attributes & FILE_ATTRIBUTE_SYSTEM.0 != 0 {
        blockers.push(Blocker::System);
    }
    if attributes & FILE_ATTRIBUTE_HIDDEN.0 != 0 {
        blockers.push(Blocker::Hidden);
    }
    if matches!(kind, EntryKind::Link) {
        blockers.push(Blocker::ReparsePoint { tag: 0 });
    }
    probe(
        Entry {
            nt_path: path.clone(),
            path: path.to_path(),
            name,
            depth,
            kind,
            size,
            attributes,
            reparse_tag: 0,
            blockers,
        },
        options,
    )
}

/// Атрибуты и размер цели, если это файл или ссылка. У каталога обход
/// нужен, поэтому `None`; `None` и когда цель не открылась — обход сам
/// доложит «нет такого пути» или отказ в доступе.
fn root_leaf(root: &NtPath) -> Option<(u32, u64)> {
    use windows::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandleEx, FILE_STANDARD_INFO,
    };
    let handle = open_for_read(root).ok()?;
    let attrs = attributes(&handle).ok()?;
    if classify(attrs.attributes) == EntryKind::Directory {
        return None;
    }
    let mut info = FILE_STANDARD_INFO::default();
    let sized = unsafe {
        GetFileInformationByHandleEx(
            handle.raw(),
            FileStandardInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    }
    .is_ok();
    Some((attrs.attributes, if sized { info.EndOfFile as u64 } else { 0 }))
}

/// Цель — один файл или одна ссылка: в плане ровно один объект.
fn scan_leaf(root: &NtPath, attributes: u32, size: u64, options: &ScanOptions) -> (Vec<Entry>, ScanSummary) {
    let name = root
        .to_path()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy());
    let entry = make_entry(root.clone(), name, 0, attributes, size, options);
    let mut summary = ScanSummary::default();
    match entry.kind {
        EntryKind::File => summary.files = 1,
        EntryKind::Directory => summary.directories = 1,
        EntryKind::Link => summary.links = 1,
    }
    summary.bytes = entry.size;
    summary.troubled = usize::from(entry.has_trouble());
    (vec![entry], summary)
}

/// Достраивает запись реальными атрибутами из дескриптора и, если попросили,
/// проверяет доступ и держателей.
fn probe(entry: Entry, options: &ScanOptions) -> Entry {
    let mut entry = entry;
    if let Ok(handle) = open_for_delete(&entry.nt_path) {
        if let Ok(attrs) = attributes(&handle) {
            entry.attributes = attrs.attributes;
            entry.reparse_tag = attrs.reparse_tag;
            if matches!(entry.kind, EntryKind::Link) {
                entry.blockers.retain(|b| !matches!(b, Blocker::ReparsePoint { .. }));
                entry.blockers.push(Blocker::ReparsePoint {
                    tag: attrs.reparse_tag,
                });
            }
        }
    } else if let Err(error) = open_for_delete(&entry.nt_path) {
        if error.access_denied() {
            entry.blockers.push(Blocker::AccessDenied);
        } else if !error.not_found() {
            entry.blockers.push(Blocker::Unreadable { code: error.code() });
        }
    }

    if options.probe_locks && matches!(entry.kind, EntryKind::File | EntryKind::Link) {
        let holders = locker::processes_using(&[entry.nt_path.to_path()]);
        if !holders.is_empty() {
            entry.blockers.push(Blocker::InUse { holders });
        }
    }
    entry
}

/// Заглушка для каталога, который не удалось прочитать: без неё пользователь
/// не увидит, что «папка пуста», и решит, что всё в порядке.
fn unreadable(dir: NtPath, depth: usize, code: u32) -> Entry {
    Entry {
        path: dir.to_path(),
        name: dir.to_string_lossy(),
        nt_path: dir,
        depth,
        kind: EntryKind::Directory,
        size: 0,
        attributes: 0,
        reparse_tag: 0,
        blockers: vec![Blocker::Unreadable { code }],
    }
}

pub fn classify(attributes: u32) -> EntryKind {
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        EntryKind::Link
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        EntryKind::Directory
    } else {
        EntryKind::File
    }
}

fn reserved_component(path: &NtPath) -> Option<String> {
    let skip = extended_root_len(path);
    path.ext()
        .as_slice()[skip..]
        .split(|c| *c == b'\\' as u16)
        .filter(|c| !c.is_empty())
        .map(|c| c.to_vec())
        .find(|c| is_reserved_component_wide(c))
        .map(|c| String::from_utf16_lossy(&c))
}

fn extended_root_len(path: &NtPath) -> usize {
    let text = path.ext().to_string_lossy();
    if text.starts_with("\\\\?\\UNC\\") {
        // \\?\UNC\server\share — корень заканчивается после имени шары.
        let rest = &text[8..];
        match rest.find('\\') {
            Some(pos) => 8 + pos + 1,
            None => text.len(),
        }
    } else {
        4.min(text.len())
    }
}

/// `cFileName` — буфер фиксированной длины без NUL в начале, но с NUL в
/// конце; имя заканчивается на первом нуле.
#[cfg(test)]
#[allow(dead_code)]
fn find_data_name(data: &windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW) -> &[u16] {
    let end = data
        .cFileName
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(data.cFileName.len());
    &data.cFileName[..end]
}

/// Быстрая проверка одного пути без полного обхода — для контекстного меню и
/// для проверки «а что вообще выбрано».
pub fn probe_single(path: &NtPath, options: &ScanOptions) -> Option<Entry> {
    let data = match attributes(&open_for_delete(path).ok()?) {
        Ok(attrs) => attrs,
        Err(_) => Attributes::default(),
    };
    let name = path
        .to_string_lossy()
        .rsplit('\\')
        .next()
        .unwrap_or_default()
        .to_string();
    let mut blockers = Vec::new();
    if let Some(component) = path.reserved_component() {
        blockers.push(Blocker::ReservedName { component });
    }
    if path.plain_len() >= LONG_PATH_WARN {
        blockers.push(Blocker::LongPath {
            length: path.plain_len(),
        });
    }
    let entry = Entry {
        path: path.to_path(),
        nt_path: path.clone(),
        name,
        depth: 0,
        kind: classify(data.attributes),
        size: 0,
        attributes: data.attributes,
        reparse_tag: data.reparse_tag,
        blockers,
    };
    Some(probe(entry, options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::path::Wide;
    use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;

    #[test]
    fn classification_prefers_link_over_directory() {
        // Junction помечен и как каталог, и как reparse-точка. Удалять надо
        // сам junction, поэтому Link важнее.
        let attrs = FILE_ATTRIBUTE_REPARSE_POINT.0 | FILE_ATTRIBUTE_DIRECTORY.0;
        assert_eq!(classify(attrs), EntryKind::Link);
        assert_eq!(classify(FILE_ATTRIBUTE_DIRECTORY.0), EntryKind::Directory);
        assert_eq!(classify(0), EntryKind::File);
    }

    #[test]
    fn only_real_trouble_counts_as_trouble() {
        assert!(Blocker::ReservedName {
            component: "NUL".into()
        }
        .is_trouble());
        assert!(Blocker::InUse { holders: vec![] }.is_trouble());
        assert!(!Blocker::ReadOnly.is_trouble());
        assert!(!Blocker::Hidden.is_trouble());
        assert!(!Blocker::ReparsePoint { tag: 0 }.is_trouble());
    }

    #[test]
    fn labels_are_localized() {
        use crate::i18n::{Language, with_language};
        for language in [Language::En, Language::Ru] {
            with_language(language, || {
                let label = Blocker::ReservedName {
                    component: "NUL".into(),
                }
                .label();
                assert!(label.contains("NUL"));
                if language == Language::Ru {
                    assert!(label.contains("зарезервированное"));
                } else {
                    assert!(label.contains("reserved"));
                }
            });
        }
    }

    #[test]
    fn a_single_file_target_is_one_file_not_an_unreadable_folder() {
        let dir = std::env::temp_dir().join(format!("raze-scan-leaf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("one.txt");
        std::fs::write(&file, b"hello").unwrap();

        let (entries, summary) = scan(&NtPath::new(&file).unwrap(), &ScanOptions::default());
        assert_eq!((summary.files, summary.directories, summary.links), (1, 0, 0));
        assert_eq!(summary.bytes, 5);
        assert!(!summary.truncated);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "one.txt");
        assert!(!entries[0]
            .blockers
            .iter()
            .any(|b| matches!(b, Blocker::Unreadable { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_data_name_stops_at_nul() {
        let mut data = WIN32_FIND_DATAW::default();
        data.cFileName[0] = b'N' as u16;
        data.cFileName[1] = b'U' as u16;
        data.cFileName[2] = b'L' as u16;
        assert_eq!(find_data_name(&data), &[b'N' as u16, b'U' as u16, b'L' as u16]);
    }

    #[test]
    fn scan_finds_a_file_named_nul_and_removes_it() {
        // Файл с именем NUL нельзя создать обычным API: система превращает
        // такое имя в устройство. Создаём напрямую через расширенный путь —
        // и проверяем весь путь целиком: обход видит объект, помечает его
        // проблемным, а удаление действительно сносит.
        use windows::Win32::Storage::FileSystem::{
            CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
        };

        let base = std::env::temp_dir().join(format!(
            "raze-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let root = NtPath::new(&base).unwrap();

        let nul = root.child_raw(Wide::new("NUL").as_slice());
        let created = unsafe {
            windows::Win32::Storage::FileSystem::CreateFileW(
                nul.ext().pc(),
                super::super::handle::delete_access().0,
                windows::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows::Win32::Storage::FileSystem::FILE_SHARE_WRITE
                    | windows::Win32::Storage::FileSystem::FILE_SHARE_DELETE,
                None,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        };
        assert!(created.is_ok(), "расширенный путь обязан обойти разбор имён");

        let (entries, summary) = scan(&root, &ScanOptions::default());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"NUL"), "обход обязан увидеть NUL: {names:?}");
        assert!(summary.troubled >= 1);
        let entry = entries.iter().find(|e| e.name == "NUL").expect("entry exists");
        assert!(
            entry
                .blockers
                .iter()
                .any(|b| matches!(b, Blocker::ReservedName { .. })),
            "NUL обязан быть помечен как зарезервированное имя"
        );

        let outcome = super::super::remove::remove_tree(&nul, &Default::default());
        assert!(outcome.removed_files >= 1, "удаление NUL обязано сработать");
        let (_after, summary_after) = scan(&root, &ScanOptions::default());
        assert_eq!(summary_after.files, 0, "после удаления файлов быть не должно");

        let _ = std::fs::remove_dir_all(&base);
    }
}