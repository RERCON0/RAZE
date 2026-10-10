//! Привилегии процесса: SeBackupPrivilege, SeRestorePrivilege,
//! SeTakeOwnershipPrivilege.
//!
//! Это то, чем RAZE отличается от `del /f`: привилегии позволяют открыть
//! объект «в обход» DACL, но менять права на чужие файлы они не позволяют.
//! Права меняются отдельным осознанным шагом в [`super::security`].

use std::mem;

use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::error::{NtError, nt};

/// `AdjustTokenPrivileges` возвращает TRUE даже когда привилегии у токена
/// нет — тогда в `GetLastError` лежит ERROR_NOT_ALL_ASSIGNED (1300).
const ERROR_NOT_ALL_ASSIGNED: u32 = 1300;

pub const SE_BACKUP: &str = "SeBackupPrivilege";
pub const SE_RESTORE: &str = "SeRestorePrivilege";
pub const SE_TAKE_OWNERSHIP: &str = "SeTakeOwnershipPrivilege";

/// Включает привилегию текущего процесса.
pub fn enable(name: &str) -> Result<(), NtError> {
    let name = super::path::Wide::new(name);
    let mut luid = windows::Win32::Foundation::LUID::default();
    unsafe { nt(LookupPrivilegeValueW(None, name.pc(), &mut luid)) }?;

    let token = unsafe { open_process_token()? };
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    unsafe {
        nt(AdjustTokenPrivileges(
            token,
            false,
            Some(&privileges),
            mem::size_of::<TOKEN_PRIVILEGES>() as u32,
            None,
            None,
        ))
    }?;
    let code = unsafe { GetLastError() }.0;
    if code == ERROR_NOT_ALL_ASSIGNED {
        return Err(NtError(ERROR_NOT_ALL_ASSIGNED));
    }
    Ok(())
}

/// Включает пару привилегий, нужных почти каждому «трудному» удалению.
/// Частичный успех — не ошибка: у непривилегированного процесса их может не
/// быть вовсе, и тогда просто останется ACCESS_DENIED.
pub fn enable_backup_restore() -> bool {
    let se_backup = enable(SE_BACKUP).is_ok();
    let se_restore = enable(SE_RESTORE).is_ok();
    se_backup || se_restore
}

pub fn enable_take_ownership() -> bool {
    enable(SE_TAKE_OWNERSHIP).is_ok()
}

pub unsafe fn open_process_token() -> Result<HANDLE, NtError> {
    let mut token = HANDLE::default();
    nt(OpenProcessToken(
        GetCurrentProcess(),
        TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
        &mut token,
    ))?;
    Ok(token)
}

struct TokenHandle(HANDLE);
impl Drop for TokenHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// SID текущего пользователя, скопированный в кучу: указатель внутри токена
/// перестаёт быть валидным сразу после закрытия токена.
pub fn current_user_sid() -> Result<Vec<u8>, NtError> {
    let token = unsafe { TokenHandle(open_process_token()?) };
    // SAFETY: буфер выделен по размеру, который вернул GetTokenInformation,
    // и читается ровно на этот размер.
    unsafe {
        let mut needed = 0u32;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            return Err(NtError::last());
        }
        let mut buffer = vec![0u8; needed as usize];
        nt(GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut _),
            needed,
            &mut needed,
        ))?;
        // TOKEN_USER = { SID_AND_ATTRIBUTES { PSID Sid; DWORD Attributes } }.
        // Первое поле — указатель, сам SID лежит по этому адресу.
        let sid = *(buffer.as_ptr() as *const *const u8);
        if sid.is_null() {
            return Err(NtError::last());
        }
        // У SID нет поля «длина»: размер выводится из SubAuthorityCount.
        //   { BYTE Revision; BYTE SubAuthorityCount; BYTE Authority[6];
        //     DWORD SubAuthority[SubAuthorityCount] }
        let sub_authorities = *sid.add(1) as usize;
        let length = 8 + 4 * sub_authorities;
        let mut copy = vec![0u8; length];
        std::ptr::copy_nonoverlapping(sid, copy.as_mut_ptr(), length);
        Ok(copy)
    }
}

/// Запущен ли процесс с повышенными правами.
pub fn is_elevated() -> bool {
    let Ok(token) = (unsafe { open_process_token() }) else {
        return false;
    };
    let token = TokenHandle(token);
    let mut elevation = windows::Win32::Security::TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    unsafe {
        let ok = GetTokenInformation(
            token.0,
            windows::Win32::Security::TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            mem::size_of_val(&elevation) as u32,
            &mut returned,
        )
        .is_ok();
        ok && elevation.TokenIsElevated != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privilege_names_are_the_documented_ones() {
        assert_eq!(SE_BACKUP, "SeBackupPrivilege");
        assert_eq!(SE_RESTORE, "SeRestorePrivilege");
        assert_eq!(SE_TAKE_OWNERSHIP, "SeTakeOwnershipPrivilege");
    }

    #[test]
    fn current_user_sid_returns_sid_header() {
        // SID начинается с revision 1 и RevisionCount > 0.
        let sid = current_user_sid().expect("own SID is always readable");
        assert_eq!(sid.first().copied(), Some(1));
        assert!(sid[1] > 0);
    }
}