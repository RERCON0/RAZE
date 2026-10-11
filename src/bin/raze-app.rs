//! Native RAZE window. Planning/removal use the same job engine as the CLI.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;
use raze_rs::engine::elevation::{build_arguments, is_elevated, relaunch_elevated};
use raze_rs::engine::recycle::Disposition;
use raze_rs::job::{self, Options, Target};
use raze_rs::report::{human_bytes, Report};
use raze_rs::{i18n, tr, tr_format, TELEGRAM_URL, VERSION};

#[path = "raze-app/style.rs"]
mod style;
#[path = "raze-app/view.rs"]
mod view;
#[cfg(feature = "ui-preview")]
#[path = "raze-app/preview.rs"]
mod preview;

const MAX_LOG_LINES: usize = 500;
const MAX_TARGETS: usize = 512;
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
    Permanent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Planning,
    Confirming(Confirm),
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
    running_options: Options,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    #[cfg(test)]
    geometry: view::Geometry,
    #[cfg(feature = "ui-preview")]
    preview: Option<preview::Preview>,
}

impl RazeApp {
    fn new(ctx: &egui::Context) -> Self {
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
            status: None,
            status_error: false,
            show_log: false,
            close_locks: false,
            running_options: Options::default(),
            tx,
            rx,
            #[cfg(test)]
            geometry: view::Geometry::default(),
            #[cfg(feature = "ui-preview")]
            preview: None,
        }
    }

    fn is_preview(&self) -> bool {
        #[cfg(feature = "ui-preview")]
        if self.preview.is_some() {
            return true;
        }
        false
    }

    fn log(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
        if self.log.len() > MAX_LOG_LINES {
            self.log.drain(..self.log.len() - MAX_LOG_LINES);
        }
    }

    fn add_paths(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        if self.phase != Phase::Idle || self.is_preview() {
            return;
        }
        let mut fresh = Vec::new();
        for path in paths {
            let display = path.to_string_lossy();
            if self.plan.iter().any(|t| t.display.eq_ignore_ascii_case(&display))
                || fresh.iter().any(|p: &PathBuf| p == &path)
            {
                continue;
            }
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
        let (ctx, tx) = (ctx.clone(), self.tx.clone());
        thread::spawn(move || {
            let targets = job::plan(&fresh, &Options::default());
            let _ = tx.send(Msg::Plan(targets));
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
        self.plan.iter().zip(&self.rows)
            .filter(|(t, r)| r.selected && t.refused.is_none())
            .map(|(t, _)| t.clone()).collect()
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
            close_locks: self.close_locks,
            ..Options::default()
        }
    }

    fn start_removal(&mut self, ctx: &egui::Context, options: Options) {
        if self.phase.working() || self.is_preview() {
            return;
        }
        let targets = self.selected_targets();
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
            };
            self.log(tr_format!(
                "{path} — {mode}: {files} files, {dirs} folders, {size}",
                "{path} — {mode}: файлов: {files}, папок: {dirs}, {size}",
                path = target.path, mode = disposition,
                files = target.removed_files, dirs = target.removed_directories,
                size = human_bytes(target.freed_bytes)
            ));
            for failure in &target.failures {
                self.log(format!("  {} [{}]: {}", failure.path, failure.code, failure.message));
            }
            for note in &target.notes {
                self.log(format!("  {}", note_display(note)));
            }
        }
        for refused in &report.refused {
            self.log(tr_format!(
                "{path} — refused: {reason}", "{path} — отказано: {reason}",
                path = refused.path, reason = refused.reason
            ));
        }
        self.phase = Phase::Idle;
        self.status_error = report.failed();
        self.show_log = report.failed();
        self.status = Some(report.summary_line());

        // Keep the retry paths before removing completed cards from the view.
        if report.needs_elevation && !is_elevated() && !self.running_options.dry_run {
            let paths: Vec<PathBuf> = report.targets.iter()
                .filter(|t| t.failures.iter().any(|f| f.code == 5))
                .map(|t| PathBuf::from(&t.path)).collect();
            self.relaunch_as_admin(&paths);
        }
        if !self.running_options.dry_run {
            let mut kept = Vec::new();
            let mut rows = Vec::new();
            for (target, row) in self.plan.drain(..).zip(self.rows.drain(..)) {
                let completed = row.selected && report.targets.iter()
                    .any(|r| r.path == target.display && r.ok());
                if !completed {
                    kept.push(target);
                    rows.push(row);
                }
            }
            self.plan = kept;
            self.rows = rows;
        }
    }

    fn relaunch_as_admin(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() || self.is_preview() {
            return;
        }
        let Some(exe) = std::env::current_exe().ok().map(|e| e.with_file_name("raze.exe")) else {
            return;
        };
        match relaunch_elevated(&exe, &elevation_arguments(paths, &self.running_options)) {
            Ok(true) => self.log(tr!(
                "Handed off to the administrator CLI. Its report is separate.",
                "Передано CLI с правами администратора. Его отчёт — отдельно."
            )),
            Ok(false) => self.log(tr!("Administrator rights were not granted.", "Права администратора не выданы.")),
            Err(error) => self.log(tr_format!(
                "Cannot restart elevated: {error}", "Не удалось повысить права: {error}"
            )),
        }
    }

    fn drain_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Msg::Plan(targets) => {
                    for target in targets {
                        self.rows.push(RowState { selected: target.refused.is_none(), expanded: false });
                        self.plan.push(target);
                    }
                    self.phase = Phase::Idle;
                    self.log(tr_format!(
                        "Scan complete. Targets: {count}.",
                        "Сканирование закончено. Целей: {count}.", count = self.plan.len()
                    ));
                }
                Msg::Report(report) => self.handle_report(report),
            }
        }
    }
}

fn elevation_arguments(paths: &[PathBuf], options: &Options) -> String {
    let mut args = vec!["--elevated".to_string(), "--yes".to_string()];
    if options.permanent { args.push("--permanent".into()); }
    if options.close_locks { args.push("--close-locks".into()); }
    if !options.take_ownership { args.push("--keep-owners".into()); }
    args.extend(["--lang".into(), i18n::language().code().into(), "--".into()]);
    args.extend(paths.iter().map(|p| p.to_string_lossy().into_owned()));
    build_arguments(&args)
}

fn note_display(note: &raze_rs::engine::remove::Note) -> String {
    use raze_rs::engine::remove::Note;
    match note {
        Note::ReadOnlyCleared { path } => tr_format!("Read-only cleared: {path}", "Снят атрибут чтения: {path}"),
        Note::OwnershipTaken { path } => tr_format!("Ownership taken: {path}", "Сменён владелец: {path}"),
        Note::AccessGranted { path } => tr_format!("Access granted: {path}", "Выданы права: {path}"),
        Note::ProcessCloseRequested { pid, name } => tr_format!("Close requested: {name} ({pid})", "Запрошено закрытие: {name} ({pid})"),
        Note::Vanished { path } => tr_format!("Already gone: {path}", "Уже отсутствует: {path}"),
        Note::DeletePending { path } => tr_format!("Delete pending: {path}", "Ожидает удаления: {path}"),
    }
}

impl eframe::App for RazeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // eframe re-applies the OS scale factor on DPI/monitor changes, so the
        // pin is re-asserted every frame, not just at startup.
        style::pin_pixel_scale(ctx, false);
        self.drain_messages();
        if ctx.input(|i| i.viewport().close_requested()) && self.phase.working() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.add_paths(ctx, dropped);
        self.render(ctx);
        #[cfg(feature = "ui-preview")]
        if let Some(preview) = &mut self.preview {
            preview.capture(ctx);
        }
    }
}

fn main() -> eframe::Result {
    std::panic::set_hook(Box::new(|info| fatal_dialog(
        tr!("RAZE — internal error", "RAZE — внутренняя ошибка"),
        &tr_format!("RAZE could not continue.\n\n{info}", "RAZE не смог продолжить работу.\n\n{info}")
    )));
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size(style::WINDOW_SIZE)
        .with_min_inner_size(style::WINDOW_MIN_SIZE)
        .with_title("RAZE — by rercon prod.");
    #[cfg(windows)]
    { viewport = viewport.with_decorations(false); }
    if let Some(icon) = app_icon() { viewport = viewport.with_icon(icon); }
    #[cfg(feature = "ui-preview")]
    { viewport = preview::configure_viewport(viewport); }
    let result = eframe::run_native("RAZE", eframe::NativeOptions { viewport, ..Default::default() },
        Box::new(|cc| {
            let mut app = RazeApp::new(&cc.egui_ctx);
            #[cfg(feature = "ui-preview")]
            preview::configure_app(&mut app, &cc.egui_ctx);
            // Keep one creation path for production, preview, and layout tests.
            let _ = &mut app;
            Ok(Box::new(app))
        }));
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
#[path = "raze-app/tests.rs"]
mod tests;
