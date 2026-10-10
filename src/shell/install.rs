//! Установка RAZE в контекстное меню Проводника — двумя независимыми
//! способами, потому что надёжность здесь важнее минимализма.
//!
//! **Классический verb** (ветка реестра) работает всегда и на любой версии
//! Windows, но в Windows 11 живёт внутри «Показать дополнительные параметры».
//!
//! **Современное меню** (`IExplorerCommand` + разреженный MSIX-пакет) — это
//! то самое меню со скриншота. Оно регистрируется на пользователя, без
//! подписи и без установки в `Program Files`.
//!
//! Если со вторым способом не выйдет (например, на редакции без
//! разреженных пакетов), первый всё равно работает — установщик сообщает об
//! этом и не считает установку неудачной.

use std::path::{Path, PathBuf};


use serde::Serialize;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

/// CLSID нашей COM-расширки. Тот же GUID зашит в `shell-ext` — он
/// загружается в процесс Проводника и не должен зависеть от основного крейта.
/// Менять только вместе с ним.
pub const COMMAND_CLSID: &str = "{8E5B2C41-6D3A-4F17-9C0B-51D2A7E4F108}";

/// Имя пакета для `Add-AppxPackage`.
const PACKAGE_NAME: &str = "RAZE.ShellExtension";
const PUBLISHER: &str = "CN=RERCON RAZE";

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
    copy_binaries(source_dir, &dir, &mut report);
    write_appx_assets(&dir, &mut report);
    register_com(&dir, &mut report);
    write_classic(&dir, &mut report);
    report
}

/// Раскладывает исполняемые файлы и DLL в каталог установки.
fn copy_binaries(source: &Path, target: &Path, report: &mut InstallReport) {
    if let Err(error) = std::fs::create_dir_all(target) {
        report.failed("create install directory", error.to_string());
        return;
    }
    // DLL крейта `raze-shell` собирается как `raze_shell.dll`:
    // cargo не разрешает дефисы в именах lib-таргетов.
    let wanted = ["raze.exe", "raze-app.exe", "raze_shell.dll"];
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

/// Пишет манифест разреженного пакета и картинку, которые нужны
/// современному меню.
fn write_appx_assets(dir: &Path, report: &mut InstallReport) {
    let manifest = dir.join("AppxManifest.xml");
    let xml = manifest_xml();
    if let Err(error) = std::fs::write(&manifest, xml) {
        report.failed("write AppxManifest.xml", error.to_string());
        return;
    }
    report.done("wrote AppxManifest.xml");

    // Логотип пакета: MSIX требует файл, даже если он не показывается.
    let logo = dir.join("logo.png");
    if let Err(error) = write_logo(&logo) {
        report.failed("write logo.png", error.to_string());
    } else {
        report.done("wrote logo.png");
    }
}

/// Уменьшает app-иконку до размера, который ожидает манифест пакета.
fn write_logo(target: &Path) -> Result<(), String> {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("icons")
        .join("icon-app.png");
    let bytes = std::fs::read(&source).map_err(|e| e.to_string())?;
    let image = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let resized = image.resize_exact(44, 44, image::imageops::FilterType::Triangle);
    resized.save_with_format(target, image::ImageFormat::Png).map_err(|e| e.to_string())
}

/// Регистрирует COM-класс в `HKCU\Software\Classes\CLSID`. Проводник сам
/// поднимет его через `DllGetClassObject`.
fn register_com(dir: &Path, report: &mut InstallReport) {
    let key = format!(r"Software\Classes\CLSID\{COMMAND_CLSID}");
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let result = (|| -> Result<(), String> {
        let (clsid, _) = hkcu.create_subkey(&key).map_err(|e| e.to_string())?;
        clsid.set_value("", &"RAZE delete command").map_err(|e| e.to_string())?;

        let (server, _) = clsid.create_subkey("InprocServer32").map_err(|e| e.to_string())?;
        server
            .set_value("", &dir.join("raze_shell.dll").to_string_lossy().to_string())
            .map_err(|e| e.to_string())?;
        // Объект не хранит состояния (всё приходит параметрами,
        // единственный глобал — атомарный счётчик), поэтому
        // модель потоков «оба» безопасна.
        server
            .set_value("ThreadingModel", &"Both")
            .map_err(|e| e.to_string())?;
        Ok(())
    })();
    match result {
        Ok(()) => report.done("registered COM class in HKCU"),
        Err(error) => report.failed("register COM class", error),
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
        report.skipped(
            "modern Windows 11 menu needs the sparse package; restart Explorer or log off to see it",
        );
    }
}

fn remove_classic(report: &mut InstallReport) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut removed = 0usize;
    for verb in VERB_KEYS {
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
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = format!(r"Software\Classes\CLSID\{COMMAND_CLSID}");
    match hkcu.delete_subkey_all(&key) {
        Ok(()) => report.done("removed COM class registration"),
        Err(error) => report.skipped(format!("COM class was not registered ({error})")),
    }

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

fn manifest_xml() -> String {
    let label = "Delete with RAZE";
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
         xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10"
         xmlns:rescap="http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities"
         IgnorableNamespaces="uap rescap">
  <Identity Name="{PACKAGE_NAME}" Publisher="{PUBLISHER}" Version="{version}" />
  <Properties>
    <DisplayName>RAZE</DisplayName>
    <PublisherDisplayName>RAZE</PublisherDisplayName>
    <Logo>logo.png</Logo>
  </Properties>
  <Dependencies>
    <TargetDeviceFamily Name="Windows.Desktop" MinVersion="10.0.22621.0" MaxVersionTested="10.0.26100.0" />
  </Dependencies>
  <Resources>
    <Resource Language="en" />
    <Resource Language="ru" />
  </Resources>
  <Applications />
  <Extensions>
    <uap:Extension Category="windows.explorerCommand" dllPath="raze_shell.dll" Clsid="{clsid}">
      <uap:ExtensionData>
        <uap:Data Name="Label" Value="{label}" />
        <uap:Data Name="Icon" Value="logo.png" />
      </uap:ExtensionData>
    </uap:Extension>
  </Extensions>
</Package>
"#,
        version = crate::VERSION,
        clsid = COMMAND_CLSID,
        label = label,
    )
}

/// Регистрация разреженного пакета средствами PowerShell. Вынесено
/// отдельно, чтобы установщик можно было вызвать и без нас.
pub fn register_sparse_package() -> Result<(), String> {
    let manifest = default_install_dir().join("AppxManifest.xml");
    if !manifest.exists() {
        return Err(format!("{} is missing", manifest.display()));
    }
    let script = format!(
        "Add-AppxPackage -DisableDevelopmentMode -Register \"{}\"",
        manifest.display()
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
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
    fn manifest_contains_the_command_declaration() {
        let xml = manifest_xml();
        assert!(xml.contains(COMMAND_CLSID));
        assert!(xml.contains("windows.explorerCommand"));
        assert!(xml.contains("raze_shell.dll"));
        assert!(xml.contains(crate::VERSION));
    }

    #[test]
    fn install_dir_is_inside_local_appdata() {
        let dir = default_install_dir();
        assert!(dir.ends_with("Programs/RAZE") || dir.ends_with("Programs\\RAZE"));
    }

    #[test]
    fn uninstall_without_installation_is_not_an_error() {
        // Повторный запуск сноса не должен падать — он просто ничего не найдёт.
        let mut report = InstallReport::default();
        remove_classic(&mut report);
        assert!(report.steps.iter().all(|s| !matches!(s, Step::Failed { .. })));
    }
}