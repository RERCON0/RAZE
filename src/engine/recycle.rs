//! Корзина: когда её можно использовать, а когда объект уйдёт безвозвратно.
//!
//! Важное ограничение, о котором стоит говорить прямо: если Корзина
//! переполнена или том её не поддерживает, оболочка **молча** удалит файл
//! окончательно. Поэтому RAZE решает режим сам, до удаления, и говорит
//! пользователю, что именно будет сделано, — вместо того чтобы обнаружить
//! это постфактум.

use crate::tr_format;
use serde::Serialize;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS, GetDriveTypeW,
    GetVolumeInformationW,
};
use windows::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE};
use windows::Win32::UI::Shell::{
    FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOCONFIRMMKDIR, FOF_NOERRORUI, FOF_SILENT, FO_DELETE,
    SHFileOperationW, SHFILEOPSTRUCTW,
};

use super::error::NtError;
use super::handle::open_with;
use super::path::{MAX_PATH, NtPath, Wide};

/// Файловые системы, где Корзина работает штатно.
const RECYCLABLE_FILESYSTEMS: [&str; 5] = ["NTFS", "REFS", "FAT", "FAT32", "EXFAT"];

/// Почему объект нельзя отправить в Корзину. Пустое поле — можно.
#[derive(Clone, Debug, Default, Serialize)]
pub struct RecycleVerdict {
    pub reusable: bool,
    pub reason: String,
}

pub fn is_recyclable(path: &NtPath) -> RecycleVerdict {
    use crate::tr;

    if let Some(component) = path.reserved_component() {
        return RecycleVerdict {
            reusable: false,
            reason: tr_format!(
                "reserved name {component}: the Recycle Bin cannot address it",
                "зарезервированное имя {component}: Корзина к нему не обращается"
            ),
        };
    }
    if path.plain_len() >= MAX_PATH {
        return RecycleVerdict {
            reusable: false,
            reason: tr_format!(
                "path is {} chars, the shell limit is {MAX_PATH}",
                "путь в {} симв., лимит оболочки — {MAX_PATH}",
                path.plain_len()
            ),
        };
    }
    if path.to_string_lossy().starts_with("\\\\") {
        return RecycleVerdict {
            reusable: false,
            reason: tr!(
                "network paths have no Recycle Bin",
                "для сетевых путей Корзины нет"
            )
            .to_string(),
        };
    }
    if let Ok(handle) = open_with(path, FILE_READ_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS) {
        let attrs = super::handle::attributes(&handle).map(|a| a.attributes).unwrap_or(0);
        if attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return RecycleVerdict {
                reusable: false,
                reason: tr!(
                    "links are removed as links, not through the Recycle Bin",
                    "ссылки удаляются как ссылки, мимо Корзины"
                )
                .to_string(),
            };
        }
    }
    match volume_filesystem(path) {
        Ok(name) if RECYCLABLE_FILESYSTEMS.iter().any(|f| f.eq_ignore_ascii_case(&name)) => {}
        Ok(name) => {
            return RecycleVerdict {
                reusable: false,
                reason: tr_format!("{name} has no Recycle Bin", "у {name} нет Корзины"),
            }
        }
        Err(error) => {
            return RecycleVerdict {
                reusable: false,
                reason: tr_format!(
                    "volume type is unknown ({})",
                    "тип тома неизвестен ({})",
                    error.code()
                ),
            }
        }
    }
    if !matches!(drive_type(path), Ok(t) if t == DRIVE_FIXED || t == DRIVE_REMOVABLE) {
        return RecycleVerdict {
            reusable: false,
            reason: tr!(
                "only local disks can use the Recycle Bin",
                "Корзина доступна только на локальных дисках"
            )
            .to_string(),
        };
    }
    RecycleVerdict {
        reusable: true,
        reason: String::new(),
    }
}

/// Отправляет пути в Корзину. Возвращает `Ok(false)`, если оболочка отказала
/// или часть операций прервалась, — тогда вызывающий переходит к
/// безвозвратному удалению.
pub fn send_to_recycle(paths: &[std::path::PathBuf]) -> Result<(), NtError> {
    if paths.is_empty() {
        return Ok(());
    }
    let wide: Vec<Wide> = paths.iter().map(|p| Wide::from_path(p)).collect();
    let list = Wide::nul_terminated_list(&wide);
    let mut operation = SHFILEOPSTRUCTW {
        hwnd: windows::Win32::Foundation::HWND::default(),
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(list.as_ptr()),
        pTo: windows::core::PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO
            | FOF_NOCONFIRMATION
            | FOF_NOCONFIRMMKDIR
            | FOF_NOERRORUI
            | FOF_SILENT)
            .0 as u16,
        fAnyOperationsAborted: windows::core::BOOL(0),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: windows::core::PCWSTR::null(),
    };
    let result = unsafe { SHFileOperationW(&mut operation) };
    if result != 0 {
        return Err(NtError::last());
    }
    if operation.fAnyOperationsAborted.as_bool() {
        return Err(NtError(ERROR_ABORTED));
    }
    Ok(())
}

const ERROR_ABORTED: u32 = 1223;

fn volume_filesystem(path: &NtPath) -> Result<String, NtError> {
    let root = Wide::new(&format!("{}\\", path.root()));
    let mut buffer = [0u16; 64];
    // В 0.62 обёртка возвращает Result без длины: имя файловой системы
    // NUL-терминированное, поэтому длину находим сами.
    unsafe {
        GetVolumeInformationW(root.pc(), None, None, None, None, Some(&mut buffer))
            .map_err(|_| NtError::last())?;
    }
    let end = buffer
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..end]))
}

fn drive_type(path: &NtPath) -> Result<u32, NtError> {
    let root = Wide::new(&format!("{}\\", path.root()));
    let result = unsafe { GetDriveTypeW(root.pc()) };
    if result == 0 {
        Err(NtError::last())
    } else {
        Ok(result)
    }
}

/// Что RAZE сделала с объектом: убрала в Корзину или стёрла безвозвратно.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    RecycleBin,
    Permanent,
}

/// Отправляет в Корзину то, что туда положить можно, и безвозвратно удаляет
/// всё остальное. Ошибка Корзины не проваливает задачу: для проблемного
/// файла безвозвратное удаление и есть цель.
pub fn remove(
    path: &NtPath,
    options: &super::remove::RemoveOptions,
    prefer_recycle: bool,
) -> (Disposition, Option<super::remove::Outcome>) {
    let verdict = is_recyclable(path);
    if prefer_recycle && verdict.reusable && !options.dry_run {
        match send_to_recycle(&[path.to_path()]) {
            Ok(()) => return (Disposition::RecycleBin, None),
            Err(_) => {
                // Корзина отказала — падаем в безвозвратное удаление, но
                // причину отказа покажем в отчёте.
            }
        }
    }
    (
        Disposition::Permanent,
        Some(super::remove::remove_tree(path, options)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "raze-recycle-{}-{}-{name}",
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
    fn local_temp_is_recyclable() {
        let base = scratch("local");
        let file = base.join("note.txt");
        std::fs::write(&file, b"x").unwrap();
        let verdict = is_recyclable(&NtPath::new(&file).unwrap());
        assert!(verdict.reusable, "{}", verdict.reason);

        send_to_recycle(std::slice::from_ref(&file)).expect("корзина обязана принять обычный файл");
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reserved_names_are_never_recyclable() {
        let base = scratch("nul");
        let path = base.join("NUL");
        let nt_path = NtPath::new(&path).unwrap();
        let verdict = is_recyclable(&nt_path);
        assert!(!verdict.reusable);
        assert!(verdict.reason.contains("NUL"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_list_is_a_no_op() {
        assert!(send_to_recycle(&[]).is_ok());
    }
}