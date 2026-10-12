//! Native RAZE window. Planning/removal use the same job engine as the CLI.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;
use raze_rs::engine::elevation::{build_arguments, is_elevated, relaunch_elevated};
use raze_rs::engine::path::{comparison_key, eq_ignore_case};
use raze_rs::engine::recycle::Disposition;
use raze_rs::job::{self, Options, RecycleFallback, Target};
use raze_rs::report::{human_bytes, Report};
use raze_rs::{i18n, tr, tr_format, TELEGRAM_URL, VERSION};

#[path = "raze-app/dialog.rs"]
mod dialog;
#[path = "raze-app/style.rs"]
mod style;
#[path = "raze-app/view.rs"]
mod view;

const MAX_LOG_LINES: usize = 500;
const MAX_TARGETS: usize = 512;
const MAX_LISTED_ENTRIES: usize = 20_000;
const ICON_PNG: &[u8] = include_bytes!("../../icons/icon.png");

fn app_icon() -> Option<egui::IconData> {
    use image::GenericImageView;
    let image = image::load_from_memory(ICON_PNG).ok()?;
    let (width, height) = image.dimensions();
    let side = width.min(height);
    if side == 0 {
        return None;
    }
    let rgba = image
        .crop_imm((width - side) / 2, (height - side) / 2, side, side)
        .resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    Some(egui::IconData {
        width: 256,
        height: 256,
        rgba: rgba.into_raw(),
    })
}

fn fatal_dialog(title: &str, text: &str) {
    rfd::MessageDialog::new()
        .set_title(title)
        .set_description(text)
        .set_level(rfd::MessageLevel::Error)
        .show();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Recycle,
    Reboot,
    Permanent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Planning,
    Confirming(Confirm),
    /// Корзина отказала: без согласия ничего не удалено, спросить о безвозвратном.
    ConfirmingFallback,
    Removing,
}

impl Phase {
    fn working(self) -> bool {
        matches!(self, Self::Planning | Self::Removing)
    }
}

enum Msg {
    Plan(Vec<Target>),
    Report(Report),
}

#[derive(Clone, Copy, Default)]
struct RowState {
    selected: bool,
    expanded: bool,
}

#[derive(Default)]
struct Totals {
    targets: usize,
    files: usize,
    directories: usize,
    links: usize,
    bytes: u64,
    troubled: usize,
    permanent: usize,
}

struct RazeApp {
    phase: Phase,
    mode: Confirm,
    plan: Vec<Target>,
    rows: Vec<RowState>,
    typed: String,
    log: Vec<String>,
    dark: bool,
    maximized: bool,
    status: Option<String>,
    status_error: bool,
    show_log: bool,
    close_locks: bool,
    /// Цели, которые Корзина отказалась принять: ждут решения пользователя.
    fallback: Vec<PathBuf>,
    running_options: Options,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl RazeApp {
    fn new(ctx: &egui::Context) -> Self {
        let status = match i18n::load_language() {
            Ok(language) => {
                i18n::set_language(language);
                None
            }
            Err(error) => {
                i18n::set_language(i18n::Language::En);
                Some(tr_format!(
                    "Could not load saved language: {error}",
                    "Не удалось загрузить сохранённый язык: {error}"
                ))
            }
        };
        let status_error = status.is_some();
        style::setup_fonts(ctx);
        style::setup_theme(ctx, true);
        style::pin_pixel_scale(ctx, true);
        let (tx, rx) = mpsc::channel();
        Self {
            phase: Phase::Idle,
            mode: Confirm::Recycle,
            plan: Vec::new(),
            rows: Vec::new(),
            typed: String::new(),
            log: Vec::new(),
            dark: true,
            maximized: false,
            status,
            status_error,
            show_log: false,
            close_locks: false,
            fallback: Vec::new(),
            running_options: Options::default(),
            tx,
            rx,
        }
    }

    fn log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > MAX_LOG_LINES {
            self.log.drain(..self.log.len() - MAX_LOG_LINES);
        }
    }

    fn add_paths(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        // Вызывается на каждом кадре: без перетаскивания разбирать план не нужно.
        if paths.is_empty() || self.phase != Phase::Idle {
            return;
        }
        let mut fresh = Vec::new();
        let mut seen: Vec<Vec<u16>> = self
            .plan
            .iter()
            .filter_map(|target| target.path.as_ref())
            .map(|path| comparison_key(&path.to_path()))
            .collect();
        for path in paths {
            // Сравнение по разобранному пути и по правилам Windows, как в CLI:
            // `C:\a\` и `c:/a` — одна цель, как и `C:\Данные` с `c:\данные`.
            let key = comparison_key(&path);
            if seen.iter().any(|previous| eq_ignore_case(previous, &key)) {
                continue;
            }
            seen.push(key);
            if self.plan.len() + fresh.len() >= MAX_TARGETS {
                self.log(tr_format!(
                    "Target limit: {max}. Use the CLI for larger batches.",
                    "Лимит целей: {max}. Для больших списков используйте CLI.",
                    max = MAX_TARGETS
                ));
                break;
            }
            fresh.push(path);
        }
        if fresh.is_empty() {
            return;
        }
        self.status = None;
        self.log(tr_format!(
            "Scanning targets: {count}…",
            "Сканирую целей: {count}…",
            count = fresh.len()
        ));
        self.phase = Phase::Planning;
        let entry_budget = MAX_LISTED_ENTRIES.saturating_sub(
            self.plan
                .iter()
                .map(|target| target.entries.len())
                .sum::<usize>(),
        );
        let (ctx, tx) = (ctx.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(Msg::Plan(plan_paths(fresh, entry_budget)));
            ctx.request_repaint();
        });
    }

    fn add_typed(&mut self, ctx: &egui::Context) {
        let text = self.typed.trim().trim_matches('"');
        if !text.is_empty() && self.phase == Phase::Idle {
            let path = PathBuf::from(text);
            self.add_paths(ctx, vec![path]);
            self.typed.clear();
        }
    }

    fn selected_targets(&self) -> Vec<Target> {
        self.plan
            .iter()
            .zip(&self.rows)
            .filter(|(t, r)| r.selected && t.refused.is_none())
            .map(|(t, _)| clone_target(t))
            .collect()
    }

    /// Цели, которым Корзина отказала: именно их предлагается стереть
    /// безвозвратно после явного согласия.
    fn fallback_targets(&self) -> Vec<Target> {
        self.plan
            .iter()
            .filter(|t| {
                t.path
                    .as_ref()
                    .is_some_and(|p| self.fallback.contains(&p.to_path()))
            })
            .map(clone_target)
            .collect()
    }

    fn totals(&self) -> Totals {
        let mut total = Totals::default();
        for (target, row) in self.plan.iter().zip(&self.rows) {
            if !row.selected || target.refused.is_some() {
                continue;
            }
            total.targets += 1;
            total.files += target.summary.files;
            total.directories += target.summary.directories;
            total.links += target.summary.links;
            total.bytes = total.bytes.saturating_add(target.summary.bytes);
            total.troubled += target.summary.troubled;
            total.permanent += usize::from(!target.verdict.reusable);
        }
        total
    }

    fn base_options(&self) -> Options {
        Options {
            permanent: self.mode == Confirm::Permanent,
            reboot: self.mode == Confirm::Reboot,
            close_locks: self.close_locks,
            ..Options::default()
        }
    }

    fn start_removal(&mut self, ctx: &egui::Context, options: Options) {
        let targets = self.selected_targets();
        self.start_removal_targets(ctx, targets, options);
    }

    fn start_removal_targets(
        &mut self,
        ctx: &egui::Context,
        targets: Vec<Target>,
        options: Options,
    ) {
        if self.phase.working() {
            return;
        }
        if targets.is_empty() {
            self.phase = Phase::Idle;
            return;
        }
        self.running_options = options.clone();
        self.phase = Phase::Removing;
        self.status = None;
        self.log(tr_format!(
            "Working on targets: {count}…",
            "Обрабатываю целей: {count}…",
            count = targets.len()
        ));
        let (ctx, tx) = (ctx.clone(), self.tx.clone());
        thread::spawn(move || {
            let report = job::execute(&targets, &options);
            let _ = tx.send(Msg::Report(report));
            ctx.request_repaint();
        });
    }

    fn handle_report(&mut self, report: Report) {
        for target in &report.targets {
            let disposition = match target.disposition {
                Disposition::RecycleBin => tr!("Recycle Bin", "Корзина"),
                Disposition::Permanent => tr!("permanent", "безвозвратно"),
                Disposition::Reboot => tr!("at the next reboot", "при следующей перезагрузке"),
            };
            self.log(tr_format!(
                "{path} — {mode}: {files} files, {dirs} folders, {size}",
                "{path} — {mode}: файлов: {files}, папок: {dirs}, {size}",
                path = target.path,
                mode = disposition,
                files = target.removed_files,
                dirs = target.removed_directories,
                size = human_bytes(target.freed_bytes)
            ));
            for failure in &target.failures {
                self.log(format!(
                    "  {} [{}]: {}",
                    failure.path, failure.code, failure.message
                ));
            }
            for note in &target.notes {
                self.log(format!("  {}", note.describe()));
            }
        }
        for refused in &report.refused {
            self.log(tr_format!(
                "{path} — refused: {reason}",
                "{path} — отказано: {reason}",
                path = refused.path,
                reason = refused.reason
            ));
        }
        self.phase = Phase::Idle;
        self.status_error = report.failed();
        self.show_log = report.failed();
        self.status = Some(report.summary_line());

        // Keep the retry paths before removing completed cards from the view.
        if report.needs_elevation && !is_elevated() && !self.running_options.dry_run {
            let paths: Vec<PathBuf> = report
                .targets
                .iter()
                .filter(|t| {
                    t.failures
                        .iter()
                        .any(|f| raze_rs::engine::error::NtError(f.code).needs_elevation())
                })
                .map(|t| t.native_path.clone())
                .collect();
            self.relaunch_as_admin(&paths);
        }
        if !self.running_options.dry_run && !report.cancelled {
            let mut kept = Vec::new();
            let mut rows = Vec::new();
            for (target, row) in self.plan.drain(..).zip(self.rows.drain(..)) {
                let completed = row.selected
                    && report.targets.iter().any(|r| {
                        r.ok()
                            && target
                                .path
                                .as_ref()
                                .is_some_and(|p| p.to_path() == r.native_path)
                    });
                if !completed {
                    kept.push(target);
                    rows.push(row);
                }
            }
            self.plan = kept;
            self.rows = rows;
        }

        // Корзина отказала: без явного согласия объект остался на месте.
        // Предлагаем решение диалогом, а не удаляем за спиной пользователя.
        if !self.running_options.dry_run
            && !report.cancelled
            && self.running_options.recycle_fallback == RecycleFallback::Never
        {
            let refused: Vec<PathBuf> = report
                .targets
                .iter()
                .filter(|target| target.bin_refused.is_some())
                .map(|target| target.native_path.clone())
                .collect();
            if !refused.is_empty() {
                self.fallback = refused;
                self.phase = Phase::ConfirmingFallback;
            }
        }
    }

    /// Согласие на безвозвратное удаление целей, которым отказала Корзина.
    fn confirm_fallback(&mut self, ctx: &egui::Context) {
        let targets = self.fallback_targets();
        self.fallback.clear();
        self.phase = Phase::Idle;
        let options = Options {
            permanent: true,
            recycle_fallback: RecycleFallback::Never,
            ..self.running_options.clone()
        };
        self.start_removal_targets(ctx, targets, options);
    }

    fn dismiss_fallback(&mut self) {
        self.fallback.clear();
        self.phase = Phase::Idle;
    }

    fn relaunch_as_admin(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let Some(exe) = std::env::current_exe()
            .ok()
            .map(|e| e.with_file_name("raze.exe"))
        else {
            return;
        };
        match relaunch_elevated(&exe, &elevation_arguments(paths, &self.running_options)) {
            Ok(true) => self.log(tr!(
                "Handed off to the administrator CLI. Its report is separate.",
                "Передано CLI с правами администратора. Его отчёт — отдельно."
            )),
            Ok(false) => self.log(tr!(
                "Administrator rights were not granted.",
                "Права администратора не выданы."
            )),
            Err(error) => self.log(tr_format!(
                "Cannot restart elevated: {error}",
                "Не удалось повысить права: {error}"
            )),
        }
    }

    fn drain_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Msg::Plan(targets) => {
                    for target in targets {
                        self.rows.push(RowState {
                            selected: target.refused.is_none(),
                            expanded: false,
                        });
                        self.plan.push(target);
                    }
                    self.phase = Phase::Idle;
                    self.log(tr_format!(
                        "Scan complete. Targets: {count}.",
                        "Сканирование закончено. Целей: {count}.",
                        count = self.plan.len()
                    ));
                }
                Msg::Report(report) => self.handle_report(report),
            }
        }
    }
}

fn plan_paths(paths: Vec<PathBuf>, mut entry_budget: usize) -> Vec<Target> {
    let mut targets = Vec::with_capacity(paths.len());
    for path in paths {
        let mut options = Options::default();
        // Troublesome entries may use a second max_listed allowance.
        options.scan.max_listed = options.scan.max_listed.min(entry_budget / 2);
        for mut target in job::plan(std::slice::from_ref(&path), &options) {
            let retained = target.entries.len().min(entry_budget);
            target.summary.unlisted = target
                .summary
                .unlisted
                .saturating_add(target.entries.len() - retained);
            target.entries.truncate(retained);
            entry_budget -= retained;
            targets.push(target);
        }
    }
    targets
}

fn elevation_arguments(paths: &[PathBuf], options: &Options) -> OsString {
    let mut args: Vec<OsString> = vec!["--elevated".into(), "--yes".into()];
    if options.permanent {
        args.push("--permanent".into());
    }
    if options.reboot {
        args.push("--reboot".into());
    }
    if options.recycle_fallback == RecycleFallback::Permanent {
        args.push("--permanent-on-bin-failure".into());
    }
    if options.close_locks {
        args.push("--close-locks".into());
    }
    if !options.take_ownership {
        args.push("--keep-owners".into());
    }
    args.extend(["--lang".into(), i18n::language().code().into(), "--".into()]);
    args.extend(paths.iter().map(|p| p.as_os_str().to_os_string()));
    build_arguments(&args)
}

fn clone_target(target: &Target) -> Target {
    Target {
        display: target.display.clone(),
        path: target.path.clone(),
        entries: Vec::new(),
        summary: target.summary.clone(),
        verdict: target.verdict.clone(),
        mode: target.mode,
        refused: target.refused.clone(),
    }
}

impl eframe::App for RazeApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Worker results must also be consumed while the window is minimized.
        self.drain_messages();
        if ctx.input(|i| i.viewport().close_requested()) && self.phase.working() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // eframe re-applies the OS scale factor on DPI/monitor changes, so the
        // pin is re-asserted every frame, not just at startup.
        style::pin_pixel_scale(ctx, false);
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        self.add_paths(ctx, dropped);
        self.render(ui);
    }
}

fn main() -> eframe::Result {
    // Режим окна-вопроса для `raze.exe` из контекстного меню: без главного окна
    // и без диалога о внутренней ошибке — вызывающий сам спросит иначе.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|mode| mode == "--dialog") {
        std::process::exit(
            raze_rs::dialog::Spec::parse(&args)
                .map_or(raze_rs::dialog::EXIT_UNAVAILABLE, dialog::run),
        );
    }
    std::panic::set_hook(Box::new(|info| {
        fatal_dialog(
            tr!("RAZE — internal error", "RAZE — внутренняя ошибка"),
            &tr_format!(
                "RAZE could not continue.\n\n{info}",
                "RAZE не смог продолжить работу.\n\n{info}"
            ),
        )
    }));
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(style::WINDOW_SIZE)
        .with_min_inner_size(style::WINDOW_MIN_SIZE)
        .with_title("RAZE — by rercon prod.");
    #[cfg(windows)]
    {
        viewport = viewport.with_decorations(false);
    }
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    let result = eframe::run_native(
        "RAZE",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(RazeApp::new(&cc.egui_ctx)))),
    );
    if let Err(error) = &result {
        fatal_dialog(tr!("RAZE — could not open window", "RAZE — не удалось открыть окно"),
            &tr_format!(
                "{error}\n\nThe GUI needs OpenGL 2.1+. The CLI (raze.exe) does not require graphics.",
                "{error}\n\nОкну нужен OpenGL 2.1+. CLI (raze.exe) не требует графики."
            ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use raze_rs::engine::path::NtPath;
    use raze_rs::engine::recycle::RecycleVerdict;
    use raze_rs::report::TargetReport;

    #[test]
    fn a_hidden_window_cannot_close_during_removal() {
        let ctx = egui::Context::default();
        let mut app = RazeApp::new(&ctx);
        app.phase = Phase::Removing;
        let mut input = egui::RawInput::default();
        let viewport = input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap();
        viewport.minimized = Some(true);
        viewport.events.push(egui::ViewportEvent::Close);
        let output = ctx.run_logic(&input, |ctx| {
            eframe::App::logic(&mut app, ctx, &mut eframe::Frame::_new_kittest());
        });
        assert!(output.viewport_commands[&egui::ViewportId::ROOT]
            .iter()
            .any(|command| matches!(command, egui::ViewportCommand::CancelClose)));
        assert!(matches!(app.phase, Phase::Removing));
    }

    fn target(path: PathBuf) -> Target {
        let path = NtPath::new(&path).unwrap();
        Target {
            display: path.to_string_lossy(),
            path: Some(path),
            entries: Vec::new(),
            summary: Default::default(),
            verdict: RecycleVerdict {
                reusable: true,
                reason: String::new(),
            },
            mode: Disposition::RecycleBin,
            refused: None,
        }
    }

    fn completed(target: &Target, dry_run: bool) -> TargetReport {
        TargetReport {
            path: target.display.clone(),
            native_path: target.path.as_ref().unwrap().to_path(),
            disposition: Disposition::RecycleBin,
            reason: String::new(),
            bin_refused: None,
            removed_files: 1,
            removed_directories: 0,
            removed_links: 0,
            freed_bytes: 1,
            notes: Vec::new(),
            failures: Vec::new(),
            dry_run,
        }
    }

    #[test]
    fn list_budget_preserves_full_counts_and_every_requested_target() {
        let scratch = std::env::temp_dir().join(format!(
            "raze-gui-budget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let folder = scratch.join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        for name in ["one", "two", "three"] {
            std::fs::write(folder.join(name), b"x").unwrap();
        }
        let file = scratch.join("file");
        std::fs::write(&file, b"x").unwrap();
        let plan = plan_paths(vec![folder, file], 2);
        std::fs::remove_dir_all(scratch).unwrap();
        assert_eq!(plan.len(), 2);
        assert!(plan.iter().all(|target| target.refused.is_none()));
        assert!(
            plan.iter()
                .map(|target| target.entries.len())
                .sum::<usize>()
                <= 2
        );
        assert_eq!(
            plan.iter()
                .map(|target| target.summary.files)
                .sum::<usize>(),
            4
        );
        assert_eq!(
            plan.iter().map(|target| target.summary.bytes).sum::<u64>(),
            4
        );
        assert_eq!(
            plan.iter()
                .map(|target| target.entries.len() + target.summary.unlisted)
                .sum::<usize>(),
            4
        );
    }

    #[test]
    fn preview_keeps_the_targets_available_for_actual_removal() {
        let mut app = RazeApp::new(&egui::Context::default());
        app.plan
            .push(target(PathBuf::from(r"C:\raze-ui-test\file")));
        app.rows.push(RowState {
            selected: true,
            expanded: true,
        });
        app.running_options.dry_run = true;
        let report = Report {
            targets: vec![completed(&app.plan[0], true)],
            ..Default::default()
        };
        app.handle_report(report);
        assert_eq!(app.plan.len(), 1);
        assert!(app.rows[0].selected && app.rows[0].expanded);
        assert!(matches!(app.phase, Phase::Idle));
    }

    #[test]
    fn a_refused_recycle_bin_asks_before_permanent_deletion() {
        let mut app = RazeApp::new(&egui::Context::default());
        app.plan
            .push(target(PathBuf::from(r"C:\raze-ui-test\bin-refused")));
        app.rows.push(RowState {
            selected: true,
            expanded: false,
        });
        let mut report = completed(&app.plan[0], false);
        report.removed_files = 0;
        report.freed_bytes = 0;
        report.bin_refused = Some("denied".into());
        report.failures.push(raze_rs::engine::remove::Failure {
            path: report.path.clone(),
            code: 31,
            message: String::new(),
            holders: Vec::new(),
        });
        app.handle_report(Report {
            targets: vec![report],
            ..Default::default()
        });

        assert!(matches!(app.phase, Phase::ConfirmingFallback));
        assert_eq!(app.fallback.len(), 1);
        assert_eq!(app.plan.len(), 1, "цель остаётся для повтора");
        assert!(app.status_error);
    }

    #[test]
    fn only_completed_selected_targets_disappear() {
        let mut app = RazeApp::new(&egui::Context::default());
        for name in ["completed", "failed", "unselected"] {
            app.plan
                .push(target(PathBuf::from(r"C:\raze-ui-test").join(name)));
            app.rows.push(RowState {
                selected: name != "unselected",
                expanded: false,
            });
        }
        let successful = completed(&app.plan[0], false);
        let mut failed = completed(&app.plan[1], false);
        failed.failures.push(raze_rs::engine::remove::Failure {
            path: failed.path.clone(),
            code: 32,
            message: String::new(),
            holders: Vec::new(),
        });
        app.handle_report(Report {
            targets: vec![successful, failed],
            ..Default::default()
        });
        assert_eq!(app.plan.len(), 2);
        assert!(app.plan[0].display.ends_with("failed"));
        assert!(app.plan[1].display.ends_with("unselected"));
        assert!(app.rows[0].selected);
        assert!(!app.rows[1].selected);
        assert!(app.status_error);
    }

    #[cfg(windows)]
    #[test]
    fn lossy_display_collision_does_not_hide_an_unremoved_native_target() {
        use std::os::windows::ffi::OsStringExt;
        let mut app = RazeApp::new(&egui::Context::default());
        let prefix: Vec<u16> = r"C:\raze-ui-test\".encode_utf16().collect();
        for name in [0xd800, 0xfffd] {
            let mut wide = prefix.clone();
            wide.push(name);
            app.plan
                .push(target(PathBuf::from(OsString::from_wide(&wide))));
            app.rows.push(RowState {
                selected: true,
                expanded: false,
            });
        }
        assert_eq!(app.plan[0].display, app.plan[1].display);
        let native_unremoved = app.plan[0].path.as_ref().unwrap().to_path();
        let report = Report {
            targets: vec![completed(&app.plan[1], false)],
            ..Default::default()
        };
        app.handle_report(report);
        assert_eq!(app.plan.len(), 1);
        assert_eq!(
            app.plan[0].path.as_ref().unwrap().to_path(),
            native_unremoved
        );
    }
}
