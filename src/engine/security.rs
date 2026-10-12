//! Права доступа: стать владельцем и выдать себе полный доступ.
//!
//! Порядок именно такой, и он важен: сначала `SeTakeOwnershipPrivilege`
//! позволяет открыть объект **только** на запись владельца, затем мы
//! становимся владельцем и только после этого получаем право менять DACL.
//! Пытаться сначала выдать права нельзя — их и не у кого взять.

use std::ptr;

use windows::core::PWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_ALIAS, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
    TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR,
};

use super::error::{NtError, ERROR_OUTSIDE_TREE};
use super::handle::{open_with, Containment, OwnedHandle};
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

/// SID allocated by `ConvertStringSidToSidW`; trustees borrow its bytes.
struct LocalSid(windows::Win32::Security::PSID, usize);

impl LocalSid {
    fn from_string(text: &str) -> Result<Self, NtError> {
        let wide = Wide::new(text);
        let mut sid = windows::Win32::Security::PSID::default();
        unsafe { ConvertStringSidToSidW(wide.pc(), &mut sid) }.map_err(NtError::from)?;
        // SID = { BYTE Revision; BYTE SubAuthorityCount; BYTE Authority[6];
        //        DWORD SubAuthority[SubAuthorityCount] }
        let header = unsafe { std::slice::from_raw_parts(sid.0.cast::<u8>(), 2) };
        let length = 8 + 4 * header[1] as usize;
        Ok(Self(sid, length))
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.0 .0.cast::<u8>(), self.1) }
    }
}

impl Drop for LocalSid {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0.cast())));
        }
    }
}

struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);
impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0 .0)));
        }
    }
}

/// Объект должен лежать в удаляемом дереве. Права и владельца нельзя менять
/// у того, что оказалось снаружи: иначе подмена каталога на junction дала бы
/// возможность забрать чужой системный файл, а не только удалить его.
fn ensure_inside(handle: &OwnedHandle, guard: Option<&Containment>) -> Result<(), NtError> {
    match guard {
        Some(guard) if !guard.holds(handle) => Err(NtError(ERROR_OUTSIDE_TREE)),
        _ => Ok(()),
    }
}

/// Перехватывает объект и назначает владельцем текущего пользователя.
pub fn take_ownership(path: &NtPath, guard: Option<&Containment>) -> Result<(), NtError> {
    let _ = privs::enable_take_ownership();
    let owner = privs::current_user_sid()?;
    // SeTakeOwnershipPrivilege grants WRITE_OWNER, not DELETE. Requesting
    // DELETE here would prevent repairing precisely the ACL that denies it.
    let handle = open_with(
        path,
        windows::Win32::Storage::FileSystem::WRITE_OWNER,
        windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
            | windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    ensure_inside(&handle, guard)?;
    // Раньше здесь стоял `GetSecurityInfo(OWNER)`: результат нигде не
    // использовался, а читать владельца без READ_CONTROL нельзя, так что вызов
    // мог только провалить смену владельца отказом в доступе.
    let result = unsafe {
        SetSecurityInfo(
            handle.raw(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(windows::Win32::Security::PSID(
                owner.as_ptr() as *mut core::ffi::c_void
            )),
            None,
            None,
            None,
        )
    };
    result.ok().map_err(|_| NtError(result.0))
}

/// Добавляет в DACL полный доступ для текущего пользователя, SYSTEM и
/// администраторов, **не трогая** уже существующие записи: файлы часто
/// наследуют права от родителя, и сбрасывать их целиком нельзя.
pub fn grant_full_control(path: &NtPath, guard: Option<&Containment>) -> Result<(), NtError> {
    grant_rights(
        path,
        guard,
        windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS.0,
    )
}

/// Открывает удаление детей каталога: единственный безопасный маршрут для
/// файла с несколькими жёсткими ссылками, у которого менять DACL нельзя —
/// владелец и права принадлежат файлу, а не имени. `FILE_DELETE_CHILD` на
/// родителе разрешает удалить имя, не трогая сам файл.
pub fn grant_delete_child(path: &NtPath, guard: Option<&Containment>) -> Result<(), NtError> {
    grant_rights(
        path,
        guard,
        windows::Win32::Storage::FileSystem::FILE_DELETE_CHILD.0,
    )
}

fn grant_rights(path: &NtPath, guard: Option<&Containment>, rights: u32) -> Result<(), NtError> {
    let _ = privs::enable_take_ownership();
    let handle: OwnedHandle = open_with(
        path,
        windows::Win32::Storage::FileSystem::READ_CONTROL
            | windows::Win32::Storage::FileSystem::WRITE_DAC,
        backup_semantics(),
    )?;
    ensure_inside(&handle, guard)?;

    // SID-ы живут до конца вызова: SetEntriesInAclW читает их по указателям
    // из TRUSTEE_W, поэтому освобождать раньше времени нельзя.
    let current = privs::current_user_sid()?;
    let system = LocalSid::from_string(SID_SYSTEM)?;
    let administrators = LocalSid::from_string(SID_ADMINISTRATORS)?;

    unsafe {
        let mut existing: *mut ACL = ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let result = GetSecurityInfo(
            handle.raw(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut existing),
            None,
            Some(&mut descriptor),
        );
        result.ok().map_err(|_| NtError(result.0))?;
        let _descriptor = LocalSecurityDescriptor(descriptor);
        // A NULL DACL already grants everybody full access; replacing it with
        // just our trustees would revoke existing access rather than extend it.
        if existing.is_null() {
            return Ok(());
        }

        let entries = [
            entry(current.as_slice(), TRUSTEE_IS_USER, rights),
            entry(system.as_slice(), TRUSTEE_IS_WELL_KNOWN_GROUP, rights),
            entry(administrators.as_slice(), TRUSTEE_IS_ALIAS, rights),
        ];

        let mut merged: *mut ACL = ptr::null_mut();
        let result = SetEntriesInAclW(Some(&entries), Some(existing), &mut merged);
        result.ok().map_err(|_| NtError(result.0))?;
        let merged = LocalAcl(merged);

        let result = SetSecurityInfo(
            handle.raw(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(merged.0),
            None,
        );
        result.ok().map_err(|_| NtError(result.0))
    }
}

fn backup_semantics() -> windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES {
    windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS
        | windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT
}

fn entry(
    sid: &[u8],
    trustee_type: windows::Win32::Security::Authorization::TRUSTEE_TYPE,
    rights: u32,
) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: rights,
        grfAccessMode: GRANT_ACCESS,
        // Each child is repaired through its own containment-checked handle.
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: windows::Win32::Security::Authorization::NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: trustee_type,
            ptstrName: PWSTR(sid.as_ptr() as *mut u16),
        },
    }
}

/// Тестовый помощник: защищённый DACL, где у текущего пользователя ровно
/// перечисленные права. Тесты удаления получают настоящий отказ, а не
/// смоделированный.
#[cfg(test)]
pub(crate) fn set_test_dacl(path: &NtPath, rights: u32) {
    let open_failed = |error: NtError| {
        panic!(
            "set_test_dacl {} ({rights:#x}): {error}",
            path.to_string_lossy()
        )
    };
    let handle = open_with(
        path,
        windows::Win32::Storage::FileSystem::READ_CONTROL
            | windows::Win32::Storage::FileSystem::WRITE_DAC,
        backup_semantics(),
    )
    .unwrap_or_else(open_failed);
    let sid = privs::current_user_sid().unwrap();
    let access = entry(sid.as_slice(), TRUSTEE_IS_USER, rights);
    let mut acl = ptr::null_mut();
    unsafe { SetEntriesInAclW(Some(&[access]), None, &mut acl) }
        .ok()
        .unwrap();
    let acl = LocalAcl(acl);
    unsafe {
        SetSecurityInfo(
            handle.raw(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION
                | windows::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(acl.0),
            None,
        )
    }
    .ok()
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_ALL_ACCESS, FILE_DELETE_CHILD, FILE_READ_ATTRIBUTES, READ_CONTROL, WRITE_DAC,
        WRITE_OWNER,
    };

    fn restrict(path: &NtPath, rights: u32) -> OwnedHandle {
        let handle = open_with(path, READ_CONTROL | WRITE_DAC, backup_semantics()).unwrap();
        set_access(&handle, rights);
        handle
    }

    fn set_access(handle: &OwnedHandle, rights: u32) {
        let sid = privs::current_user_sid().unwrap();
        let access = entry(&sid, TRUSTEE_IS_USER, rights);
        let mut acl = ptr::null_mut();
        unsafe { SetEntriesInAclW(Some(&[access]), None, &mut acl) }
            .ok()
            .unwrap();
        let acl = LocalAcl(acl);
        unsafe {
            SetSecurityInfo(
                handle.raw(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(acl.0),
                None,
            )
        }
        .ok()
        .unwrap();
    }

    #[test]
    fn ownership_and_acl_repair_do_not_require_delete_access() {
        let base = std::env::temp_dir().join(format!(
            "raze-acl-repair-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&base).unwrap();
        let file = base.join("locked.txt");
        std::fs::write(&file, b"x").unwrap();
        let root = NtPath::new(&base).unwrap();
        let path = NtPath::new(&file).unwrap();
        let guard = Containment::new(&root).unwrap();
        // Neither the file nor its parent grants a deletion route.
        // Win32 CreateFile also requires SYNCHRONIZE; omit DELETE, not this
        // unrelated prerequisite for opening a synchronous file handle.
        let file_access = restrict(
            &path,
            (READ_CONTROL
                | WRITE_DAC
                | WRITE_OWNER
                | FILE_READ_ATTRIBUTES
                | windows::Win32::Storage::FileSystem::SYNCHRONIZE)
                .0,
        );
        let root_access = restrict(&root, FILE_ALL_ACCESS.0 & !FILE_DELETE_CHILD.0);
        let denied = open_with(
            &path,
            DELETE,
            windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
        )
        .is_err();
        let owner = take_ownership(&path, Some(&guard));
        let grant = grant_full_control(&path, Some(&guard));
        let deletable = open_with(
            &path,
            DELETE,
            windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
        )
        .is_ok();
        // Restore access before assertions so a failed regression does not
        // leave an ACL-restricted temporary object behind.
        set_access(&file_access, FILE_ALL_ACCESS.0);
        set_access(&root_access, FILE_ALL_ACCESS.0);
        drop(file_access);
        drop(root_access);
        std::fs::remove_dir_all(&base).unwrap();
        assert!(denied);
        assert!(owner.is_ok(), "{owner:?}");
        assert!(grant.is_ok(), "{grant:?}");
        assert!(deletable);
    }

    #[test]
    fn unreadable_subdirectories_are_counted_once_and_obey_the_display_budget() {
        use crate::engine::scan::{scan, ScanOptions};
        let base = std::env::temp_dir().join(format!(
            "raze-acl-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&base).unwrap();
        let root = NtPath::new(&base).unwrap();
        let mut paths = Vec::new();
        for index in 0..4 {
            let path = base.join(format!("hidden-{index}"));
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("contents.txt"), b"x").unwrap();
            let path = NtPath::new(&path).unwrap();
            restrict(
                &path,
                FILE_ALL_ACCESS.0 & !windows::Win32::Storage::FileSystem::FILE_LIST_DIRECTORY.0,
            );
            paths.push(path);
        }
        let options = ScanOptions {
            max_listed: 1,
            probe_access: false,
            ..ScanOptions::default()
        };
        let (listed, summary) = scan(&root, &options);
        let (empty, zero) = scan(
            &root,
            &ScanOptions {
                max_listed: 0,
                ..options
            },
        );
        for path in paths {
            restrict(&path, FILE_ALL_ACCESS.0);
        }
        std::fs::remove_dir_all(&base).unwrap();
        assert!(summary.truncated);
        assert_eq!((summary.directories, summary.troubled), (4, 4));
        assert_eq!(listed.len() + summary.unlisted, summary.directories);
        assert!(listed.len() <= options.max_listed * 2);
        assert!(listed.iter().all(|entry| entry
            .blockers
            .iter()
            .any(|blocker| matches!(blocker, crate::engine::scan::Blocker::Unreadable { .. }))));
        let mut listed_paths: Vec<_> = listed.iter().map(|entry| &entry.path).collect();
        listed_paths.sort();
        listed_paths.dedup();
        assert_eq!(listed_paths.len(), listed.len());
        assert!(empty.is_empty());
        assert_eq!((zero.directories, zero.troubled, zero.unlisted), (4, 4, 4));
    }
}
