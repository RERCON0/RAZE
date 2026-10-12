//! Установка RAZE в контекстное меню Проводника — двумя независимыми
//! способами, потому что надёжность здесь важнее минимализма.
//!
//! **Классический verb** (ветка реестра) работает всегда и на любой версии
//! Windows, но в Windows 11 живёт внутри «Показать дополнительные параметры».
//!
//! **Современное меню** (`IExplorerCommand` + разреженный MSIX-пакет) — это
//! то самое меню первого уровня. Windows показывает в нём команды только тех
//! программ, у которых есть идентификатор пакета, а пакет обязан быть
//! подписан доверенным сертификатом. Поэтому пакет `RAZE.msix` собирается
//! заранее (`packaging\build-package.ps1`) и подписывается самоподписанным
//! сертификатом `CN=RERCON RAZE`; установщик один раз добавляет **публичную**
//! часть сертификата в `LocalMachine\TrustedPeople` (запрос UAC) и
//! регистрирует пакет.
//!
//! Если со вторым способом не выйдет (нет файлов пакета, отказ в UAC, редакция
//! без разреженных пакетов), первый всё равно работает — установщик сообщает
//! об этом и не считает установку неудачной.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

use crate::engine::elevation::is_elevated;

/// CLSID нашей COM-расширки. Тот же GUID зашит в `shell-ext` и в
/// `packaging/AppxManifest.xml` (тест ниже сверяет манифест с этой
/// константой). Менять только вместе.
pub const COMMAND_CLSID: &str = "{8E5B2C41-6D3A-4F17-9C0B-51D2A7E4F108}";

/// Имя пакета для `Get-AppxPackage` / `Remove-AppxPackage`.
const PACKAGE_NAME: &str = "RAZE.ShellExtension";
const PACKAGE_FILE: &str = "RAZE.msix";
const CERT_FILE: &str = "RAZE.cer";

/// Отпечаток (SHA-1) сертификата, которым подписан `RAZE.msix`. Доверие в
/// `TrustedPeople` выдаётся только сертификату с этим отпечатком: подменённый
/// или повреждённый `RAZE.cer` в хранилище не попадёт. Значение вшивается в
/// `raze.exe` при сборке из `packaging/cert-thumbprint.txt` (ровно 40 символов,
/// без перевода строки; сборка без этого не пройдёт). Отпечаток публичного сертификата
/// не секрет, закрытый ключ остаётся только в хранилище того, кто собирает.
/// Чужая сборка пересоздаёт сертификат через `packaging\build-package.ps1`: он
/// обновляет этот файл, после чего `raze.exe` пересобирается.
pub const PINNED_CERT_THUMBPRINT: &str =
    pinned(include_str!("../../packaging/cert-thumbprint.txt"));

// Отпечаток подставляется в сценарии PowerShell, которые идут с правами
// администратора. Лишний перевод строки или кавычка в файле ломали бы их (или
// хуже), поэтому формат проверяется при сборке, а не обещается в комментарии.
const fn pinned(text: &'static str) -> &'static str {
    assert!(
        is_thumbprint(text),
        "packaging/cert-thumbprint.txt must hold exactly 40 hex digits, no newline"
    );
    text
}

const fn is_thumbprint(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 40 {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_hexdigit() {
            return false;
        }
        index += 1;
    }
    true
}

/// Куда всё ставится: `%ProgramFiles%\RAZE`. Писать туда могут только
/// администраторы, и это не мелочь: запрос UAC поднимает права у `raze.exe`, а
/// файл из каталога, который может переписать любой процесс пользователя,
/// подменили бы до запроса (повышение прав без всякой подписи). Из Program
/// Files подменить его без прав администратора нельзя.
pub fn default_install_dir() -> Result<PathBuf, String> {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramFiles).map(|base| base.join("RAZE"))
}

/// Где ставили прежние версии: каталог пользователя. Он перезаписываем любым
/// процессом пользователя, поэтому копия оттуда убирается при установке.
pub fn legacy_install_dir() -> Result<PathBuf, String> {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)
        .map(|base| base.join("Programs").join("RAZE"))
}

fn known_folder(id: &windows::core::GUID) -> Result<PathBuf, String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    let pointer = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }
        .map_err(|error| error.to_string())?;
    let path = PathBuf::from(OsString::from_wide(unsafe { pointer.as_wide() }));
    unsafe { CoTaskMemFree(Some(pointer.0.cast())) };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("Windows returned an invalid known-folder path".into())
    }
}

/// Ключи реестра, под которыми появляется пункт меню.
const VERB_KEYS: [&str; 5] = [
    r"Software\Classes\Directory\shell\RAZE.Delete",
    r"Software\Classes\Directory\Background\shell\RAZE.Delete",
    r"Software\Classes\*\shell\RAZE.Delete",
    r"Software\Classes\AllFilesystemObjects\shell\RAZE.Delete",
    r"Software\Classes\Drive\shell\RAZE.Delete",
];

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Step {
    Done { what: String },
    Skipped { what: String },
    Failed { what: String, error: String },
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct InstallReport {
    pub steps: Vec<Step>,
    pub install_dir: PathBuf,
}

impl InstallReport {
    fn done(&mut self, what: impl Into<String>) {
        self.steps.push(Step::Done { what: what.into() });
    }
    fn skipped(&mut self, what: impl Into<String>) {
        self.steps.push(Step::Skipped { what: what.into() });
    }
    fn failed(&mut self, what: impl Into<String>, error: impl Into<String>) {
        self.steps.push(Step::Failed {
            what: what.into(),
            error: error.into(),
        });
    }
    pub fn ok(&self) -> bool {
        !self.steps.iter().any(|s| matches!(s, Step::Failed { .. }))
    }
}

/// Ставит или снимает интеграцию. `uninstall = false` — установка.
pub fn install(source_dir: &Path, uninstall: bool) -> InstallReport {
    let mut report = InstallReport {
        install_dir: PathBuf::new(),
        ..Default::default()
    };
    match default_install_dir() {
        Ok(dir) => report.install_dir = dir,
        Err(error) => {
            report.failed("locate Program Files", error);
            return report;
        }
    }

    if uninstall {
        remove_classic(&mut report);
        remove_modern(&mut report);
        remove_legacy_install(&mut report);
        return report;
    }

    let dir = report.install_dir.clone();
    if !source_dir.join("raze.exe").is_file() {
        report.failed(
            "copy raze.exe",
            format!("not found in {}", source_dir.display()),
        );
        return report;
    }
    // Зарегистрированный пакет держит `raze_shell.dll` загруженной, и файл
    // нельзя заменить. Снимаем регистрацию, ниже она ставится заново.
    let _ = unregister_package();
    if !copy_binaries(source_dir, &dir, &mut report) {
        return report;
    }
    forget_legacy_registration(&dir);
    write_classic(&dir, &mut report);
    register_modern(&dir, &mut report);
    remove_legacy_install(&mut report);
    report
}

/// Убирает копию прежней версии из каталога пользователя (см.
/// [`legacy_install_dir`]). Пункты меню и пакет к этому моменту уже указывают
/// на новый каталог. Если копия запущена прямо сейчас, файл занят: это не
/// ошибка установки, а напоминание удалить папку вручную.
fn remove_legacy_install(report: &mut InstallReport) {
    // Programs and its ancestors are writable by the user. A junction there
    // must never turn migration into an administrator's arbitrary deletion.
    let _user_context = match UnelevatedContext::enter() {
        Ok(context) => context,
        Err(error) => {
            report.skipped(format!(
                "the old per-user copy was not removed ({error}); delete it by hand"
            ));
            return;
        }
    };
    let Ok(old) = legacy_install_dir() else {
        report.skipped("could not locate the old per-user install directory");
        return;
    };
    if old == report.install_dir || !old.join("raze.exe").exists() {
        return;
    }
    match std::fs::remove_dir_all(&old) {
        Ok(()) => report.done(format!("removed the old per-user copy {}", old.display())),
        Err(error) => report.skipped(format!(
            "the old per-user copy {} was not removed ({error}); delete the folder by hand",
            old.display()
        )),
    }
}

struct UnelevatedContext;

impl UnelevatedContext {
    fn enter() -> Result<Option<Self>, String> {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::Security::{
            GetTokenInformation, ImpersonateLoggedOnUser, TokenLinkedToken, TOKEN_LINKED_TOKEN,
            TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        if !is_elevated() {
            return Ok(None);
        }
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(|error| error.to_string())?;
        let mut linked = TOKEN_LINKED_TOKEN::default();
        let mut returned = 0;
        let result = unsafe {
            GetTokenInformation(
                token,
                TokenLinkedToken,
                Some((&mut linked as *mut TOKEN_LINKED_TOKEN).cast()),
                std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
                &mut returned,
            )
        };
        let _ = unsafe { CloseHandle(token) };
        result.map_err(|error| format!("cannot obtain an unelevated token: {error}"))?;
        let result = unsafe { ImpersonateLoggedOnUser(linked.LinkedToken) };
        let _ = unsafe { CloseHandle(linked.LinkedToken) };
        result.map_err(|error| format!("cannot impersonate the unelevated user: {error}"))?;
        Ok(Some(Self))
    }
}

impl Drop for UnelevatedContext {
    fn drop(&mut self) {
        // Windows requires termination if reverting fails: continuing under an
        // unexpected identity is unsafe. Normal unwinding always restores it.
        if unsafe { windows::Win32::Security::RevertToSelf() }.is_err() {
            std::process::abort();
        }
    }
}

/// Раскладывает исполняемые файлы, DLL и файлы пакета в каталог установки.
fn copy_binaries(source: &Path, target: &Path, report: &mut InstallReport) -> bool {
    if !source.join("raze.exe").is_file() {
        report.failed(
            "copy raze.exe",
            format!("not found in {}", source.display()),
        );
        return false;
    }
    if let Err(error) = std::fs::create_dir_all(target) {
        report.failed("create install directory", error.to_string());
        return false;
    }
    let same_directory = std::fs::canonicalize(source)
        .ok()
        .zip(std::fs::canonicalize(target).ok())
        .is_some_and(|(source, target)| source == target);
    // DLL крейта `raze-shell` собирается как `raze_shell.dll`:
    // cargo не разрешает дефисы в именах lib-таргетов.
    let wanted = [
        "raze.exe",
        "raze-app.exe",
        "raze_shell.dll",
        PACKAGE_FILE,
        CERT_FILE,
    ];
    let mut copied = 0usize;
    for name in wanted {
        let from = source.join(name);
        if !from.is_file() {
            report.skipped(format!("{name} (not found in {})", source.display()));
            continue;
        }
        if same_directory {
            continue;
        }
        let to = target.join(name);
        if let Err(error) = std::fs::copy(&from, &to) {
            report.failed(format!("copy {name}"), error.to_string());
            continue;
        }
        copied += 1;
    }
    if copied > 0 {
        report.done(format!(
            "installed {copied} file(s) into {}",
            target.display()
        ));
    }
    if same_directory {
        report.done(format!("already running from {}", target.display()));
    }
    report.ok()
}

/// Прежние версии установщика писали сюда собственный манифест и
/// регистрировали COM-класс в `HKCU`. Пакет теперь сам объявляет класс, а
/// лишняя регистрация только путает, поэтому остатки убираются молча.
fn forget_legacy_registration(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("AppxManifest.xml"));
    let _ = std::fs::remove_file(dir.join("logo.png"));
    let _ = RegKey::predef(HKEY_CURRENT_USER)
        .delete_subkey_all(format!(r"Software\Classes\CLSID\{COMMAND_CLSID}"));
}

/// Современное меню Windows 11: доверие сертификату и регистрация пакета.
fn register_modern(dir: &Path, report: &mut InstallReport) {
    if !dir.join(PACKAGE_FILE).exists() || !dir.join(CERT_FILE).exists() {
        report.skipped(format!(
            "modern Windows 11 menu: {PACKAGE_FILE} / {CERT_FILE} were not found next to raze.exe \
             (build them with packaging\\build-package.ps1, then run raze --install again)"
        ));
        return;
    }
    match register_sparse_package() {
        Ok(()) => report.done("registered the Windows 11 context menu package"),
        Err(error) => report.skipped(format!(
            "modern Windows 11 menu was not registered: {error}"
        )),
    }
}

/// Классический verb. Работает всегда; в Windows 11 — во вложенном меню.
fn write_classic(dir: &Path, report: &mut InstallReport) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let app = dir.join("raze-app.exe").to_string_lossy().to_string();
    let cli = dir.join("raze.exe").to_string_lossy().to_string();
    let mut written = 0usize;

    for verb in VERB_KEYS {
        let is_background = verb.contains("Background");
        let is_drive = verb.starts_with(r"Software\Classes\Drive");
        let result = (|| -> Result<(), String> {
            let (key, _) = hkcu.create_subkey(verb).map_err(|e| e.to_string())?;
            key.set_value("", &label()).map_err(|e| e.to_string())?;
            key.set_value("Icon", &app).map_err(|e| e.to_string())?;
            // Щит на пункте меню — честный намёк, что дальше будет UAC.
            key.set_value("HasLUAShield", &"")
                .map_err(|e| e.to_string())?;

            let (command, _) = key.create_subkey("command").map_err(|e| e.to_string())?;
            // Для фонового клика `%1` пуст, а нужна сама папка: её Explorer
            // отдаёт в `%V`. Привод при `%1` — «Все файлы».
            let argument = match (is_background, is_drive) {
                (true, _) => "%V",
                _ => "%1",
            };
            let line = format!("\"{cli}\" --from-shell --mode \"{argument}\"");
            command.set_value("", &line).map_err(|e| e.to_string())?;
            Ok(())
        })();
        match result {
            Ok(()) => written += 1,
            Err(error) => report.failed(format!("register verb {verb}"), error),
        }
    }

    if written > 0 {
        report.done(format!("registered {written} classic context-menu verb(s)"));
    }
}

fn remove_classic(report: &mut InstallReport) {
    remove_verbs(&RegKey::predef(HKEY_CURRENT_USER), &VERB_KEYS, report);
}

/// Список ключей передаётся параметром, чтобы тест мог работать во
/// временной ветке реестра и не стирал настоящие пункты меню.
fn remove_verbs(hkcu: &RegKey, verbs: &[&str], report: &mut InstallReport) {
    let mut removed = 0usize;
    for verb in verbs {
        if hkcu.delete_subkey_all(verb).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        report.done(format!("removed {removed} classic verb(s)"));
    } else {
        report.skipped("no classic verbs were registered");
    }
}

fn remove_modern(report: &mut InstallReport) {
    match package_registered() {
        Ok(true) => match unregister_package() {
            Ok(()) => report.done("unregistered the Windows 11 context menu package"),
            Err(error) => report.failed("unregister the Windows 11 context menu package", error),
        },
        Ok(false) => report.skipped("the Windows 11 context menu package was not registered"),
        Err(error) => report.skipped(format!("could not query the package ({error})")),
    }

    // Доверие выдавали мы — мы его и снимаем (одно подтверждение UAC).
    if certificate_trusted().unwrap_or(false) {
        let script = format!(
            "{STORE_PREAMBLE} $s.Open('ReadWrite'); \
             foreach ($c in @($s.Certificates.Find('FindByThumbprint', '{PINNED_CERT_THUMBPRINT}', $false))) {{ $s.Remove($c) }}; \
             $s.Close()"
        );
        let result = run_privileged_powershell(&script);
        if certificate_trusted().unwrap_or(false) {
            let reason = result
                .err()
                .unwrap_or_else(|| "the certificate is still trusted".into());
            report.skipped(format!(
                "the RAZE certificate was not removed from LocalMachine\\TrustedPeople ({reason})"
            ));
        } else {
            report.done("removed the RAZE certificate from LocalMachine\\TrustedPeople");
        }
    }

    // Остаток старых установок.
    let _ = RegKey::predef(HKEY_CURRENT_USER)
        .delete_subkey_all(format!(r"Software\Classes\CLSID\{COMMAND_CLSID}"));

    let dir = report.install_dir.clone();
    // Каталог удаляем только если он действительно наш и там нечего терять.
    if dir.join("raze.exe").exists() {
        remove_install_dir(&dir, report);
    } else {
        report.skipped(format!("nothing to remove in {}", dir.display()));
    }
}

/// Удаляет каталог установки. Если снятие запущено из самой установленной
/// копии, её `raze.exe` занят: удалить его нельзя, а остальные файлы уже ушли бы,
/// оставив каталог с одним exe. Тогда остаток убирает отсоединённый PowerShell
/// после завершения процесса.
fn remove_install_dir(dir: &Path, report: &mut InstallReport) {
    let own = std::env::current_exe().ok();
    let running_from_here = own
        .as_deref()
        .and_then(Path::parent)
        .is_some_and(|parent| parent.as_os_str().eq_ignore_ascii_case(dir.as_os_str()));
    if !running_from_here {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => report.done(format!("removed {}", dir.display())),
            Err(error) => report.failed(format!("remove {}", dir.display()), error.to_string()),
        }
        return;
    }

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if Some(entry.path()) != own {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    match spawn_delayed_removal(dir) {
        Ok(()) => report.done(format!("{} will be removed as soon as this program exits", dir.display())),
        Err(error) => report.skipped(format!(
            "{} holds the running raze.exe and was not removed ({error}); delete the folder by hand",
            dir.display()
        )),
    }
}

/// Отсоединённый системный PowerShell ждёт освобождения файлов после выхода.
/// LiteralPath не интерпретирует имена как маску, а shell/PATH здесь не участвуют.
fn spawn_delayed_removal(dir: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         for ($i = 0; $i -lt 30; $i++) {{ \
             Start-Sleep -Seconds 1; \
             if (-not (Test-Path -LiteralPath {0})) {{ exit 0 }}; \
             try {{ Remove-Item -LiteralPath {0} -Recurse -Force; exit 0 }} catch {{ }} \
         }}; exit 1",
        ps_quote(dir)
    );
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let executable = powershell_exe()?;
    let working_directory = executable
        .parent()
        .ok_or("cannot locate the PowerShell directory")?;
    Command::new(&executable)
        .current_dir(working_directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &base64(&utf16),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn label() -> &'static str {
    crate::tr!("Delete with RAZE", "Удалить с помощью RAZE")
}

/// Регистрация разреженного пакета: сначала доверие сертификату, затем
/// `Add-AppxPackage` с каталогом установки как внешним расположением.
/// Вынесено отдельно, чтобы вызывать и без полной установки.
pub fn register_sparse_package() -> Result<(), String> {
    let dir = default_install_dir()?;
    let package = dir.join(PACKAGE_FILE);
    let cert = dir.join(CERT_FILE);
    for file in [&package, &cert] {
        if !file.exists() {
            return Err(format!(
                "{} is missing; build it with packaging\\build-package.ps1 and run raze --install",
                file.display()
            ));
        }
    }
    ensure_certificate_trusted(&cert)?;
    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         Get-AppxPackage -Name '{PACKAGE_NAME}' | Remove-AppxPackage; \
         Add-AppxPackage -Path {} -ExternalLocation {}",
        ps_quote(&package),
        ps_quote(&dir)
    );
    run_powershell(&script).map(|_| ())
}

/// Открывает хранилище `LocalMachine\TrustedPeople` (общая строка скриптов).
const STORE_PREAMBLE: &str = "$s = New-Object System.Security.Cryptography.X509Certificates.X509Store 'TrustedPeople', 'LocalMachine';";

fn certificate_trusted() -> Result<bool, String> {
    let script = format!(
        "Test-Path -LiteralPath 'Cert:\\LocalMachine\\TrustedPeople\\{PINNED_CERT_THUMBPRINT}'"
    );
    Ok(run_powershell(&script)?.eq_ignore_ascii_case("True"))
}

fn package_registered() -> Result<bool, String> {
    let script = format!("@(Get-AppxPackage -Name '{PACKAGE_NAME}').Count");
    Ok(run_powershell(&script)?.trim() != "0")
}

fn unregister_package() -> Result<(), String> {
    let script = format!("Get-AppxPackage -Name '{PACKAGE_NAME}' | Remove-AppxPackage");
    run_powershell(&script).map(|_| ())
}

/// Добавляет сертификат в `LocalMachine\TrustedPeople`, если его там нет.
/// Для записи в машинное хранилище нужны права администратора — Windows
/// спросит подтверждение (UAC). Сертификат читается из файла один раз, а
/// отпечаток проверяется у того самого объекта, который затем добавляется,
/// так что подменить файл между проверкой и добавлением нельзя.
fn ensure_certificate_trusted(cert: &Path) -> Result<(), String> {
    if certificate_trusted()? {
        return Ok(());
    }
    let script = format!(
        "$ErrorActionPreference = 'Stop'; \
         $c = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 {}; \
         if ($c.Thumbprint -ne '{PINNED_CERT_THUMBPRINT}') {{ exit 3 }}; \
         {STORE_PREAMBLE} $s.Open('ReadWrite'); $s.Add($c); $s.Close()",
        ps_quote(cert)
    );
    let launched = run_privileged_powershell(&script);
    if certificate_trusted()? {
        Ok(())
    } else {
        Err(match launched {
            Err(error) => {
                format!("the certificate was not trusted (was the UAC prompt declined?): {error}")
            }
            Ok(()) => format!(
                "the certificate was not trusted: {} does not match the pinned thumbprint",
                cert.display()
            ),
        })
    }
}

/// Полный путь к Windows PowerShell из системного каталога. По имени
/// `powershell` его искали бы по `PATH` и рабочему каталогу, а запускается он
/// с правами администратора: чужой `powershell.exe` в начале `PATH` получил бы
/// эти права.
fn powershell_exe() -> Result<PathBuf, String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buffer = [0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err("cannot locate the Windows system directory".into());
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..length]))
        .join(r"WindowsPowerShell\v1.0\powershell.exe"))
}

fn run_powershell(script: &str) -> Result<String, String> {
    let output = Command::new(powershell_exe()?)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference = 'Stop';",
            script,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Скрипт, которому нужны права администратора: если мы уже повышены (установка
/// в Program Files начинается с UAC), он идёт в обычный PowerShell, и второго
/// запроса не будет; иначе — с повышением.
fn run_privileged_powershell(script: &str) -> Result<(), String> {
    if is_elevated() {
        run_powershell(script).map(|_| ())
    } else {
        run_elevated_powershell(script)
    }
}

/// Запускает скрипт в отдельном PowerShell с повышением прав. Скрипт уходит
/// в `-EncodedCommand`, а не во временный файл: файл в каталоге пользователя
/// мог бы быть подменён до запуска с правами администратора.
fn run_elevated_powershell(script: &str) -> Result<(), String> {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let launcher = format!(
        "$ErrorActionPreference = 'Stop'; \
         $p = Start-Process -FilePath {} -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{}'; exit $p.ExitCode",
        ps_quote(&powershell_exe()?),
        base64(&utf16)
    );
    run_powershell(&launcher).map(|_| ())
}

/// Литерал пути PowerShell. Непарные суррогаты нельзя заменять на U+FFFD:
/// для такого NTFS-имени строим строку напрямую из исходных UTF-16 единиц.
fn ps_quote(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    if let Some(text) = path.to_str() {
        return format!("'{}'", text.replace('\'', "''"));
    }
    let units = path
        .as_os_str()
        .encode_wide()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("([string]::new([char[]]@({units})))")
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(chunk[0]) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bare_sha1_thumbprint_may_reach_an_elevated_script() {
        assert!(is_thumbprint(PINNED_CERT_THUMBPRINT));
        let good = "1CDEBA01251B61E3E40F7465A8253BA276ED204F";
        assert!(is_thumbprint(good));
        assert!(is_thumbprint(&good.to_lowercase()));
        for bad in [
            "",
            &good[1..],
            &format!("{good}\n"),
            &format!("{good}0"),
            "1CDEBA01251B61E3E40F7465A8253BA276ED204G",
            "1CDEBA01251B61E3E40F7465A8253BA276ED20'F",
        ] {
            assert!(!is_thumbprint(bad), "{bad:?}");
        }
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Man"), "TWFu");
    }

    #[test]
    fn powershell_literals_double_their_quotes() {
        assert_eq!(ps_quote(Path::new("C:\\a b\\it's")), "'C:\\a b\\it''s'");
    }

    #[test]
    fn powershell_path_literals_preserve_unpaired_surrogates() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let units = [b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'\'' as u16];
        let path = PathBuf::from(OsString::from_wide(&units));
        let result = run_powershell(&format!(
            "({}).ToCharArray() | ForEach-Object {{ [int]$_ }}",
            ps_quote(&path)
        ))
        .unwrap();
        let actual: Vec<u16> = result.lines().map(|line| line.parse().unwrap()).collect();
        assert_eq!(actual, units);
    }

    #[test]
    fn install_dir_is_in_program_files_and_the_old_user_copy_is_elsewhere() {
        // Из Program Files процесс без прав администратора файл не подменит;
        // каталог пользователя для этого не годится.
        let dir = default_install_dir().unwrap();
        assert!(dir.ends_with("RAZE"));
        assert_eq!(
            dir.parent(),
            Some(
                known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramFiles)
                    .unwrap()
                    .as_path()
            )
        );
        assert_ne!(dir, legacy_install_dir().unwrap());
    }

    #[test]
    fn missing_cli_does_not_create_or_register_an_installation() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let source = std::env::temp_dir().join(format!("raze-missing-source-{unique}"));
        let target = std::env::temp_dir().join(format!("raze-missing-target-{unique}"));
        let mut report = InstallReport::default();
        assert!(!copy_binaries(&source, &target, &mut report));
        assert!(!report.ok());
        assert!(!target.exists());
    }

    #[test]
    fn removing_verbs_that_are_not_there_is_not_an_error() {
        // Повторный запуск сноса не должен падать — он просто ничего не найдёт.
        // Настоящие ключи не трогаем: тесты идут на машине, где RAZE может
        // быть установлен, и `remove_classic` стёр бы его пункты меню.
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let base = format!(
            r"Software\RAZE.Tests\{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let verb = format!(r"{base}\shell\RAZE.Delete");

        let mut report = InstallReport::default();
        remove_verbs(&hkcu, &[verb.as_str()], &mut report);
        assert!(report
            .steps
            .iter()
            .all(|s| !matches!(s, Step::Failed { .. })));
        assert!(matches!(report.steps[0], Step::Skipped { .. }));

        hkcu.create_subkey(&verb).unwrap();
        let mut report = InstallReport::default();
        remove_verbs(&hkcu, &[verb.as_str()], &mut report);
        assert!(matches!(report.steps[0], Step::Done { .. }));
        let _ = hkcu.delete_subkey_all(&base);
    }

    #[test]
    fn delayed_removal_does_not_hold_its_target_directory_open() {
        const CHILD: &str = "RAZE_TEST_DELAYED_REMOVAL";
        if let Some(path) = std::env::var_os(CHILD) {
            spawn_delayed_removal(Path::new(&path)).unwrap();
            return;
        }
        let base = std::env::temp_dir().join(format!(
            "raze-delayed-{}-{}- 'percent%[literal]",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&base).unwrap();
        std::fs::write(base.join("raze.exe"), b"disposable placeholder file").unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shell::install::tests::delayed_removal_does_not_hold_its_target_directory_open",
                "--nocapture",
            ])
            .env(CHILD, &base)
            .current_dir(&base)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while base.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(
            !base.exists(),
            "delayed helper retained its target or interpreted its name as a wildcard"
        );
    }
}
