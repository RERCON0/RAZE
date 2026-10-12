//! Задача целиком: что выбрано, что с этим будет сделано и что получилось.
//!
//! Один и тот же код обслуживает CLI, окно и вызов из контекстного меню.
//! Иначе каждая точка входа по-своему решала бы, что считать проблемой и
//! что делать с Корзиной — и рано или поздно разошлась бы.

use std::path::PathBuf;

use serde::Serialize;

use crate::engine::error::NtError;
use crate::engine::path::NtPath;
use crate::engine::privs;
use crate::engine::reboot;
use crate::engine::recycle::{self, Disposition, RecycleVerdict};
use crate::engine::remove::{self, Failure, Outcome, RemoveOptions};
use crate::engine::scan::{self, Entry, ScanOptions, ScanSummary};
use crate::protect;
use crate::report::{Refused, Report, TargetReport};

/// Согласие на безвозвратное удаление вместо отказавшей Корзины.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecycleFallback {
    /// Отказ Корзины — это отказ: без явного согласия ничего не удаляется.
    #[default]
    Never,
    /// Пользователь заранее разрешил стереть безвозвратно, если Корзина откажет.
    Permanent,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Никогда не отправлять в Корзину.
    pub permanent: bool,
    /// Назначить удаление на следующую перезагрузку вместо удаления сейчас.
    pub reboot: bool,
    /// Что делать, если Корзина отказала без согласия на безвозвратное удаление.
    pub recycle_fallback: RecycleFallback,
    /// Забирать владение и выдавать права при ACCESS_DENIED.
    pub take_ownership: bool,
    /// Отправлять `WM_CLOSE` процессам, которые держат файлы.
    pub close_locks: bool,
    /// Ничего не удалять: только показать, что произойдёт.
    pub dry_run: bool,
    pub retries: u32,
    pub scan: ScanOptions,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            permanent: false,
            reboot: false,
            recycle_fallback: RecycleFallback::Never,
            take_ownership: true,
            close_locks: false,
            dry_run: false,
            retries: 3,
            scan: ScanOptions::default(),
        }
    }
}

impl Options {
    fn remove(&self) -> RemoveOptions {
        RemoveOptions {
            take_ownership: self.take_ownership,
            close_locks: self.close_locks,
            retries: self.retries,
            lock_timeout: std::time::Duration::from_secs(5),
            dry_run: self.dry_run,
        }
    }
}

/// Одна цель со всем, что про неё известно до удаления.
///
/// `path` — `None`, когда путь не удалось даже разобрать: такие цели всё
/// равно попадают в отчёт с причиной, но трогать их нельзя.
#[derive(Clone, Debug)]
pub struct Target {
    pub display: String,
    pub path: Option<NtPath>,
    pub entries: Vec<Entry>,
    pub summary: ScanSummary,
    pub verdict: RecycleVerdict,
    /// Режим, выбранный при планировании: Корзина, безвозвратно или при
    /// перезагрузке. Параметры запуска могут его переопределить.
    pub mode: Disposition,
    /// Причина отказа от удаления.
    pub refused: Option<String>,
}

impl Target {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.summary.files == 0
            && self.summary.directories == 0
            && self.summary.links == 0
    }

    fn refusal(display: String, reason: String) -> Self {
        Self {
            display,
            path: None,
            entries: Vec::new(),
            summary: ScanSummary::default(),
            verdict: RecycleVerdict {
                reusable: false,
                reason: reason.clone(),
            },
            mode: Disposition::Permanent,
            refused: Some(reason),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub targets: Vec<TargetSummary>,
    pub refused: Vec<Refused>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TargetSummary {
    pub path: String,
    pub files: usize,
    pub directories: usize,
    pub links: usize,
    pub bytes: u64,
    pub troubled: usize,
    pub problems: Vec<String>,
    pub mode: Disposition,
    pub reason: String,
}

/// Разбирает список путей и сканирует каждый. Ничего не удаляет.
pub fn plan(paths: &[PathBuf], options: &Options) -> Vec<Target> {
    // Привилегии нужны уже при планировании: без Backup/Restore повышенный
    // процесс не откроет даже корень папки с чужим ACL, и план покажет «пусто».
    // Включение запоминается, повторные вызовы ничего не стоят.
    privs::enable_backup_restore();
    let mut targets = Vec::new();
    for raw in paths {
        let display = raw.to_string_lossy().to_string();
        let path = match NtPath::new(raw) {
            Ok(path) => path,
            Err(error) => {
                // Показать пользователю всё равно нужно — показываем то,
                // что он ввёл, и честно говорим, почему это не сработает.
                targets.push(Target::refusal(display, error.to_string()));
                continue;
            }
        };
        if let Err(reason) = protect::check(&path) {
            let text = reason.message();
            targets.push(Target::refusal(path.to_string_lossy(), text));
            continue;
        }
        let (entries, summary) = scan::scan(&path, &options.scan);
        let (verdict, mode) = if options.reboot {
            (
                RecycleVerdict {
                    reusable: false,
                    reason: crate::tr!(
                        "removal at the next reboot was requested",
                        "выбрано удаление при следующей перезагрузке"
                    )
                    .to_string(),
                },
                Disposition::Reboot,
            )
        } else if options.permanent {
            (
                RecycleVerdict {
                    reusable: false,
                    reason: crate::tr!(
                        "permanent removal was requested",
                        "выбрано безвозвратное удаление"
                    )
                    .to_string(),
                },
                Disposition::Permanent,
            )
        } else {
            let mut verdict = recycle::is_recyclable(&path);
            // Предел Корзины тома зависит от размера, а он известен только после
            // обхода: объект больше предела оболочка удалила бы молча.
            if verdict.reusable {
                if let Some(reason) = recycle::bin_settings(&path).refusal(summary.bytes) {
                    verdict = RecycleVerdict {
                        reusable: false,
                        reason,
                    };
                }
            }
            let mode = if verdict.reusable {
                Disposition::RecycleBin
            } else {
                Disposition::Permanent
            };
            (verdict, mode)
        };
        targets.push(Target {
            display: path.to_string_lossy(),
            path: Some(path),
            entries,
            summary,
            verdict,
            mode,
            refused: None,
        });
    }
    targets
}

/// Короткое описание плана для подтверждения и для CLI.
pub fn describe(targets: &[Target]) -> Summary {
    Summary {
        targets: targets
            .iter()
            .map(|target| TargetSummary {
                path: target.display.clone(),
                files: target.summary.files,
                directories: target.summary.directories,
                links: target.summary.links,
                bytes: target.summary.bytes,
                troubled: target.summary.troubled,
                problems: target
                    .entries
                    .iter()
                    .filter(|entry| entry.has_trouble())
                    .map(|entry| {
                        format!(
                            "{} — {}",
                            entry.name,
                            entry
                                .blockers
                                .iter()
                                .map(|b| b.label())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                    .take(50)
                    .collect(),
                mode: target.mode,
                reason: target.verdict.reason.clone(),
            })
            .collect(),
        refused: targets
            .iter()
            .filter_map(|target| {
                target.refused.as_ref().map(|reason| Refused {
                    path: target.display.clone(),
                    reason: reason.clone(),
                })
            })
            .collect(),
    }
}

/// Выполняет план. Отчёт честно разделяет «ушло в Корзину», «стёрто
/// безвозвратно», «назначено к перезагрузке» и «не получилось».
pub fn execute(targets: &[Target], options: &Options) -> Report {
    execute_with(targets, options, &|path| {
        recycle::send_to_recycle(&[path.to_path()])
    })
}

/// Тело [`execute`] с подменяемым слоем Корзины: тесты моделируют отказ
/// оболочки, не имея под рукой настоящего отказа.
fn execute_with(
    targets: &[Target],
    options: &Options,
    send_to_bin: &dyn Fn(&NtPath) -> Result<Disposition, NtError>,
) -> Report {
    let mut report = Report::default();
    for target in targets {
        if let Some(reason) = &target.refused {
            report.refused.push(Refused {
                path: target.display.clone(),
                reason: reason.clone(),
            });
            continue;
        }
        let Some(path) = target.path.as_ref() else {
            continue;
        };
        // План мог лежать в окне минутами, а путь за это время — стать другим
        // (родитель подменён ссылкой, папку переименовали). Запретные места
        // проверяются заново перед самим удалением, а не только при разборе.
        if let Err(reason) = protect::check(path) {
            report.refused.push(Refused {
                path: target.display.clone(),
                reason: reason.message(),
            });
            continue;
        }

        // Текущие параметры важнее устаревшего плана: окно могло лежать
        // открытым, а режим за это время переключили.
        let mode = if options.reboot {
            Disposition::Reboot
        } else if options.permanent {
            Disposition::Permanent
        } else {
            target.mode
        };
        let reason = if options.reboot {
            crate::tr!(
                "removal at the next reboot was requested",
                "выбрано удаление при следующей перезагрузке"
            )
            .to_string()
        } else if options.permanent {
            crate::tr!(
                "permanent removal was requested",
                "выбрано безвозвратное удаление"
            )
            .to_string()
        } else {
            target.verdict.reason.clone()
        };

        let mut target_report = TargetReport {
            path: target.display.clone(),
            native_path: path.to_path(),
            disposition: mode,
            reason,
            bin_refused: None,
            removed_files: 0,
            removed_directories: 0,
            removed_links: 0,
            freed_bytes: 0,
            notes: Vec::new(),
            failures: Vec::new(),
            dry_run: options.dry_run,
        };

        let outcome = match mode {
            Disposition::Reboot => reboot::schedule_tree(path, &options.remove()),
            Disposition::Permanent => remove::remove_tree(path, &options.remove()),
            Disposition::RecycleBin => {
                if options.dry_run {
                    remove::remove_tree(path, &options.remove())
                } else {
                    match send_to_bin(path) {
                        Ok(disposition) => {
                            target_report.disposition = disposition;
                            // Считаем, что ушло всё содержимое цели: Корзина
                            // работает по дереву, а не пофайлово.
                            target_report.removed_files = target.summary.files;
                            target_report.removed_directories = target.summary.directories;
                            target_report.removed_links = target.summary.links;
                            // Перенос в Корзину сохраняет данные на диске.
                            if disposition == Disposition::Permanent {
                                target_report.freed_bytes = target.summary.bytes;
                            }
                            Outcome::default()
                        }
                        Err(error)
                            if error.code() == windows::Win32::Foundation::ERROR_CANCELLED.0 =>
                        {
                            // Отказ от операции Корзины — не разрешение стереть
                            // насовсем.
                            report.cancelled = true;
                            Outcome {
                                failures: vec![Failure {
                                    path: target.display.clone(),
                                    code: error.code(),
                                    message: error.message(),
                                    holders: Vec::new(),
                                }],
                                ..Default::default()
                            }
                        }
                        Err(error) if options.recycle_fallback == RecycleFallback::Permanent => {
                            // Согласие дано заранее — переходим к безвозвратному
                            // удалению и говорим об этом в отчёте.
                            target_report.disposition = Disposition::Permanent;
                            target_report.reason = crate::tr_format!(
                                "the Recycle Bin refused ({reason}); permanent removal was allowed",
                                "Корзина отказала ({reason}); разрешено безвозвратное удаление",
                                reason = error.message()
                            );
                            let mut outcome = remove::remove_tree(path, &options.remove());
                            outcome.notes.push(remove::Note::RecycleRefused {
                                reason: error.message(),
                            });
                            outcome
                        }
                        Err(error) => {
                            // Без явного согласия отказ Корзины — просто отказ:
                            // объект остаётся, отчёт говорит почему.
                            target_report.bin_refused = Some(error.message());
                            Outcome {
                                failures: vec![Failure {
                                    path: target.display.clone(),
                                    code: error.code(),
                                    message: crate::tr_format!(
                                        "the Recycle Bin refused: {message}; nothing was deleted",
                                        "Корзина отказала: {message}; ничего не удалено",
                                        message = error.message()
                                    ),
                                    holders: Vec::new(),
                                }],
                                ..Default::default()
                            }
                        }
                    }
                }
            }
        };

        absorb(&mut target_report, outcome);
        report.targets.push(target_report);
        if report.cancelled {
            break;
        }
    }
    report.refresh_needs_elevation();
    report
}

fn absorb(target: &mut TargetReport, outcome: Outcome) {
    target.removed_files += outcome.removed_files;
    target.removed_directories += outcome.removed_directories;
    target.removed_links += outcome.removed_links;
    target.freed_bytes += outcome.freed_bytes;
    target.notes.extend(outcome.notes);
    target.failures.extend(outcome.failures);
}

/// Готова ли задача к запуску: есть хоть что-то удаляемое. Пустая папка тоже
/// работа: в счётчиках её содержимого нули, но сама она есть и подлежит
/// удалению (раньше `raze` молча выходил, не тронув пустую «зависшую» папку).
pub fn has_work(targets: &[Target]) -> bool {
    targets.iter().any(|t| {
        t.refused.is_none()
            && ((t.summary.files + t.summary.directories + t.summary.links) > 0
                || t.path.as_ref().is_some_and(remove::present))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("raze-job-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn plan_refuses_a_volume_root_and_scans_a_folder() {
        let base = scratch("plan");
        std::fs::write(base.join("f.txt"), b"x").unwrap();
        let mut paths = vec![base.clone()];
        let mut options = Options::default();
        options.scan.probe_locks = false;

        let targets = plan(&paths, &options);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].summary.files, 1);
        assert!(targets[0].refused.is_none());
        assert!(has_work(&targets));

        paths.push(PathBuf::from("C:\\"));
        paths.push(PathBuf::from("C:\\."));
        let targets = plan(&paths, &options);
        assert!(
            targets[1].refused.is_some(),
            "корень тома обязан быть отклонён"
        );
        assert!(targets[2].refused.is_some(), "и в любой другой записи тоже");
        assert!(!has_work(&targets[1..]));
    }

    #[test]
    fn dry_run_plan_reports_without_touching_disk() {
        let base = scratch("dry");
        std::fs::write(base.join("f.txt"), b"x").unwrap();
        let options = Options {
            dry_run: true,
            ..Default::default()
        };
        let targets = plan(&[base.clone()], &options);
        let report = execute(&targets, &options);
        assert_eq!(report.removed_total(), 1);
        assert!(base.join("f.txt").exists());
        assert!(report.targets[0].dry_run);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_recycled_file_does_not_report_freed_space() {
        let base = scratch("recycle-accounting");
        let file = base.join("one.bin");
        std::fs::write(&file, b"occupied in bin").unwrap();
        let options = Options::default();
        let targets = plan(std::slice::from_ref(&file), &options);
        assert!(targets[0].verdict.reusable, "{}", targets[0].verdict.reason);

        let report = execute(&targets, &options);
        let removed = !file.exists();
        std::fs::remove_dir_all(base).unwrap();
        assert!(!report.failed(), "{:?}", report.targets);
        assert!(removed);
        assert_eq!(report.targets[0].disposition, Disposition::RecycleBin);
        assert_eq!(report.targets[0].removed_files, 1);
        assert_eq!(report.targets[0].freed_bytes, 0);
        assert_eq!(report.freed_bytes(), 0);
    }

    #[test]
    fn a_single_file_target_is_planned_and_removed() {
        // Регрессия: файл-цель читался как «нечитаемая папка» из нуля
        // объектов, `has_work` был ложным, и удаление не запускалось.
        let base = scratch("single-file");
        let file = base.join("one.bin");
        std::fs::write(&file, b"hello").unwrap();
        let options = Options {
            permanent: true,
            ..Default::default()
        };

        let targets = plan(&[file.clone()], &options);
        assert_eq!(targets[0].summary.files, 1);
        assert_eq!(targets[0].summary.directories, 0);
        assert_eq!(targets[0].summary.bytes, 5);
        assert!(has_work(&targets));

        let report = execute(&targets, &options);
        assert!(!report.failed(), "{:?}", report.targets);
        assert!(!file.exists());
        assert!(base.exists(), "удаляется файл, а не его папка");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_empty_folder_is_work_and_is_removed() {
        // Регрессия: у пустой папки в плане нули, `has_work` был ложным, и
        // удалить её было нечем.
        let base = scratch("empty-folder");
        let empty = base.join("stuck");
        std::fs::create_dir_all(&empty).unwrap();
        let options = Options {
            permanent: true,
            ..Default::default()
        };

        let targets = plan(&[empty.clone()], &options);
        assert!(has_work(&targets));
        let report = execute(&targets, &options);
        assert!(!report.failed(), "{:?}", report.targets);
        assert!(!empty.exists());

        // А несуществующая цель — не работа.
        assert!(!has_work(&plan(&[empty], &options)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn execute_checks_the_protected_places_again() {
        // План собран, а путь потом стал запретным: удаление не должно
        // полагаться на проверку, сделанную при разборе.
        let windows = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let forged = Target {
            display: format!("{windows}\\System32"),
            path: Some(NtPath::new(std::path::Path::new(&format!("{windows}\\System32"))).unwrap()),
            entries: Vec::new(),
            summary: ScanSummary::default(),
            verdict: RecycleVerdict {
                reusable: false,
                reason: String::new(),
            },
            mode: Disposition::Permanent,
            refused: None,
        };
        let options = Options {
            dry_run: true,
            ..Default::default()
        };
        let report = execute(&[forged], &options);
        assert_eq!(report.refused.len(), 1);
        assert!(report.targets.is_empty());
    }

    #[test]
    fn raw_utf16_target_preserves_a_replacement_character_sibling() {
        use std::os::windows::ffi::OsStringExt;
        let base = scratch("raw-utf16");
        let file = base.join(std::ffi::OsString::from_wide(&[
            b'r' as u16,
            b'a' as u16,
            b'w' as u16,
            0xD800,
        ]));
        let sibling = base.join("raw\u{FFFD}");
        std::fs::write(&file, b"intended").unwrap();
        std::fs::write(&sibling, b"preserve").unwrap();
        let options = Options {
            permanent: true,
            ..Default::default()
        };
        let targets = plan(&[file.clone()], &options);
        let report = execute(&targets, &options);
        assert!(!report.failed(), "{:?}", report.targets);
        assert!(!file.exists());
        assert_eq!(std::fs::read(&sibling).unwrap(), b"preserve");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn current_permanent_option_overrides_an_old_recycle_plan() {
        let base = scratch("changed-mode");
        let file = base.join("one.bin");
        std::fs::write(&file, b"keep during preview").unwrap();
        let mut targets = plan(&[file.clone()], &Options::default());
        targets[0].verdict.reusable = true;
        let options = Options {
            permanent: true,
            dry_run: true,
            ..Default::default()
        };
        let report = execute(&targets, &options);
        assert_eq!(report.targets[0].disposition, Disposition::Permanent);
        assert!(file.exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn summary_lists_problems_by_name() {
        let base = scratch("problems");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/ok.txt"), b"x").unwrap();
        let targets = plan(&[base.clone()], &Options::default());
        let summary = describe(&targets);
        assert_eq!(summary.targets[0].files, 1);
        assert!(summary.targets[0].bytes > 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_refused_recycle_bin_never_deletes_without_consent() {
        let base = scratch("bin-refused");
        let file = base.join("keep.bin");
        std::fs::write(&file, b"keep").unwrap();
        let targets = plan(std::slice::from_ref(&file), &Options::default());
        assert_eq!(targets[0].mode, Disposition::RecycleBin);

        let report = execute_with(&targets, &Options::default(), &|_| Err(NtError(31)));
        assert!(file.exists(), "без согласия объект обязан остаться");
        assert!(report.targets[0].bin_refused.is_some());
        assert_eq!(report.removed_total(), 0);
        assert_eq!(report.freed_bytes(), 0);
        assert!(report.failed());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn an_allowed_recycle_failure_deletes_permanently_and_says_so() {
        let base = scratch("bin-allowed");
        let file = base.join("gone.bin");
        std::fs::write(&file, b"erase me").unwrap();
        let targets = plan(std::slice::from_ref(&file), &Options::default());
        let options = Options {
            recycle_fallback: RecycleFallback::Permanent,
            ..Default::default()
        };

        let report = execute_with(&targets, &options, &|_| Err(NtError(31)));
        assert!(!file.exists());
        assert!(!report.failed(), "{:?}", report.targets);
        assert_eq!(report.targets[0].disposition, Disposition::Permanent);
        assert!(report.targets[0].bin_refused.is_none());
        assert!(report.targets[0]
            .notes
            .iter()
            .any(|note| matches!(note, remove::Note::RecycleRefused { .. })));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_reboot_plan_is_marked_and_dry_run_touches_nothing() {
        let base = scratch("reboot-plan");
        std::fs::write(base.join("f.txt"), b"x").unwrap();
        let options = Options {
            reboot: true,
            ..Default::default()
        };
        let targets = plan(&[base.clone()], &options);
        assert!(targets[0].refused.is_none());
        assert_eq!(targets[0].mode, Disposition::Reboot);

        let options = Options {
            reboot: true,
            dry_run: true,
            ..Default::default()
        };
        let report = execute(&targets, &options);
        assert!(base.join("f.txt").exists());
        assert_eq!(report.targets[0].disposition, Disposition::Reboot);
        assert_eq!(report.targets[0].removed_files, 1);
        std::fs::remove_dir_all(base).unwrap();
    }
}
