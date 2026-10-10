//! Предохранители: что RAZE откажется удалять даже с `--force`.
//!
//! Удалялка — это программа, которая по ошибке человека сносит то, что
//! восстановить нельзя. Поэтому список «нельзя» задан жёстко и не зависит от
//! флагов: корни томов, системные каталоги и профиль пользователя.
//! Всё, что не попало в список, удаляется — но с показом того, что именно.

use std::path::{Path, PathBuf};

use crate::engine::path::NtPath;
use crate::tr_format;

/// Почему цель отклонена.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Корень тома: `C:\`, `\\server\share`.
    VolumeRoot,
    /// Системный каталог или его родитель (`C:\Windows`, `C:\Program Files`).
    SystemLocation { anchor: String },
    /// Домашний каталог или профиль целиком.
    UserProfile { anchor: String },
    /// Каталог самой программы.
    ProgramDirectory,
    /// Путь не является абсолютным — значит, мы не знаем, что именно удалим.
    NotAbsolute,
}

impl Reason {
    pub fn message(&self) -> String {
        use crate::tr;
        match self {
            Reason::VolumeRoot => tr!(
                "this is a volume root — removing it would erase the whole disk",
                "это корень тома — удаление снесёт весь диск"
            )
            .to_string(),
            Reason::SystemLocation { anchor } => tr_format!(
                "{anchor} is a system location and is on the deny list",
                "{anchor} — системный каталог из списка запрещённых"
            ),
            Reason::UserProfile { anchor } => tr_format!(
                "{anchor} holds the user profile",
                "{anchor} содержит профиль пользователя"
            ),
            Reason::ProgramDirectory => tr!(
                "this is where RAZE itself lives",
                "это каталог самой программы RAZE"
            )
            .to_string(),
            Reason::NotAbsolute => tr!(
                "path is not absolute, so its target is unknown",
                "путь не абсолютный — непонятно, что именно удалять"
            )
            .to_string(),
        }
    }
}

/// `Ok(())`, если удалять можно; иначе — с причиной.
///
/// Путь проверяется дважды: как его ввели и как он на самом деле лежит на
/// диске. Одной лексической проверки мало: `C:\PROGRA~1` — это `Program
/// Files`, а `C:\Users\me\link\System32` при `link` → `C:\Windows` — это
/// `System32`, и ни то ни другое по тексту на запретное не похоже.
pub fn check(path: &NtPath) -> Result<(), Reason> {
    if path.is_volume_root() {
        return Err(Reason::VolumeRoot);
    }
    // `NtPath::new` уже отсекает device-пространство (`\\.\`), потоки NTFS и
    // маски, поэтому здесь остаётся только проверка самих якорей.

    let given = path.to_path();
    check_location(&given)?;

    if let Some(real) = canonical_path(path) {
        if components_of(&real).len() <= 2 {
            return Err(Reason::VolumeRoot);
        }
        if components_of(&real) != components_of(&given) {
            check_location(&real)?;
        }
    }
    Ok(())
}

/// Путь так, как он лежит на диске: короткие имена и регистр приведены к
/// настоящим, junction и symlink в родительских каталогах раскрыты. Сам
/// последний компонент не раскрывается: ссылка удаляется как ссылка, и то, что
/// она показывает, защищать от этого не нужно. `None` — объекта нет или он не
/// открывается; тогда остаётся лексическая проверка.
fn canonical_path(path: &NtPath) -> Option<PathBuf> {
    use windows::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

    use crate::engine::handle::{final_path, object_flags, open_with};

    let handle = open_with(path, FILE_READ_ATTRIBUTES, object_flags()).ok()?;
    let text = String::from_utf16_lossy(&final_path(&handle)?);
    Some(PathBuf::from(if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{rest}")
    } else if let Some(rest) = text.strip_prefix("\\\\?\\") {
        rest.to_string()
    } else {
        text
    }))
}

/// Проверка одной записи пути против якорей и защищённых поддеревьев.
fn check_location(target: &Path) -> Result<(), Reason> {
    let program_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));
    if let Some(dir) = program_dir {
        if anchor_forms(&dir).iter().any(|form| would_take_anchor(target, form)) {
            return Err(Reason::ProgramDirectory);
        }
    }

    for anchor in system_anchors() {
        if anchor_forms(&anchor).iter().any(|form| would_take_anchor(target, form)) {
            let label = anchor.to_string_lossy().to_string();
            let system = SYSTEM_MARKERS
                .iter()
                .any(|marker| label.to_lowercase().contains(marker));
            return Err(if system {
                Reason::SystemLocation { anchor: label }
            } else {
                Reason::UserProfile { anchor: label }
            });
        }
    }

    if let Some(anchor) = protected_interior(target) {
        return Err(Reason::SystemLocation { anchor });
    }
    Ok(())
}

/// Якорь в том виде, как его вводят, и в настоящем: `UserProfile` может быть
/// записан коротким именем (`C:\Users\LONGUS~1`).
fn anchor_forms(anchor: &Path) -> Vec<PathBuf> {
    let mut forms = vec![anchor.to_path_buf()];
    if let Some(real) = NtPath::new(anchor).ok().and_then(|nt| canonical_path(&nt)) {
        if components_of(&real) != components_of(anchor) {
            forms.push(real);
        }
    }
    forms
}

/// Подкаталоги `%SystemRoot%`, которые удалять можно: это мусор, а не система.
/// Всё остальное внутри Windows — нельзя: `System32` или `WinSxS` сносятся
/// одним кликом в меню, а восстановить их нечем.
const WINDOWS_DISPOSABLE: [&[&str]; 10] = [
    &["temp"],
    &["logs"],
    &["minidump"],
    &["livekernelreports"],
    &["softwaredistribution"],
    &["panther"],
    &["debug"],
    &["prefetch"],
    // Зависшая очередь печати — классика «файл не удаляется».
    &["system32", "spool", "printers"],
    &["system32", "logfiles"],
];

/// Папки в корне системного диска, которые принадлежат системе целиком.
/// `$Recycle.Bin` здесь нет: сломанную Корзину как раз и сносят.
const SYSTEM_DRIVE_FOLDERS: [&str; 4] = ["system volume information", "recovery", "boot", "efi"];

/// `Some(якорь)`, если `target` лежит внутри системного каталога, который
/// удалять нельзя, хотя сам по себе он ни предком, ни самим якорем не является.
fn protected_interior(target: &Path) -> Option<String> {
    let parts = components_of(target);

    // 1. Внутри Windows можно только список выше.
    for variable in ["SystemRoot", "windir"] {
        let Ok(value) = std::env::var(variable) else { continue };
        for base in anchor_forms(Path::new(&value)) {
            if let Some(rest) = inside(&parts, &components_of(&base)) {
                let disposable = WINDOWS_DISPOSABLE
                    .iter()
                    .any(|allowed| rest.len() >= allowed.len() && rest.iter().zip(allowed.iter()).all(|(a, b)| a.as_str() == *b));
                if !disposable {
                    return Some(base.to_string_lossy().to_string());
                }
            }
        }
    }

    // 2. Program Files: приложения сносить можно, а то, что принадлежит самой
    //    Windows (Store-приложения, Defender, общие библиотеки), — нет.
    for variable in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        let Ok(value) = std::env::var(variable) else { continue };
        for base in anchor_forms(Path::new(&value)) {
            if let Some(rest) = inside(&parts, &components_of(&base)) {
                let first = rest[0].as_str();
                if first.starts_with("windows") || first == "common files" || first == "modifiablewindowsapps" {
                    return Some(base.join(first).to_string_lossy().to_string());
                }
            }
        }
    }

    // 3. Служебные папки в корне системного диска.
    if let Ok(drive) = std::env::var("SystemDrive") {
        let root = components_of(Path::new(&format!("{drive}\\")));
        if let Some(rest) = inside(&parts, &root) {
            if SYSTEM_DRIVE_FOLDERS.contains(&rest[0].as_str()) {
                return Some(format!("{drive}\\{}", rest[0]));
            }
        }
    }
    None
}

/// Компоненты `target` после `base`, если `target` строго внутри `base`.
fn inside<'a>(target: &'a [String], base: &[String]) -> Option<&'a [String]> {
    if target.len() > base.len() && target.starts_with(base) {
        Some(&target[base.len()..])
    } else {
        None
    }
}

/// Части пути, по которым видно, что каталог системный.
const SYSTEM_MARKERS: [&str; 6] = [
    "\\windows",
    "\\program files",
    "\\program files (x86)",
    "\\programdata",
    "\\system volume information",
    "\\$recycle.bin",
];

fn system_anchors() -> Vec<PathBuf> {
    let mut anchors = Vec::new();
    for variable in [
        "SystemRoot",
        "windir",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
        "UserProfile",
        "LOCALAPPDATA",
        "APPDATA",
        "TEMP",
    ] {
        if let Ok(value) = std::env::var(variable) {
            if !value.is_empty() {
                anchors.push(PathBuf::from(value));
            }
        }
    }
    anchors
}

/// `true`, если удаление `target` унесло бы `anchor` вместе с собой —
/// то есть `target` это `anchor` или один из его предков. Сравнение
/// регистронезависимое, как это делает сама Windows, и **по компонентам**:
/// `C:\ab` не является предком `C:\a`.
pub fn would_take_anchor(target: &Path, anchor: &Path) -> bool {
    let target_parts = components_of(target);
    let anchor_parts = components_of(anchor);
    if target_parts.len() > anchor_parts.len() {
        return false;
    }
    anchor_parts.starts_with(&target_parts)
}

fn components_of(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| normalize(c.as_os_str().to_string_lossy().as_ref()))
        .collect()
}

fn normalize(component: &str) -> String {
    component.to_lowercase().trim_end_matches(['\\', '/']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_roots_are_always_refused() {
        assert_eq!(
            check(&NtPath::new(Path::new("C:\\")).unwrap()).unwrap_err(),
            Reason::VolumeRoot
        );
    }

    #[test]
    fn windows_directory_and_its_ancestors_are_refused() {
        let windows = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let windows = PathBuf::from(windows);
        assert!(check(&NtPath::new(&windows).unwrap()).is_err());
        // Родитель системного каталога тоже под запретом.
        assert!(check(&NtPath::new(Path::new("C:\\")).unwrap()).is_err());
        // А вот содержимое Windows — обычные цели.
        assert!(check(&NtPath::new(Path::new("C:\\Windows\\Temp\\x")).unwrap()).is_ok());
    }

    #[test]
    fn user_profile_root_is_refused_but_its_subfolders_are_not() {
        let profile = std::env::var("UserProfile").ok().map(PathBuf::from);
        let Some(profile) = profile else {
            return;
        };
        assert!(check(&NtPath::new(&profile).unwrap()).is_err());
        let desktop = profile.join("Desktop");
        assert!(
            check(&NtPath::new(&desktop).unwrap()).is_ok(),
            "то, что внутри профиля, удалять можно"
        );
    }

    #[test]
    fn rase_directory_is_protected() {
        let exe = std::env::current_exe().unwrap();
        let dir = exe.parent().unwrap().to_path_buf();
        assert_eq!(
            check(&NtPath::new(&dir).unwrap()).unwrap_err(),
            Reason::ProgramDirectory
        );
    }

    #[test]
    fn anchor_matching_respects_separators_and_direction() {
        // Цель — предок якоря: удаление унесёт якорь с собой.
        assert!(would_take_anchor(Path::new("C:\\a"), Path::new("C:\\a\\b")));
        assert!(would_take_anchor(Path::new("C:\\a"), Path::new("C:\\a")));
        // Цель — потомок якоря: якорь при этом остаётся целым.
        assert!(!would_take_anchor(Path::new("C:\\a\\b"), Path::new("C:\\a")));
        // Похожий по имени сосед — не предок.
        assert!(!would_take_anchor(Path::new("C:\\ab"), Path::new("C:\\a")));
        assert!(would_take_anchor(Path::new("C:\\A\\B"), Path::new("c:\\a\\b\\c")));
    }

    fn target(path: &str) -> NtPath {
        NtPath::new(Path::new(path)).unwrap()
    }

    fn windows_dir() -> String {
        std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())
    }

    #[test]
    fn the_inside_of_windows_is_protected_except_disposable_folders() {
        // Раньше защищался только сам `C:\Windows`, а `C:\Windows\System32`
        // принимался: один клик в меню без подтверждения.
        let windows = windows_dir();
        for protected in [
            "System32",
            "System32\\drivers",
            "SysWOW64",
            "WinSxS",
            "System32\\spool",
            "Fonts",
        ] {
            assert!(
                matches!(check(&target(&format!("{windows}\\{protected}"))), Err(Reason::SystemLocation { .. })),
                "{protected}"
            );
        }
        for disposable in [
            "Temp",
            "Temp\\x",
            "SoftwareDistribution\\Download",
            "System32\\spool\\PRINTERS",
            "System32\\spool\\PRINTERS\\00001.SPL",
            "Logs\\CBS",
        ] {
            assert!(check(&target(&format!("{windows}\\{disposable}"))).is_ok(), "{disposable}");
        }
        // Соседи по имени — не Windows.
        assert!(check(&target("C:\\Windows.old")).is_ok());
    }

    #[test]
    fn windows_owned_folders_in_program_files_are_protected() {
        let Ok(files) = std::env::var("ProgramFiles") else { return };
        for protected in ["WindowsApps", "Windows Defender\\x", "Common Files", "WindowsPowerShell"] {
            assert!(check(&target(&format!("{files}\\{protected}"))).is_err(), "{protected}");
        }
        // А вот оставшиеся от удалённой программы папки — как раз цель.
        assert!(check(&target(&format!("{files}\\SomeLeftoverApp"))).is_ok());
        assert!(check(&target(&format!("{files}\\SomeLeftoverApp\\bin"))).is_ok());
    }

    #[test]
    fn system_drive_service_folders_are_protected_but_a_broken_recycle_bin_is_not() {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        for protected in ["System Volume Information", "Recovery\\x", "Boot"] {
            assert!(check(&target(&format!("{drive}\\{protected}"))).is_err(), "{protected}");
        }
        assert!(check(&target(&format!("{drive}\\$Recycle.Bin"))).is_ok());
    }

    #[test]
    fn a_short_8_3_name_cannot_hide_a_protected_folder() {
        // `C:\PROGRA~1` — это `C:\Program Files`. Если коротких имён на диске
        // нет, проверять нечего.
        let alias = format!("{}\\PROGRA~1", std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()));
        if !Path::new(&alias).exists() {
            return;
        }
        assert!(check(&target(&alias)).is_err());
    }

    #[test]
    fn a_junction_in_the_parents_cannot_hide_a_protected_folder() {
        let base = std::env::temp_dir().join(format!("raze-protect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let link = base.join("link");
        let created = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(windows_dir())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if created {
            // По тексту это обычный путь во временной папке, по сути — System32.
            assert!(check(&NtPath::new(&link.join("System32")).unwrap()).is_err());
            // Сама ссылка удаляется как ссылка: цель ей не принадлежит.
            assert!(check(&NtPath::new(&link).unwrap()).is_ok());
            // А то, что в Windows можно, можно и через ссылку.
            assert!(check(&NtPath::new(&link.join("Temp")).unwrap()).is_ok());
            // Ссылку снимаем сама по себе (rmdir не заходит в цель), и только
            // потом чистим каталог: рекурсия по каталогу с junction на Windows
            // — самый неудачный способ убирать за собой.
            let _ = std::fs::remove_dir(&link);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn desktop_subfolder_is_allowed() {
        // Ровно тот случай из описания задачи: `C:\Users\<user>\Desktop\Seller`.
        let Ok(profile) = std::env::var("UserProfile") else {
            return;
        };
        let target = PathBuf::from(profile).join("Desktop").join("Seller");
        assert!(check(&NtPath::new(&target).unwrap()).is_ok());
    }
}