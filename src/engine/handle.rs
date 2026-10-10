//! Дескрипторы файлов и каталогов.
//!
//! Удаление всегда идёт через дескриптор, а не через повторный разбор пути:
//! между «проверили, что это тот самый файл» и «удалили» путь мог бы
//! подменить другой процесс. Дескриптор закрывает этот зазор.

use std::ops::{Deref, DerefMut};

use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ACCESS_RIGHTS, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo,
    FileBasicInfo, FILE_FLAGS_AND_ATTRIBUTES, GetFileInformationByHandleEx, OPEN_EXISTING,
    READ_CONTROL, SetFileInformationByHandle, WRITE_DAC, WRITE_OWNER, FILE_ATTRIBUTE_TAG_INFO,
    FILE_BASIC_INFO,
};

/// Флаги, без которых нельзя ни удалить, ни безопасно открыть каталог:
/// `BACKUP_SEMANTICS` разрешает открыть папку, `OPEN_REPARSE_POINT` не даёт
/// провалиться внутрь junction.
pub fn object_flags() -> FILE_FLAGS_AND_ATTRIBUTES {
    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
}

use super::error::{NtError, nt};
use super::path::NtPath;

/// Права, достаточные для удаления и для чтения атрибутов.
pub fn delete_access() -> FILE_ACCESS_RIGHTS {
    DELETE | FILE_READ_ATTRIBUTES
}

/// Права для обхода защиты: с ними и привилетией `SeBackupPrivilege`
/// открывается почти всё, включая файлы, на которые у процесса нет прав.
pub fn privileged_access() -> FILE_ACCESS_RIGHTS {
    DELETE | FILE_READ_ATTRIBUTES | READ_CONTROL
}

/// Права, нужные чтобы стать владельцем и починить DACL.
pub fn security_access() -> FILE_ACCESS_RIGHTS {
    DELETE | FILE_READ_ATTRIBUTES | READ_CONTROL | WRITE_OWNER | WRITE_DAC
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
    nt(unsafe { CreateFileW(
        path.ext().pc(),
        access.0,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        None,
        OPEN_EXISTING,
        options,
        None,
    ) })
    .map(OwnedHandle)
}

/// Попытка открыть объект, не падая на отсутствии: `None` — значит «нет
/// такого пути», и это не ошибка удаления.
pub fn try_open(
    path: &NtPath,
    access: FILE_ACCESS_RIGHTS,
    options: FILE_FLAGS_AND_ATTRIBUTES,
) -> Option<OwnedHandle> {
    open_with(path, access, options).ok()
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

/// Снимает атрибут «только чтение» прямо через дескриптор. Обычный
/// `SetFileAttributesW` не работает с путями, которые не проходят разбор
/// имени, — то есть ровно с теми, ради которых существует RAZE.
pub fn clear_read_only(handle: &OwnedHandle) -> Result<(), NtError> {
    let current = attributes(handle)?;
    if !current.is_read_only() {
        return Ok(());
    }
    let info = FILE_BASIC_INFO {
        FileAttributes: current.attributes & !FILE_ATTRIBUTE_READONLY.0,
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

/// Читает время последней записи — используется для сортировки в GUI.
pub fn last_write_time(path: &NtPath) -> Option<std::time::SystemTime> {
    use windows::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let handle = try_open(
        path,
        FILE_READ_ATTRIBUTES,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle.raw(), &mut info) }.ok()?;
    let ft = info.ftLastWriteTime;
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(
        (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64,
    ))
}