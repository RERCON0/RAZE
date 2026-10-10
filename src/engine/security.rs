//! Права доступа: стать владельцем и выдать себе полный доступ.
//!
//! Порядок именно такой, и он важен: сначала `SeTakeOwnershipPrivilege`
//! позволяет открыть объект **только** на запись владельца, затем мы
//! становимся владельцем и только после этого получаем право менять DACL.
//! Пытаться сначала выдать права нельзя — их и не у кого взять.

use std::ptr;

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GetSecurityInfo, GRANT_ACCESS, SE_FILE_OBJECT,
    SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_ALIAS, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
    TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
};
use windows::core::PWSTR;

use super::error::NtError;
use super::handle::{OwnedHandle, open_with, security_access};
use super::path::{NtPath, Wide};
use super::privs;

/// Идентификаторы, которым всегда нужен доступ к файлу, который мы сносим.
const SID_SYSTEM: &str = "S-1-5-18";
const SID_ADMINISTRATORS: &str = "S-1-5-32-544";

/// Владелец ACL, который нужно освободить через `LocalFree`.
struct LocalAcl(*mut ACL);
impl Drop for LocalAcl {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.cast())));
        }
    }
}

/// SID, полученный из `ConvertStringSidToSidW`, с освобождением. Хранит и
/// указатель (для `TRUSTEE_W`), и копию байтов — длину SID можно узнать
/// только из его заголовка.
struct LocalSid(windows::Win32::Security::PSID, Vec<u8>);

impl LocalSid {
    fn from_string(text: &str) -> Result<Self, NtError> {
        let wide = Wide::new(text);
        let mut sid = windows::Win32::Security::PSID::default();
        unsafe { ConvertStringSidToSidW(wide.pc(), &mut sid) }.map_err(|_| NtError::last())?;
        // SID = { BYTE Revision; BYTE SubAuthorityCount; BYTE Authority[6];
        //        DWORD SubAuthority[SubAuthorityCount] }
        let header = unsafe { std::slice::from_raw_parts(sid.0.cast::<u8>(), 2) };
        let length = 8 + 4 * header[1] as usize;
        let copy = unsafe { std::slice::from_raw_parts(sid.0.cast::<u8>(), length) }.to_vec();
        Ok(Self(sid, copy))
    }

    fn as_slice(&self) -> &[u8] {
        &self.1
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0.cast())));
        }
    }
}

/// Перехватывает объект и назначает владельцем текущего пользователя.
pub fn take_ownership(path: &NtPath) -> Result<(), NtError> {
    let _ = privs::enable_take_ownership();
    let owner = privs::current_user_sid()?;
    // С привилегией достаточно права только на смену владельца; DELETE
    // запрашиваем сразу, чтобы сразу попробовать и удалить.
    let handle = open_with(
        path,
        windows::Win32::Storage::FileSystem::WRITE_OWNER
            | windows::Win32::Storage::FileSystem::DELETE
            | windows::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES,
        windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
            | windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    unsafe {
        GetSecurityInfo(
            handle.raw(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            None,
        )
        .ok()
        .map_err(|_| NtError::last())?;

        SetSecurityInfo(
            handle.raw(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(windows::Win32::Security::PSID(owner.as_ptr() as *mut core::ffi::c_void)),
            None,
            None,
            None,
        )
        .ok()
        .map_err(|_| NtError::last())
    }
}

/// Добавляет в DACL полный доступ для текущего пользователя, SYSTEM и
/// администраторов, **не трогая** уже существующие записи: файлы часто
/// наследуют права от родителя, и сбрасывать их целиком нельзя.
pub fn grant_full_control(path: &NtPath) -> Result<(), NtError> {
    let _ = privs::enable_take_ownership();
    let handle: OwnedHandle = open_with(path, security_access(), backup_semantics())?;

    // SID-ы живут до конца вызова: SetEntriesInAclW читает их по указателям
    // из TRUSTEE_W, поэтому освобождать раньше времени нельзя.
    let current = privs::current_user_sid()?;
    let system = LocalSid::from_string(SID_SYSTEM)?;
    let administrators = LocalSid::from_string(SID_ADMINISTRATORS)?;

    unsafe {
        let mut existing: *mut ACL = ptr::null_mut();
        GetSecurityInfo(
            handle.raw(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut existing),
            None,
            None,
        )
        .ok()
        .map_err(|_| NtError::last())?;

        let entries = [
            entry(current.as_slice(), TRUSTEE_IS_USER),
            entry(system.as_slice(), TRUSTEE_IS_WELL_KNOWN_GROUP),
            entry(administrators.as_slice(), TRUSTEE_IS_ALIAS),
        ];

        let mut merged: *mut ACL = ptr::null_mut();
        SetEntriesInAclW(Some(&entries), Some(existing), &mut merged)
            .ok()
            .map_err(|_| NtError::last())?;
        let merged = LocalAcl(merged);

        SetSecurityInfo(
            handle.raw(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(merged.0),
            None,
        )
        .ok()
        .map_err(|_| NtError::last())
    }
}

fn backup_semantics() -> windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES {
    windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
        | windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT
}

fn entry(
    sid: &[u8],
    trustee_type: windows::Win32::Security::Authorization::TRUSTEE_TYPE,
) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS.0,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: windows::Win32::Security::Authorization::NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: trustee_type,
            ptstrName: PWSTR(sid.as_ptr() as *mut u16),
        },
    }
}