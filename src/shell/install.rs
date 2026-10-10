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

/// CLSID нашей COM-расширки. Тот же GUID зашит в `shell-ext` и в
/// `packaging/AppxManifest.xml` (тест ниже сверяет манифест с этой
/// константой). Менять только вместе.
pub const COMMAND_CLSID: &str = "{8E5B2C41-6D3A-4F17-9C0B-51D2A7E4F108}";

/// Имя пакета для `Get-AppxPackage` / `Remove-AppxPackage`.
const PACKAGE_NAME: &str = "RAZE.ShellExtension";
const PACKAGE_FILE: &str = "RAZE.msix";
const CERT_FILE: &str = "RAZE.cer";

/// Отпечаток (SHA-1) сертификата, которым подписан `RAZE.msix`. Доверие в
/// `TrustedPeople` выдаётся только сертификату с этим отпечатком: так
/// случайно подменённый или повреждённый `RAZE.cer` не попадёт в хранилище.
/// Это защита от ошибки, а не от злоумышленника с правами пользователя: тот
/// мог бы заменить и сам `raze.exe`. При пересоздании сертификата константу
/// надо обновить (`packaging\build-package.ps1` это проверяет).
pub const PINNED_CERT_THUMBPRINT: &str = "1CDEBA01251B61E3E40F7465A8253BA276ED204F";

/// Куда всё ставится по умолчанию.
pub fn default_install_dir() -> PathBuf {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| "C:\\".into());
    PathBuf::from(local).join("Programs").join("RAZE")
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
        !self.steps
            .iter()
            .any(|s| matches!(s, Step::Failed { .. }))
    }
}

/// Ставит или снимает интеграцию. `uninstall = false` — установка.
pub fn install(source_dir: &Path, uninstall: bool) -> InstallReport {
    let mut report = InstallReport {
        install_dir: default_install_dir(),
        ..Default::default()
    };

    if uninstall {
        remove_classic(&mut report);
        remove_modern(&mut report);
        return report;
    }

    let dir = report.install_dir.clone();
    // Зарегистрированный пакет держит `raze_shell.dll` загруженной, и файл
    // нельзя заменить. Снимаем регистрацию, ниже она ставится заново.
    let _ = unregister_package();
    copy_binaries(source_dir, &dir, &mut report);
    forget_legacy_registration(&dir);
    write_classic(&dir, &mut report);
    register_modern(&dir, &mut report);
    report
}

/// Раскладывает исполняемые файлы, DLL и файлы пакета в каталог установки.
fn copy_binaries(source: &Path, target: &Path, report: &mut InstallReport) {
    if let Err(error) = std::fs::create_dir_all(target) {
        report.failed("create install directory", error.to_string());
        return;
    }
    // DLL крейта `raze-shell` собирается как `raze_shell.dll`:
    // cargo не разрешает дефисы в именах lib-таргетов.
    let wanted = ["raze.exe", "raze-app.exe", "raze_shell.dll", PACKAGE_FILE, CERT_FILE];
    let mut copied = 0usize;
    for name in wanted {
        let from = source.join(name);
        let Ok(bytes) = std::fs::read(&from) else {
            report.skipped(format!("{name} (not found in {})", source.display()));
            continue;
        };
        let to = target.join(name);
        if let Err(error) = std::fs::write(&to, &bytes) {
            report.failed(format!("copy {name}"), error.to_string());
            continue;
        }
        copied += 1;
    }
    if copied > 0 {
        report.done(format!("installed {copied} file(s) into {}", target.display()));
    }
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
        Err(error) => report.skipped(format!("modern Windows 11 menu was not registered: {error}")),
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
            key.set_value("HasLUAShield", &"").map_err(|e| e.to_string())?;

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
        let result = run_elevated_powershell(&script);
        if certificate_trusted().unwrap_or(false) {
            let reason = result.err().unwrap_or_else(|| "the certificate is still trusted".into());
            report.skipped(format!("the RAZE certificate was not removed from LocalMachine\\TrustedPeople ({reason})"));
        } else {
            report.done("removed the RAZE certificate from LocalMachine\\TrustedPeople");
        }
    }

    // Остаток старых установок.
    let _ = RegKey::predef(HKEY_CURRENT_USER)
        .delete_subkey_all(format!(r"Software\Classes\CLSID\{COMMAND_CLSID}"));

    let dir = default_install_dir();
    // Каталог удаляем только если он действительно наш и там нечего терять.
    if dir.join("raze.exe").exists() {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => report.done(format!("removed {}", dir.display())),
            Err(error) => report.failed(format!("remove {}", dir.display()), error.to_string()),
        }
    } else {
        report.skipped(format!("nothing to remove in {}", dir.display()));
    }
}

fn label() -> &'static str {
    crate::tr!("Delete with RAZE", "Удалить с помощью RAZE")
}

/// Регистрация разреженного пакета: сначала доверие сертификату, затем
/// `Add-AppxPackage` с каталогом установки как внешним расположением.
/// Вынесено отдельно, чтобы вызывать и без полной установки.
pub fn register_sparse_package() -> Result<(), String> {
    let dir = default_install_dir();
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
    let launched = run_elevated_powershell(&script);
    if certificate_trusted()? {
        Ok(())
    } else {
        Err(match launched {
            Err(error) => format!("the certificate was not trusted (was the UAC prompt declined?): {error}"),
            Ok(()) => format!(
                "the certificate was not trusted: {} does not match the pinned thumbprint",
                cert.display()
            ),
        })
    }
}

fn run_powershell(script: &str) -> Result<String, String> {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Запускает скрипт в отдельном PowerShell с повышением прав. Скрипт уходит
/// в `-EncodedCommand`, а не во временный файл: файл в каталоге пользователя
/// мог бы быть подменён до запуска с правами администратора.
fn run_elevated_powershell(script: &str) -> Result<(), String> {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let launcher = format!(
        "Start-Process -FilePath powershell.exe -Verb RunAs -Wait -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{}'",
        base64(&utf16)
    );
    run_powershell(&launcher).map(|_| ())
}

/// Строка в одинарных кавычках PowerShell (внутри них ничего не
/// раскрывается, кавычка удваивается).
fn ps_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
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
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_has_the_shape_shell_expects() {
        assert!(COMMAND_CLSID.starts_with('{'));
        assert!(COMMAND_CLSID.ends_with('}'));
        assert_eq!(COMMAND_CLSID.len(), 38, "GUID with braces is 38 characters");
        assert_eq!(COMMAND_CLSID.matches('-').count(), 4);
        assert!(COMMAND_CLSID[1..37]
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn package_template_agrees_with_the_code() {
        let xml = include_str!("../../packaging/AppxManifest.xml");
        assert!(xml.contains(&COMMAND_CLSID[1..37]), "CLSID in the manifest");
        assert!(xml.contains(&format!("Name=\"{PACKAGE_NAME}\"")));
        assert!(xml.contains("Publisher=\"CN=RERCON RAZE\""));
        assert!(xml.contains("raze_shell.dll"));
        assert!(xml.contains("windows.fileExplorerContextMenus"));
        assert!(xml.contains("Version=\"@VERSION@\""), "version is filled in by build-package.ps1");
    }

    #[test]
    fn pinned_thumbprint_is_a_sha1_in_upper_hex() {
        assert_eq!(PINNED_CERT_THUMBPRINT.len(), 40);
        assert!(PINNED_CERT_THUMBPRINT
            .chars()
            .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)));
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
    fn install_dir_is_inside_local_appdata() {
        let dir = default_install_dir();
        assert!(dir.ends_with("Programs/RAZE") || dir.ends_with("Programs\\RAZE"));
    }

    #[test]
    fn removing_verbs_that_are_not_there_is_not_an_error() {
        // Повторный запуск сноса не должен падать — он просто ничего не найдёт.
        // Настоящие ключи не трогаем: тесты идут на машине, где RAZE может
        // быть установлен, и `remove_classic` стёр бы его пункты меню.
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let verb = format!(r"Software\RAZE.Tests\{}\shell\RAZE.Delete", std::process::id());

        let mut report = InstallReport::default();
        remove_verbs(&hkcu, &[verb.as_str()], &mut report);
        assert!(report.steps.iter().all(|s| !matches!(s, Step::Failed { .. })));
        assert!(matches!(report.steps[0], Step::Skipped { .. }));

        hkcu.create_subkey(&verb).unwrap();
        let mut report = InstallReport::default();
        remove_verbs(&hkcu, &[verb.as_str()], &mut report);
        assert!(matches!(report.steps[0], Step::Done { .. }));
        let _ = hkcu.delete_subkey_all(r"Software\RAZE.Tests");
    }
}
