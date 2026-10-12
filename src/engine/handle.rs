//! Дескрипторы файлов и каталогов.
//!
//! Удаление всегда идёт через дескриптор, а не через повторный разбор пути:
//! между «проверили, что это тот самый файл» и «удалили» путь мог бы
//! подменить другой процесс. Дескриптор закрывает этот зазор.

use std::ops::{Deref, DerefMut};

use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileAttributeTagInfo, FileBasicInfo, GetFileInformationByHandleEx,
    SetFileInformationByHandle, DELETE, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_BASIC_INFO, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// Флаги, без которых нельзя ни удалить, ни безопасно открыть каталог:
/// `BACKUP_SEMANTICS` разрешает открыть папку, `OPEN_REPARSE_POINT` не даёт
/// провалиться внутрь junction.
pub fn object_flags() -> FILE_FLAGS_AND_ATTRIBUTES {
    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
}

use super::error::{nt, NtError};
use super::path::NtPath;

/// Права, достаточные для удаления и для чтения атрибутов.
pub fn delete_access() -> FILE_ACCESS_RIGHTS {
    DELETE | FILE_READ_ATTRIBUTES
}

/// Открывает объект для удаления. `open_reparse` обязателен: без него
/// junction откроется как каталог и обход уйдёт внутрь чужого дерева.
pub fn open_for_delete(path: &NtPath) -> Result<OwnedHandle, NtError> {
    open_with(path, delete_access(), object_flags())
}

pub fn open_for_read(path: &NtPath) -> Result<OwnedHandle, NtError> {
    open_with(path, FILE_READ_ATTRIBUTES, object_flags())
}

pub fn open_with(
    path: &NtPath,
    access: FILE_ACCESS_RIGHTS,
    options: FILE_FLAGS_AND_ATTRIBUTES,
) -> Result<OwnedHandle, NtError> {
    nt(unsafe {
        CreateFileW(
            path.ext().pc(),
            access.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            options,
            None,
        )
    })
    .map(OwnedHandle)
}

/// Дескриптор, который закрывается сам.
pub struct OwnedHandle(HANDLE);

impl OwnedHandle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Deref for OwnedHandle {
    type Target = HANDLE;
    fn deref(&self) -> &HANDLE {
        &self.0
    }
}

impl DerefMut for OwnedHandle {
    fn deref_mut(&mut self) -> &mut HANDLE {
        &mut self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Атрибуты вместе с тегом reparse-точки — из одного вызова.
#[derive(Clone, Copy, Debug, Default)]
pub struct Attributes {
    pub attributes: u32,
    pub reparse_tag: u32,
}

impl Attributes {
    pub fn is_directory(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
    }

    pub fn is_reparse_point(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    }

    pub fn is_read_only(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_READONLY.0 != 0
    }
}

pub fn attributes(handle: &OwnedHandle) -> Result<Attributes, NtError> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    nt(unsafe {
        GetFileInformationByHandleEx(
            handle.raw(),
            FileAttributeTagInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    })?;
    Ok(Attributes {
        attributes: info.FileAttributes,
        reparse_tag: info.ReparseTag,
    })
}

/// Число жёстких ссылок объекта по открытому дескриптору. `None` — система не
/// ответила; вызывающий обязан трактовать такой объект как общий и не менять
/// его владельца, DACL или атрибуты: они принадлежат файлу, а не имени, и
/// правка задела бы все ссылки, включая лежащие вне цели.
pub fn hard_links(handle: &OwnedHandle) -> Option<u32> {
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle.raw(), &mut info) }.ok()?;
    Some(info.nNumberOfLinks)
}

/// Окончательный путь объекта по его дескриптору: промежуточные junction и
/// symlink раскрыты, 8.3-имена и регистр приведены к тем, что лежат на диске.
/// Дескриптор открыт с `OPEN_REPARSE_POINT`, поэтому у самой ссылки это путь
/// ссылки, а не её цели.
pub fn final_path(handle: &OwnedHandle) -> Option<Vec<u16>> {
    use windows::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_DOS};
    let mut buffer = vec![0u16; 1024];
    for _ in 0..2 {
        let written =
            unsafe { GetFinalPathNameByHandleW(handle.raw(), &mut buffer, VOLUME_NAME_DOS) }
                as usize;
        if written == 0 {
            return None;
        }
        if written < buffer.len() {
            buffer.truncate(written);
            return Some(buffer);
        }
        // Не поместилось: функция вернула нужный размер вместе с NUL.
        buffer = vec![0u16; written + 1];
    }
    None
}

/// Корень удаляемого дерева, как он лежит на диске. Каждый объект, который
/// собираются удалить, должен лежать внутри него — иначе между обходом и
/// удалением каталог подменили на junction, и удаление ушло бы в чужое дерево.
/// Проверка идёт по дескриптору, который затем и удаляется, поэтому между
/// «проверили» и «удалили» подменить уже нечего.
#[derive(Clone, Debug)]
pub struct Containment {
    root: Vec<u16>,
}

impl Containment {
    /// Если окончательный путь недоступен, обход запрещён: без него нельзя
    /// проверить, что удаляемый объект принадлежит выбранному дереву.
    pub fn new(root: &NtPath) -> Result<Self, NtError> {
        let handle = open_with(root, FILE_READ_ATTRIBUTES, object_flags())?;
        use std::os::windows::ffi::OsStringExt;
        let root = final_path(&handle).ok_or(NtError(super::error::ERROR_OUTSIDE_TREE))?;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&root));
        let resolved = NtPath::new(&path).map_err(|_| NtError(super::error::ERROR_OUTSIDE_TREE))?;
        crate::protect::check_resolved(&resolved)
            .map_err(|_| NtError(super::error::ERROR_OUTSIDE_TREE))?;
        Ok(Self { root })
    }

    pub fn holds(&self, handle: &OwnedHandle) -> bool {
        match final_path(handle) {
            Some(path) => self.contains(&path),
            None => false,
        }
    }

    /// Лежит ли `path` в корне или под ним. Граница — разделитель компонента:
    /// `\\?\C:\ab` не принадлежит дереву `\\?\C:\a`.
    fn contains(&self, path: &[u16]) -> bool {
        const BSLASH: u16 = b'\\' as u16;
        let root = self.root.as_slice();
        path.starts_with(root)
            && (path.len() == root.len()
                || path[root.len()] == BSLASH
                || root.last() == Some(&BSLASH))
    }
}

/// Снимает атрибут «только чтение» прямо через дескриптор. Обычный
/// `SetFileAttributesW` не работает с путями, которые не проходят разбор
/// имени, — то есть ровно с теми, ради которых существует RAZE.
pub fn clear_read_only(handle: &OwnedHandle) -> Result<(), NtError> {
    let current = attributes(handle)?;
    if !current.is_read_only() {
        return Ok(());
    }
    let writable = current.attributes & !FILE_ATTRIBUTE_READONLY.0;
    let info = FILE_BASIC_INFO {
        FileAttributes: if writable == 0 {
            windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL.0
        } else {
            writable
        },
        ..Default::default()
    };
    nt(unsafe {
        SetFileInformationByHandle(
            handle.raw(),
            FileBasicInfo,
            &info as *const _ as *const _,
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn containment_respects_component_boundaries() {
        let guard = Containment {
            root: wide(r"\\?\C:\a"),
        };
        assert!(guard.contains(&wide(r"\\?\C:\a")));
        assert!(guard.contains(&wide(r"\\?\C:\a\b\c")));
        assert!(
            !guard.contains(&wide(r"\\?\C:\ab")),
            "похожий сосед не принадлежит дереву"
        );
        assert!(!guard.contains(&wide(r"\\?\C:\")));
        assert!(!guard.contains(&wide(r"\\?\D:\a\b")));
    }

    #[test]
    fn protected_resolved_root_is_refused_before_traversal() {
        let windows = std::env::var_os("SystemRoot").unwrap();
        let system = std::path::PathBuf::from(windows).join("System32");
        let result = Containment::new(&NtPath::new(&system).unwrap());
        assert!(matches!(
            result,
            Err(NtError(super::super::error::ERROR_OUTSIDE_TREE))
        ));
    }

    #[test]
    fn final_path_resolves_a_real_object() {
        let dir = std::env::temp_dir();
        let handle = open_for_read(&NtPath::new(&dir).unwrap()).unwrap();
        let path = String::from_utf16_lossy(&final_path(&handle).expect("final path"));
        assert!(path.starts_with(r"\\?\"), "{path}");
    }
}
