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
pub fn check(path: &NtPath) -> Result<(), Reason> {
    if path.is_volume_root() {
        return Err(Reason::VolumeRoot);
    }
    // `NtPath::new` уже отсекает device-пространство (`\\.\`), поэтому
    // здесь остаётся только проверка самих якорей.

    let target = path.to_path();
    let program_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));
    if let Some(dir) = program_dir {
        if would_take_anchor(&target, &dir) {
            return Err(Reason::ProgramDirectory);
        }
    }

    for anchor in system_anchors() {
        if would_take_anchor(&target, &anchor) {
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
    Ok(())
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