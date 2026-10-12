//! Отчёт о том, что именно произошло.
//!
//! Удаление — операция без возможности отмены, поэтому отчёт здесь не
//! украшение: по нему человек понимает, куда делись файлы, что ушло в
//! Корзину, что стёрто навсегда и что не удалось. Всё, что не удалось,
//! перечислено с кодом ошибки и именем мешавшего процесса.

use serde::Serialize;

use crate::engine::error::NtError;
use crate::engine::recycle::Disposition;
use crate::engine::remove::{Failure, Note};

#[derive(Clone, Debug, Serialize)]
pub struct TargetReport {
    pub path: String,
    /// Original native path for UI correlation and elevation; display text is lossy.
    #[serde(skip)]
    pub native_path: std::path::PathBuf,
    pub disposition: Disposition,
    /// Почему выбран именно такой режим (например, «зарезервированное имя NUL»).
    pub reason: String,
    /// Корзина отказала: ничего не удалено, согласия на безвозвратное удаление
    /// не было. Причина — здесь; повтор с явным разрешением выполняет фронтенд.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bin_refused: Option<String>,
    pub removed_files: usize,
    pub removed_directories: usize,
    pub removed_links: usize,
    /// Освобождено байт. В режиме `reboot` — сколько освободится при
    /// перезагрузке: сейчас ничего не удалено.
    pub freed_bytes: u64,
    pub notes: Vec<Note>,
    pub failures: Vec<Failure>,
    pub dry_run: bool,
}

impl TargetReport {
    pub fn removed_total(&self) -> usize {
        self.removed_files + self.removed_directories + self.removed_links
    }

    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub targets: Vec<TargetReport>,
    pub refused: Vec<Refused>,
    /// Хоть раз понадобились права администратора — GUI/CLI предложат перезапуск.
    pub needs_elevation: bool,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Refused {
    pub path: String,
    pub reason: String,
}

impl Report {
    pub fn removed_total(&self) -> usize {
        self.targets.iter().map(TargetReport::removed_total).sum()
    }

    pub fn freed_bytes(&self) -> u64 {
        self.targets.iter().map(|t| t.freed_bytes).sum()
    }

    pub fn failures(&self) -> usize {
        self.targets.iter().map(|t| t.failures.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.refused.is_empty()
    }

    pub fn failed(&self) -> bool {
        self.cancelled || self.failures() > 0 || !self.refused.is_empty()
    }

    /// Пересчитывает `needs_elevation` по ошибкам в отчёте: после того как
    /// состав целей изменился (повтор отказавших), старое значение врёт.
    pub fn refresh_needs_elevation(&mut self) {
        self.needs_elevation = self.targets.iter().any(|target| {
            target
                .failures
                .iter()
                .any(|failure| NtError(failure.code).needs_elevation())
        });
    }

    /// Одна строка итогов — и для GUI, и для CLI.
    pub fn summary_line(&self) -> String {
        if self.cancelled {
            return crate::tr!("Cancelled", "Отменено").to_string();
        }
        let dry = self.targets.first().map(|t| t.dry_run).unwrap_or(false);
        let reboot = self
            .targets
            .iter()
            .any(|t| t.disposition == Disposition::Reboot);
        let failed = self.failures() + self.refused.len();
        let verb = match (dry, reboot) {
            (true, true) => crate::tr!(
                "Would schedule for reboot",
                "Будет запланировано к перезагрузке"
            ),
            (false, true) => crate::tr!("Scheduled for reboot", "Запланировано к перезагрузке"),
            (true, false) => crate::tr!("Would remove", "Будет удалено"),
            (false, false) => crate::tr!("Removed", "Удалено"),
        };
        if failed == 0 {
            crate::tr_format!(
                "{verb}: {items}",
                "{verb}: {items}",
                verb = verb,
                items = self.removed_total()
            )
        } else {
            crate::tr_format!(
                "{verb}: {items}, failed: {failed}",
                "{verb}: {items}, с ошибкой: {failed}",
                verb = verb,
                items = self.removed_total(),
                failed = failed
            )
        }
    }

    /// Отчёт для unit tests без файловой системы.
    #[cfg(test)]
    pub fn demo() -> Self {
        Self {
            targets: vec![TargetReport {
                path: r"C:\tmp\NUL".into(),
                native_path: std::path::PathBuf::from(r"C:\tmp\NUL"),
                disposition: Disposition::Permanent,
                reason: "reserved name NUL".into(),
                bin_refused: None,
                removed_files: 1,
                removed_directories: 0,
                removed_links: 0,
                freed_bytes: 0,
                notes: vec![],
                failures: vec![],
                dry_run: false,
            }],
            ..Default::default()
        }
    }
}

/// «12,3 МБ» — без битых символов и с правильными единицами.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        crate::tr_format!("{bytes} B", "{bytes} Б")
    } else {
        crate::tr_format!(
            "{value:.1} {unit}",
            "{value:.1} {unit}",
            value = value,
            unit = UNITS[unit]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_add_up() {
        let mut report = Report::demo();
        report.targets[0].removed_directories = 2;
        assert_eq!(report.removed_total(), 3);
        assert!(!report.failed());
    }

    #[test]
    fn json_distinguishes_preview_from_actual_deletion() {
        let mut report = Report::demo();
        report.targets[0].dry_run = true;
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["targets"][0]["dry_run"], true);
        report.targets[0].dry_run = false;
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["targets"][0]["dry_run"], false);
    }

    #[test]
    fn cancellation_is_not_success() {
        let report = Report {
            cancelled: true,
            ..Report::default()
        };
        assert!(report.failed());
        assert_eq!(report.removed_total(), 0);
    }
}
