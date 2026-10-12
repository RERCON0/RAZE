//! Корзина: когда её можно использовать, а когда объект уйдёт безвозвратно.
//!
//! The Shell's default delete behavior can silently bypass a full or disabled
//! Recycle Bin. RAZE checks the settings before deletion, requests a warning
//! before destruction, and records the actual per-item IFileOperation result.

use crate::tr_format;
use serde::Serialize;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_READ_ATTRIBUTES, GetDriveTypeW,
    GetVolumeInformationW, GetVolumePathNameW,
};
use windows::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE};
use windows::Win32::UI::Shell::{
    FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_NO_CONNECTED_ELEMENTS, FOF_SILENT, FOF_WANTNUKEWARNING,
    FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation, IFileOperation,
    IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem, SHCreateItemFromParsingName,
    SICHINT_CANONICAL, TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows::core::{HRESULT, PCWSTR, Ref};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use winreg::RegKey;

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
    // The Shell parses ordinary Win32 names and can trim these suffixes,
    // addressing a different sibling than the raw NT object selected by RAZE.
    if path.ext().as_slice().split(|c| *c == b'\\' as u16)
        .any(|c| matches!(c.last(), Some(c) if *c == b'.' as u16 || *c == b' ' as u16))
    {
        return RecycleVerdict {
            reusable: false,
            reason: tr!(
                "names ending in a dot or space cannot be safely sent to the Recycle Bin",
                "имена с точкой или пробелом в конце нельзя безопасно отправить в Корзину"
            ).to_string(),
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
    if let Ok(handle) = open_with(path, FILE_READ_ATTRIBUTES, super::handle::object_flags()) {
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
    // Размер здесь неизвестен: предел Корзины проверяет `job::plan`, когда
    // дерево уже просканировано (`BinSettings::refusal`).
    if let Some(reason) = bin_settings(path).refusal(0) {
        return RecycleVerdict {
            reusable: false,
            reason,
        };
    }
    RecycleVerdict {
        reusable: true,
        reason: String::new(),
    }
}

const BITBUCKET: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket";
const EXPLORER_POLICY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";

/// Настройки Корзины, по которым оболочка **молча** удаляет насовсем: при
/// `FOF_NOCONFIRMATION` она не спрашивает, и отчёт «ушло в Корзину» был бы
/// неправдой. Читаются только твёрдо известные значения; не нашли — значит,
/// ограничений нет.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BinSettings {
    /// «Не перемещать файлы в Корзину» — для тома или политикой.
    pub nuke_on_delete: bool,
    /// Предельный размер Корзины тома в мегабайтах.
    pub max_megabytes: Option<u32>,
}

impl BinSettings {
    /// Читает настройки тома `guid` (вида `{xxxxxxxx-…}`). Корни и пути ключей
    /// передаются параметрами, чтобы тест мог работать во временной ветке.
    fn read(user: &RegKey, machine: &RegKey, bitbucket: &str, policy: &str, guid: &str) -> Self {
        let flag = |key: &RegKey, name: &str| key.get_value::<u32, _>(name).map(|v| v != 0).unwrap_or(false);
        let mut settings = Self::default();
        for root in [user, machine] {
            if let Ok(key) = root.open_subkey(policy) {
                settings.nuke_on_delete |= flag(&key, "NoRecycleFiles");
            }
        }
        if let Ok(key) = user.open_subkey(format!(r"{bitbucket}\Volume\{guid}")) {
            settings.nuke_on_delete |= flag(&key, "NukeOnDelete");
            settings.max_megabytes = key.get_value::<u32, _>("MaxCapacity").ok().filter(|mb| *mb > 0);
        }
        settings
    }

    /// Почему оболочка не положила бы `bytes` в Корзину, а удалила бы сразу.
    pub fn refusal(&self, bytes: u64) -> Option<String> {
        use crate::tr;
        if self.nuke_on_delete {
            return Some(
                tr!(
                    "the Recycle Bin is set to delete immediately on this drive",
                    "для этого диска Корзина настроена на немедленное удаление"
                )
                .to_string(),
            );
        }
        match self.max_megabytes {
            Some(limit) if bytes > u64::from(limit) * 1024 * 1024 => Some(tr_format!(
                "{size} is over the Recycle Bin limit of {limit} MB for this drive",
                "{size} больше предела Корзины этого диска ({limit} МБ)",
                size = crate::report::human_bytes(bytes),
                limit = limit
            )),
            _ => None,
        }
    }
}

/// GUID тома, на котором лежит путь: ключ настроек Корзины называется им.
fn volume_guid(path: &NtPath) -> Option<String> {
    use windows::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;

    let root = volume_root(path).ok()?;
    let mut buffer = [0u16; 64];
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root.as_ptr()), &mut buffer) }.ok()?;
    let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    let text = String::from_utf16_lossy(&buffer[..end]);
    let (start, stop) = (text.find('{')?, text.find('}')?);
    text.get(start..=stop).map(str::to_string)
}

pub fn bin_settings(path: &NtPath) -> BinSettings {
    let guid = volume_guid(path);
    BinSettings::read(
        &RegKey::predef(HKEY_CURRENT_USER),
        &RegKey::predef(HKEY_LOCAL_MACHINE),
        BITBUCKET,
        EXPLORER_POLICY,
        guid.as_deref().unwrap_or(""),
    )
}

/// Sends items to the Shell and reports their actual disposition. A successful
/// queue/PerformOperations alone does not mean the item reached the Recycle Bin.
pub fn send_to_recycle(paths: &[std::path::PathBuf]) -> Result<Disposition, NtError> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    if paths.is_empty() {
        return Ok(Disposition::RecycleBin);
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) { unsafe { CoUninitialize() }; }
    }
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok().map_err(NtError::from)?;
    let _apartment = Apartment;
    let operation: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) }
        .map_err(NtError::from)?;
    unsafe {
        operation.SetOperationFlags(
            FOFX_RECYCLEONDELETE | FOFX_EARLYFAILURE | FOF_NOERRORUI | FOF_SILENT
                | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING | FOF_NO_CONNECTED_ELEMENTS,
        )
    }.map_err(NtError::from)?;
    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        let nt_path = NtPath::new(path).map_err(|_| NtError(super::error::ERROR_INVALID_PARAMETER))?;
        if !is_recyclable(&nt_path).reusable {
            return Err(NtError(super::error::ERROR_NOT_SUPPORTED));
        }
        let wide = Wide::from_path(path);
        let item: IShellItem = unsafe { SHCreateItemFromParsingName(wide.pc(), None) }
            .map_err(NtError::from)?;
        let state = Arc::new(DeleteState::default());
        let sink: IFileOperationProgressSink = DeleteSink { item: item.clone(), state: state.clone() }.into();
        unsafe { operation.DeleteItem(&item, &sink) }.map_err(NtError::from)?;
        results.push(state);
    }
    let performed = unsafe { operation.PerformOperations() };
    let aborted = unsafe { operation.GetAnyOperationsAborted() }.map_err(NtError::from)?.as_bool();
    let mut incomplete = false;
    let mut disposition = Disposition::RecycleBin;
    for state in results {
        let error = state.error.load(Ordering::Relaxed);
        if error != 0 { return Err(NtError(error)); }
        match state.disposition.load(Ordering::Relaxed) {
            1 => {}
            2 => disposition = Disposition::Permanent,
            _ => incomplete = true,
        }
    }
    performed.map_err(NtError::from)?;
    if aborted || incomplete { return Err(NtError(ERROR_ABORTED)); }
    Ok(disposition)
}

#[derive(Default)]
struct DeleteState {
    disposition: AtomicU8,
    error: AtomicU32,
}

#[windows::core::implement(IFileOperationProgressSink)]
struct DeleteSink {
    item: IShellItem,
    state: Arc<DeleteState>,
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for DeleteSink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> { Ok(()) }
    fn FinishOperations(&self, _result: HRESULT) -> windows::core::Result<()> { Ok(()) }
    fn PreDeleteItem(&self, flags: u32, _item: Ref<IShellItem>) -> windows::core::Result<()> {
        if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            return Err(windows::core::Error::from_hresult(HRESULT::from_win32(ERROR_ABORTED)));
        }
        Ok(())
    }
    fn PostDeleteItem(&self, _flags: u32, item: Ref<IShellItem>, result: HRESULT, recycled: Ref<IShellItem>) -> windows::core::Result<()> {
        let item = item.as_ref().ok_or_else(|| windows::core::Error::from_hresult(HRESULT::from_win32(ERROR_ABORTED)))?;
        // A folder can generate callbacks for its children as well.
        if unsafe { self.item.Compare(item, SICHINT_CANONICAL.0 as u32) }? != 0 { return Ok(()); }
        if result.is_err() {
            self.state.error.store(
                NtError::from(windows::core::Error::from_hresult(result)).code(),
                Ordering::Relaxed,
            );
        } else if recycled.as_ref().is_some() {
            // Recycling can return COPYENGINE_S_DONT_PROCESS_CHILDREN: the
            // complete item moved to the Bin, so its children need no work.
            self.state.disposition.store(1, Ordering::Relaxed);
        } else if result.0 == 0 {
            self.state.disposition.store(2, Ordering::Relaxed);
        } else {
            // Success-class statuses such as USER_IGNORED without a resulting
            // Bin item are not proof of a completed delete.
            self.state.error.store(ERROR_ABORTED, Ordering::Relaxed);
        }
        Ok(())
    }
    fn PreRenameItem(&self, _flags: u32, _item: Ref<IShellItem>, _name: &PCWSTR) -> windows::core::Result<()> { Ok(()) }
    fn PostRenameItem(&self, _flags: u32, _item: Ref<IShellItem>, _name: &PCWSTR, _result: HRESULT, _new: Ref<IShellItem>) -> windows::core::Result<()> { Ok(()) }
    fn PreMoveItem(&self, _flags: u32, _item: Ref<IShellItem>, _destination: Ref<IShellItem>, _name: &PCWSTR) -> windows::core::Result<()> { Ok(()) }
    fn PostMoveItem(&self, _flags: u32, _item: Ref<IShellItem>, _destination: Ref<IShellItem>, _name: &PCWSTR, _result: HRESULT, _new: Ref<IShellItem>) -> windows::core::Result<()> { Ok(()) }
    fn PreCopyItem(&self, _flags: u32, _item: Ref<IShellItem>, _destination: Ref<IShellItem>, _name: &PCWSTR) -> windows::core::Result<()> { Ok(()) }
    fn PostCopyItem(&self, _flags: u32, _item: Ref<IShellItem>, _destination: Ref<IShellItem>, _name: &PCWSTR, _result: HRESULT, _new: Ref<IShellItem>) -> windows::core::Result<()> { Ok(()) }
    fn PreNewItem(&self, _flags: u32, _destination: Ref<IShellItem>, _name: &PCWSTR) -> windows::core::Result<()> { Ok(()) }
    fn PostNewItem(&self, _flags: u32, _destination: Ref<IShellItem>, _name: &PCWSTR, _template: &PCWSTR, _attributes: u32, _result: HRESULT, _new: Ref<IShellItem>) -> windows::core::Result<()> { Ok(()) }
    fn UpdateProgress(&self, _total: u32, _done: u32) -> windows::core::Result<()> { Ok(()) }
    fn ResetTimer(&self) -> windows::core::Result<()> { Ok(()) }
    fn PauseTimer(&self) -> windows::core::Result<()> { Ok(()) }
    fn ResumeTimer(&self) -> windows::core::Result<()> { Ok(()) }
}

const ERROR_ABORTED: u32 = 1223;

fn volume_root(path: &NtPath) -> Result<Vec<u16>, NtError> {
    // A mounted volume can live below C:\; using the drive-letter root would
    // read the wrong filesystem and Recycle Bin settings for its contents.
    let mut root = vec![0u16; path.ext().len() + 2];
    unsafe { GetVolumePathNameW(path.ext().pc(), &mut root) }.map_err(NtError::from)?;
    let end = root.iter().position(|unit| *unit == 0).unwrap_or(root.len() - 1);
    root.truncate(end + 1);
    Ok(root)
}

fn volume_filesystem(path: &NtPath) -> Result<String, NtError> {
    let root = volume_root(path)?;
    let mut buffer = [0u16; 64];
    // В 0.62 обёртка возвращает Result без длины: имя файловой системы
    // NUL-терминированное, поэтому длину находим сами.
    unsafe {
        GetVolumeInformationW(PCWSTR(root.as_ptr()), None, None, None, None, Some(&mut buffer))
            .map_err(NtError::from)?;
    }
    let end = buffer
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..end]))
}

fn drive_type(path: &NtPath) -> Result<u32, NtError> {
    let root = volume_root(path)?;
    let result = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
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

/// Recycles supported objects, otherwise removes them permanently. An actual
/// Shell error permits fallback, but cancellation must never trigger deletion.
pub fn remove(
    path: &NtPath,
    options: &super::remove::RemoveOptions,
    prefer_recycle: bool,
) -> (Disposition, Option<super::remove::Outcome>) {
    let verdict = is_recyclable(path);
    if prefer_recycle && verdict.reusable && !options.dry_run {
        match send_to_recycle(&[path.to_path()]) {
            Ok(disposition) => return (disposition, None),
            Err(error) if error.code() == ERROR_ABORTED => {
                let mut outcome = super::remove::Outcome::default();
                outcome.failures.push(super::remove::Failure {
                    path: path.to_string_lossy(),
                    code: error.code(),
                    message: error.message(),
                    holders: Vec::new(),
                });
                return (Disposition::RecycleBin, Some(outcome));
            }
            Err(_) => {}
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

        assert_eq!(send_to_recycle(std::slice::from_ref(&file)).expect("корзина обязана принять обычный файл"), Disposition::RecycleBin);
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn recycling_a_web_page_does_not_delete_its_unselected_sidecar() {
        let base = scratch("connected");
        let page = base.join("page.htm");
        let sidecar = base.join("page_files");
        std::fs::write(&page, b"<html></html>").unwrap();
        std::fs::create_dir(&sidecar).unwrap();
        std::fs::write(sidecar.join("keep.txt"), b"outside selection").unwrap();
        let result = send_to_recycle(std::slice::from_ref(&page));
        let kept = std::fs::read(sidecar.join("keep.txt"));
        std::fs::remove_dir_all(&base).unwrap();
        assert_eq!(result.unwrap(), Disposition::RecycleBin);
        assert_eq!(kept.unwrap(), b"outside selection");
    }

    #[test]
    fn shell_ambiguous_names_cannot_delete_their_ordinary_sibling() {
        use std::os::windows::ffi::OsStringExt;
        let base = scratch("suffix");
        let ordinary = base.join("note");
        std::fs::write(&ordinary, b"keep").unwrap();
        for suffix in ["note.", "note "] {
            let nt_path = NtPath::new(&base.join(suffix)).unwrap();
            let extended = std::path::PathBuf::from(std::ffi::OsString::from_wide(nt_path.ext().as_slice()));
            std::fs::write(&extended, b"raw").unwrap();
            assert!(!is_recyclable(&nt_path).reusable);
            let result = send_to_recycle(&[nt_path.to_path()]);
            let ordinary_bytes = std::fs::read(&ordinary).unwrap();
            let raw_bytes = std::fs::read(&extended).unwrap();
            std::fs::remove_file(&extended).unwrap();
            assert!(result.is_err());
            assert_eq!(ordinary_bytes, b"keep");
            assert_eq!(raw_bytes, b"raw");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_locked_file_is_not_reported_as_recycled_after_queueing() {
        use std::os::windows::fs::OpenOptionsExt;
        let base = scratch("locked");
        let path = base.join("locked.txt");
        std::fs::write(&path, b"keep").unwrap();
        let file = std::fs::OpenOptions::new().read(true)
            .share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0)
            .open(&path).unwrap();
        let result = send_to_recycle(std::slice::from_ref(&path));
        drop(file);
        let contents = std::fs::read(&path);
        std::fs::remove_dir_all(&base).unwrap();
        assert!(result.is_err());
        assert_eq!(contents.unwrap(), b"keep");
    }

    #[test]
    fn reserved_names_are_never_recyclable() {
        let base = scratch("nul");
        let path = base.join("NUL");
        let nt_path = NtPath::new(&path).unwrap();
        let verdict = is_recyclable(&nt_path);
        assert!(!verdict.reusable);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_shell_would_delete_silently_in_these_cases_and_we_say_so() {
        let machine = RegKey::predef(HKEY_LOCAL_MACHINE);
        let user = RegKey::predef(HKEY_CURRENT_USER);
        let scratch = format!(r"Software\RAZE.Tests\bin-{}", std::process::id());
        let guid = "{11111111-2222-3333-4444-555555555555}";
        let bitbucket = format!(r"{scratch}\BitBucket");
        let policy = format!(r"{scratch}\Policy");

        // Ничего не задано — ограничений нет.
        assert_eq!(BinSettings::read(&user, &machine, &bitbucket, &policy, guid), BinSettings::default());
        assert!(BinSettings::default().refusal(u64::MAX).is_none());

        // Предел Корзины тома: 100 МБ.
        let (volume, _) = user.create_subkey(format!(r"{bitbucket}\Volume\{guid}")).unwrap();
        volume.set_value("MaxCapacity", &100u32).unwrap();
        let settings = BinSettings::read(&user, &machine, &bitbucket, &policy, guid);
        assert_eq!(settings.max_megabytes, Some(100));
        assert!(settings.refusal(99 * 1024 * 1024).is_none());
        assert!(settings.refusal(101 * 1024 * 1024).is_some());

        // «Не перемещать файлы в Корзину» — отказ при любом размере.
        volume.set_value("NukeOnDelete", &1u32).unwrap();
        let settings = BinSettings::read(&user, &machine, &bitbucket, &policy, guid);
        assert!(settings.nuke_on_delete);
        assert!(settings.refusal(0).is_some());

        // То же самое политикой.
        volume.set_value("NukeOnDelete", &0u32).unwrap();
        let (policy_key, _) = user.create_subkey(&policy).unwrap();
        policy_key.set_value("NoRecycleFiles", &1u32).unwrap();
        assert!(BinSettings::read(&user, &machine, &bitbucket, &policy, guid).nuke_on_delete);

        let _ = user.delete_subkey_all(&scratch);
    }

    #[test]
    fn a_real_volume_has_a_guid() {
        let dir = NtPath::new(&std::env::temp_dir()).unwrap();
        let guid = volume_guid(&dir).expect("the volume of %TEMP% has a GUID name");
        assert!(guid.starts_with('{') && guid.ends_with('}'), "{guid}");
    }

}