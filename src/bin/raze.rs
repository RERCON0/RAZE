//! RAZE — консольный интерфейс.
//!
//! Работает и как обычная утилита (`raze C:\путь`), и как цель контекстного
//! меню (`raze --from-shell --mode "%1"`). Второй вариант не должен
//! требовать подтверждения: человек уже сказал «удалить» самим кликом.

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use clap::{ArgAction, Parser};

use raze_rs::engine::elevation::{
    build_arguments, is_elevated, needs_elevation, relaunch_elevated, run_elevated_and_wait,
};
use raze_rs::engine::path::NtPath;
use raze_rs::engine::recycle::Disposition;
use raze_rs::engine::scan::ScanOptions;
use raze_rs::job::{self, Options, RecycleFallback};
use raze_rs::report::{human_bytes, Report};
use raze_rs::shell;
use raze_rs::{banner, i18n, tr, tr_format, TELEGRAM_URL, VERSION};

#[derive(Parser, Debug)]
#[command(
    name = "raze",
    version = VERSION,
    about = "RAZE — removes stubborn Windows files and folders",
    long_about = None,
    disable_help_subcommand = true
)]
struct Cli {
    /// Files and folders to remove.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Show what would be removed, change nothing.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Remove permanently, bypassing the Recycle Bin.
    #[arg(short, long)]
    permanent: bool,

    /// Explicitly request the default Recycle Bin mode where possible.
    #[arg(long, conflicts_with = "permanent")]
    recycle: bool,

    /// Schedule deletion at the next reboot instead of deleting now.
    #[arg(long, conflicts_with_all = ["permanent", "recycle"])]
    reboot: bool,

    /// Erase permanently if the Recycle Bin refuses an item; never silent.
    #[arg(long, conflicts_with_all = ["permanent", "reboot"])]
    permanent_on_bin_failure: bool,

    /// Do not ask for confirmation.
    #[arg(short = 'y', long)]
    yes: bool,

    /// Close the programs that hold the files (WM_CLOSE, not taskkill).
    #[arg(short = 'c', long)]
    close_locks: bool,

    /// Do not take ownership or change access rights.
    #[arg(long)]
    keep_owners: bool,

    /// Also look for the processes holding each file (slower).
    #[arg(short = 'L', long)]
    find_locks: bool,

    /// Machine-readable report on stdout.
    #[arg(long)]
    json: bool,

    /// Interface language: en or ru.
    #[arg(short, long)]
    lang: Option<String>,

    /// Internal: this process was started by the Explorer context menu.
    #[arg(long, hide = true)]
    from_shell: bool,

    /// Internal: target passed by Explorer (%1 for items, %V for background).
    #[arg(short, long, value_name = "PATH", hide = true)]
    mode: Vec<PathBuf>,

    /// Internal: this process already runs elevated.
    #[arg(long, hide = true)]
    elevated: bool,

    /// Install RAZE into Program Files with the Explorer context menu (asks for administrator rights).
    #[arg(long, conflicts_with = "uninstall")]
    install: bool,

    /// Remove RAZE and its Explorer context menu (asks for administrator rights).
    #[arg(long)]
    uninstall: bool,

    /// Register the modern Windows 11 menu package (needs PowerShell).
    #[arg(long, exclusive = true)]
    register_package: bool,

    /// Internal: where the elevated copy writes the install/uninstall report.
    #[arg(long, hide = true, value_name = "PATH")]
    report_file: Option<PathBuf>,

    /// Print the machine-readable plan and exit.
    #[arg(long)]
    plan_only: bool,

    /// Print extra detail.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

fn main() {
    let cli = Cli::parse();
    let language = match cli.lang.as_deref() {
        Some(code) => match i18n::Language::parse(code) {
            Ok(language) => language,
            Err(message) => {
                eprintln!("--lang: {message}");
                std::process::exit(2);
            }
        },
        None => i18n::load_language().unwrap_or_else(|error| {
            eprintln!("Cannot load saved language: {error}");
            i18n::Language::En
        }),
    };
    i18n::set_language(language);

    let code = run(cli);
    std::process::exit(code);
}

fn run(cli: Cli) -> i32 {
    if cli.install {
        return shell_install(&cli, false);
    }
    if cli.uninstall {
        return shell_install(&cli, true);
    }
    if cli.register_package {
        return match shell::install::register_sparse_package() {
            Ok(()) => {
                println!(
                    "{}",
                    tr_format!(
                        "The Windows 11 context menu is registered.",
                        "Меню Windows 11 зарегистрировано."
                    )
                );
                0
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    tr_format!(
                        "Could not register the modern menu: {error}\nThe classic menu keeps working.",
                        "Не удалось зарегистрировать современное меню: {error}\nКлассическое меню продолжает работать."
                    )
                );
                1
            }
        };
    }

    let targets_paths = collect_paths(&cli);
    if targets_paths.is_empty() {
        if cli.json || cli.plan_only {
            eprintln!("Usage: raze [OPTIONS] <PATH>...");
        } else {
            print_usage();
        }
        return 2;
    }

    let options = Options {
        permanent: cli.permanent,
        reboot: cli.reboot,
        recycle_fallback: if cli.permanent_on_bin_failure {
            RecycleFallback::Permanent
        } else {
            RecycleFallback::Never
        },
        take_ownership: !cli.keep_owners,
        close_locks: cli.close_locks,
        dry_run: cli.dry_run,
        retries: 3,
        scan: ScanOptions {
            probe_locks: cli.find_locks,
            ..ScanOptions::default()
        },
    };

    let planned = job::plan(&targets_paths, &options);
    let summary = job::describe(&planned);

    if cli.plan_only {
        if let Err(error) = write_json(&mut std::io::stdout().lock(), &summary) {
            eprintln!("{error}");
            return 1;
        }
        return if summary.refused.is_empty() { 0 } else { 3 };
    }
    if !cli.json {
        print_plan(&planned, &summary, cli.verbose);
    }

    if !summary.refused.is_empty() {
        for refused in &summary.refused {
            eprintln!("{}: {}", refused.path, refused.reason);
        }
    }
    if !job::has_work(&planned) {
        if cli.json {
            let report = job::execute(&planned, &options);
            if let Err(error) = write_json(&mut std::io::stdout().lock(), &report) {
                eprintln!("{error}");
                return 1;
            }
        }
        return if summary.refused.is_empty() { 0 } else { 3 };
    }

    // Подтверждение: из контекстного меню и с --yes вопросов не задаём.
    // Пробный прогон ничего не меняет, спрашивать нечего. Из меню клик
    // остаётся подтверждением только для восстановимых объектов:
    // безвозвратные и отложенные RAZE показывает диалогом.
    if !cli.dry_run && !cli.yes && !confirm_before(&cli, &planned) {
        if cli.json {
            let report = Report {
                cancelled: true,
                ..Report::default()
            };
            if let Err(error) = write_json(&mut std::io::stdout().lock(), &report) {
                eprintln!("{error}");
            }
        } else {
            println!("{}", tr!("Cancelled.", "Отменено."));
        }
        return 1;
    }

    let report = job::execute(&planned, &options);

    // A ShellExecute handoff has separate stdout and cannot return a report
    // to a script. JSON mode reports this attempt; run it elevated explicitly.
    if report.needs_elevation && !cli.dry_run && !cli.json && !cli.elevated && !is_elevated() {
        print_report(&report);
        if let Some(code) = relaunch_as_admin(&cli, &planned, &report) {
            return code;
        }
    }

    if cli.json {
        if let Err(error) = write_json(&mut std::io::stdout().lock(), &report) {
            eprintln!("{error}");
            return 1;
        }
    } else {
        print_report(&report);
    }

    if report.failed() {
        1
    } else {
        0
    }
}

fn write_json(writer: &mut impl Write, value: &impl serde::Serialize) -> std::io::Result<()> {
    serde_json::to_writer_pretty(&mut *writer, value).map_err(std::io::Error::other)?;
    writeln!(writer)
}

/// Explorer вызывает классический verb с одним или несколькими `%1`;
/// для клика по пустому месту приходит `%V` — это каталог.
fn collect_paths(cli: &Cli) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    paths.extend(cli.mode.iter().cloned());
    paths.extend(cli.paths.iter().cloned());
    dedupe(drop_empty_placeholders(paths))
}

/// Пустая строка из разворачивающегося `%1` — не цель, а мусор.
/// clap 4 не пропускает пустые значения вообще, поэтому это
/// вторая линия обороны: на случай сбора путей из других источников.
fn drop_empty_placeholders(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|path| path.as_os_str() != "")
        .collect()
}

fn dedupe(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
    let mut seen: Vec<Vec<u16>> = Vec::new();
    let mut out = Vec::new();
    for path in paths {
        // Сравниваем по разобранному пути: `C:\a\` и `C:\a` — одно и то же.
        let key = NtPath::new(&path)
            .map(|p| p.plain().as_slice().to_vec())
            .unwrap_or_else(|_| path.as_os_str().encode_wide().collect());
        if seen.iter().any(|previous| {
            previous == &key || unsafe { CompareStringOrdinal(previous, &key, true) == CSTR_EQUAL }
        }) {
            continue;
        }
        seen.push(key);
        out.push(path);
    }
    out
}

/// Установка и снятие меню. Каталог установки — Program Files, туда пишут
/// только администраторы, поэтому без прав процесс перезапускает себя через UAC
/// (один запрос на всё, включая доверие сертификату) и печатает отчёт
/// повышенной копии, которая пишет его в файл.
fn shell_install(cli: &Cli, uninstall: bool) -> i32 {
    // `--report-file` ничего не отменяет: без прав процесс в любом случае
    // перезапускается через UAC. Иначе вызов с этим параметром без повышения
    // выполнял бы запись в Program Files и реестр на полпути и без отчёта.
    if needs_elevation() {
        return install_elevated(uninstall);
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let report = shell::install(&exe_dir, uninstall);
    let (lines, code) = install_summary(&report, uninstall);
    match &cli.report_file {
        Some(path) => {
            // Только в новый файл: `create_new` не пишет в уже существующее имя,
            // в том числе в символическую ссылку, заранее положенную процессом
            // пользователя, — повышенная запись иначе перезаписала бы любой файл.
            if let Err(error) = write_install_report(path, &lines) {
                eprintln!("{}: {error}", path.display());
                return 1;
            }
        }
        None => {
            for line in lines {
                println!("{line}");
            }
        }
    }
    code
}

fn write_install_report(path: &std::path::Path, lines: &[String]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(lines.join("\n").as_bytes())
}

fn install_elevated(uninstall: bool) -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!(
            "{}",
            tr!(
                "Cannot find this program.",
                "Не удалось определить путь программы."
            )
        );
        return 1;
    };
    let report_file =
        std::env::temp_dir().join(format!("raze-install-{}.txt", unpredictable_name()));
    let line = build_arguments(&[
        OsString::from(if uninstall {
            "--uninstall"
        } else {
            "--install"
        }),
        "--lang".into(),
        i18n::language().code().into(),
        "--report-file".into(),
        report_file.as_os_str().to_os_string(),
    ]);
    eprintln!(
        "{}",
        tr!(
            "RAZE lives in Program Files, which only administrators can write: confirm the Windows prompt.",
            "RAZE ставится в Program Files, куда пишут только администраторы: подтвердите запрос Windows."
        )
    );
    let result = run_elevated_and_wait(&exe, &line, std::time::Duration::from_secs(600));
    let text = std::fs::read_to_string(&report_file);
    let _ = std::fs::remove_file(&report_file);
    match result {
        Ok(code) => {
            match text {
                Ok(text) => println!("{text}"),
                Err(_) => {
                    eprintln!(
                        "{}",
                        tr!(
                            "The elevated copy left no report.",
                            "Повышенная копия не оставила отчёта."
                        )
                    );
                    return 1;
                }
            }
            code as i32
        }
        // 1223 — человек отказал в запросе UAC.
        Err(error) if error.code() == 1223 => {
            eprintln!(
                "{}",
                tr!(
                    "Administrator rights were not granted. Nothing was changed.",
                    "Права администратора не выданы. Ничего не изменено."
                )
            );
            1
        }
        Err(error) => {
            eprintln!(
                "{}: {error}",
                tr!("Cannot restart", "Не удалось перезапустить")
            );
            1
        }
    }
}

/// 128 бит из случайного ключа `RandomState`: имя файла отчёта нельзя угадать
/// и занять заранее. (Новый файл `create_new` защищает и от занятого имени.)
fn unpredictable_name() -> String {
    use std::hash::{BuildHasher, Hasher};
    let part = |salt: u64| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt ^ u64::from(std::process::id()));
        hasher.finish()
    };
    format!("{:016x}{:016x}", part(1), part(2))
}

/// Строки отчёта об установке и код выхода.
fn install_summary(report: &shell::InstallReport, uninstall: bool) -> (Vec<String>, i32) {
    let mut lines: Vec<String> = report
        .steps
        .iter()
        .map(|step| match step {
            shell::Step::Done { what } => format!("[ok]   {what}"),
            shell::Step::Skipped { what } => format!("[skip] {what}"),
            shell::Step::Failed { what, error } => format!("[fail] {what}: {error}"),
        })
        .collect();
    if uninstall && report.ok() {
        lines.push(
            tr!(
                "Done. The context menu is back to normal.",
                "Готово. Контекстное меню снова обычное."
            )
            .to_string(),
        );
        return (lines, 0);
    }
    if !report.ok() {
        lines.push(
            tr!(
                "Some steps failed; see the errors above.",
                "Часть шагов не удалась; ошибки перечислены выше."
            )
            .to_string(),
        );
        return (lines, 1);
    }
    lines.push(tr_format!(
        "Installed into {}. If the Windows 11 menu does not show RAZE yet, open a new Explorer window or sign out and back in.",
        "Установлено в {}. Если RAZE ещё нет в меню Windows 11, откройте новое окно Проводника или перезайдите в сеанс.",
        report.install_dir.display()
    ));
    (lines, 0)
}

fn relaunch_as_admin(cli: &Cli, planned: &[job::Target], report: &Report) -> Option<i32> {
    if !needs_elevation() {
        return None;
    }
    let Ok(exe) = std::env::current_exe() else {
        return None;
    };
    let retry: Vec<&job::Target> = planned
        .iter()
        .filter(|target| {
            report.targets.iter().any(|r| {
                target
                    .path
                    .as_ref()
                    .is_some_and(|path| path.to_path() == r.native_path)
                    && r.failures.iter().any(|failure| {
                        raze_rs::engine::error::NtError(failure.code).needs_elevation()
                    })
            })
        })
        .collect();
    if retry.is_empty() {
        return None;
    }
    let line = build_arguments(&elevation_arguments(cli, retry));
    if cli.verbose > 0 {
        eprintln!(
            "{}",
            tr_format!(
                "Requesting admin rights…",
                "Запрашиваю права администратора…"
            )
        );
    }
    match relaunch_elevated(&exe, &line) {
        Ok(true) => {
            println!(
                "{}",
                tr!(
                    "Handed off to the administrator CLI. Its report is separate.",
                    "Передано CLI с правами администратора. Его отчёт — отдельно."
                )
            );
            Some(0)
        }
        Ok(false) => {
            eprintln!(
                "{}",
                tr!(
                    "Administrator rights were not granted.",
                    "Права администратора не выданы."
                )
            );
            Some(1)
        }
        Err(error) => {
            eprintln!(
                "{}: {error}",
                tr!("Cannot restart", "Не удалось перезапустить")
            );
            Some(1)
        }
    }
}

/// Копия с правами администратора должна сделать ровно то, о чём просили:
/// с теми же параметрами и над теми же объектами. Раньше передавался только
/// первый аргумент командной строки, да и тот лишь если начинался с `--`:
/// `-p` и `--keep-owners` терялись, и повышенный процесс брал владение там, где
/// просили этого не делать. Пути идут абсолютными из плана: у повышенного
/// процесса другой рабочий каталог, и относительный путь указал бы на другое.
fn elevation_arguments<'a>(
    cli: &Cli,
    planned: impl IntoIterator<Item = &'a job::Target>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["--elevated".into(), "--yes".into()];
    for (enabled, flag) in [
        (cli.permanent, "--permanent"),
        (cli.reboot, "--reboot"),
        (cli.permanent_on_bin_failure, "--permanent-on-bin-failure"),
        (cli.recycle, "--recycle"),
        (cli.close_locks, "--close-locks"),
        (cli.keep_owners, "--keep-owners"),
        (cli.json, "--json"),
        (cli.find_locks, "--find-locks"),
    ] {
        if enabled {
            args.push(flag.into());
        }
    }
    args.extend(std::iter::repeat_n(
        OsString::from("--verbose"),
        usize::from(cli.verbose),
    ));
    args.extend([OsString::from("--lang"), i18n::language().code().into()]);
    // `--`: путь, начинающийся с «-», не должен читаться как параметр.
    args.push("--".into());
    args.extend(
        planned
            .into_iter()
            .filter(|target| target.refused.is_none())
            .filter_map(|target| {
                target
                    .path
                    .as_ref()
                    .map(|path| path.to_path().into_os_string())
            }),
    );
    args
}

fn print_plan(targets: &[job::Target], summary: &job::Summary, verbose: u8) {
    println!("\n{}", banner());
    for target in &summary.targets {
        let mode = match target.mode {
            Disposition::RecycleBin => tr!("Recycle Bin", "Корзина"),
            Disposition::Permanent => tr!("permanent", "безвозвратно"),
            Disposition::Reboot => tr!("at the next reboot", "при следующей перезагрузке"),
        };
        println!(
            "{}",
            tr_format!(
                "{path} — {files} files, {dirs} folders, {size}, mode: {mode}",
                "{path} — файлов: {files}, папок: {dirs}, {size}, режим: {mode}",
                path = target.path,
                files = target.files,
                dirs = target.directories,
                size = human_bytes(target.bytes),
                mode = mode,
            )
        );
        if !target.reason.is_empty() {
            println!(
                "  {}",
                tr_format!("why: {reason}", "почему: {reason}", reason = target.reason)
            );
        }
        for problem in target
            .problems
            .iter()
            .take(if verbose > 0 { 50 } else { 5 })
        {
            println!("  - {problem}");
        }
    }
    for target in targets {
        if target.summary.truncated && verbose > 0 {
            println!(
                "  {}",
                tr!(
                    "  (scan stopped at the limit — some items were not listed)",
                    "  (обход остановлен на лимите — часть объектов не показана)"
                )
            );
        }
    }
}

fn print_report(report: &Report) {
    for target in &report.targets {
        let mode = match target.disposition {
            Disposition::RecycleBin => tr!("Recycle Bin", "Корзина"),
            Disposition::Permanent => tr!("permanently", "безвозвратно"),
            Disposition::Reboot => tr!("at the next reboot", "при следующей перезагрузке"),
        };
        let scheduled = target.disposition == Disposition::Reboot;
        let removed = match (target.dry_run, scheduled) {
            (true, _) => tr!("would remove", "будет удалено"),
            (false, true) => tr!("will remove at reboot", "будет удалено при перезагрузке"),
            (false, false) => tr!("removed", "удалено"),
        };
        let freed = match (target.dry_run, scheduled) {
            (true, _) => tr!("would free", "будет освобождено"),
            (false, true) => tr!("will free at reboot", "освободится при перезагрузке"),
            (false, false) => tr!("freed", "освобождено"),
        };
        println!(
            "{}",
            tr_format!(
                "{path}: {mode}, {removed} {count}, {freed} {size}",
                "{path}: {mode}, {removed} {count}, {freed} {size}",
                path = target.path,
                mode = mode,
                count = target.removed_total(),
                size = human_bytes(target.freed_bytes),
            )
        );
        for note in &target.notes {
            println!("  + {note:?}");
        }
        for failure in &target.failures {
            println!(
                "  {}",
                tr_format!(
                    "! {path}: {message} ({code})",
                    "! {path}: {message} ({code})",
                    path = failure.path,
                    message = failure.message,
                    code = failure.code,
                )
            );
            for holder in &failure.holders {
                println!(
                    "    {}",
                    tr_format!(
                        "held by {name} (pid {pid})",
                        "держит {name} (pid {pid})",
                        name = holder.name,
                        pid = holder.pid,
                    )
                );
            }
        }
    }
    println!("{}", report.summary_line());
}

/// Подтверждение перед удалением. Интерактивный CLI спрашивает в терминале,
/// вызов из контекстного меню — диалогом и только о безвозвратных объектах:
/// клик по меню остаётся подтверждением для того, что попадает в Корзину.
fn confirm_before(cli: &Cli, targets: &[job::Target]) -> bool {
    if cli.from_shell {
        let irreversible: Vec<&job::Target> = targets
            .iter()
            .filter(|target| target.refused.is_none() && target.mode != Disposition::RecycleBin)
            .collect();
        if irreversible.is_empty() {
            return true;
        }
        return message_box_confirm(&irreversible);
    }
    confirm(targets)
}

/// Диалог подтверждения для безвозвратных целей из контекстного меню:
/// у отсоединённого процесса нет консоли, поэтому вопрос задаётся окном.
fn message_box_confirm(targets: &[&job::Target]) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };

    // Показываем первые несколько путей: остальное — счётчиком.
    let listed = 6;
    let mut list = String::new();
    for target in targets.iter().take(listed) {
        list.push_str(&target.display);
        list.push('\n');
    }
    if targets.len() > listed {
        list.push_str(&tr_format!(
            "… and {n} more\n",
            "… и ещё {n}\n",
            n = targets.len() - listed
        ));
    }
    let text = tr_format!(
        "These objects cannot go to the Recycle Bin and will be deleted permanently:\n\n{list}\nDelete them?",
        "Эти объекты нельзя отправить в Корзину: они будут удалены безвозвратно:\n\n{list}\nУдалить?",
        list = list
    );
    let text = wide(&text);
    let title = wide("RAZE");
    unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(text.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND | MB_TOPMOST,
        ) == IDYES
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Подтверждение показывает ровно то, что будет удалено, и требует явного
/// «да»: у этой программы нет отмены.
fn confirm(targets: &[job::Target]) -> bool {
    let files: usize = targets.iter().map(|t| t.summary.files).sum();
    let dirs: usize = targets.iter().map(|t| t.summary.directories).sum();
    let bytes: u64 = targets.iter().map(|t| t.summary.bytes).sum();
    eprint!(
        "{}",
        tr_format!(
            "\nWill remove {files} files and {dirs} folders, {size}.\nContinue? [y/N] ",
            "\nБудет удалено файлов: {files}, папок: {dirs}, {size}.\nПродолжить? [y/N] ",
            files = files,
            dirs = dirs,
            size = human_bytes(bytes),
        )
    );
    let _ = std::io::stderr().flush();
    if !std::io::stdin().is_terminal() {
        return false;
    }
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(
        answer.trim().to_lowercase().as_str(),
        "y" | "yes" | "д" | "да"
    )
}

fn print_usage() {
    println!("\n{}", banner());
    println!(
        "{}",
        tr_format!(
            "Usage: raze [OPTIONS] <PATH>...",
            "Использование: raze [ПАРАМЕТРЫ] <ПУТЬ>..."
        )
    );
    println!(
        "  {}",
        tr!(
            "Example: raze C:\\Users\\me\\Desktop\\Seller",
            "Пример: raze C:\\Users\\me\\Desktop\\Seller"
        )
    );
    println!(
        "  {}",
        tr!("Example: raze -n C:\\tmp", "Пример: raze -n C:\\tmp")
    );
    println!(
        "\n{}",
        tr!(
            "Run raze --help for the full list.",
            "Полный список — raze --help."
        )
    );
    println!("{TELEGRAM_URL}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_deduplicated_case_insensitively() {
        let paths = dedupe(vec![
            PathBuf::from("C:\\A"),
            PathBuf::from("c:\\a"),
            PathBuf::from("C:\\b"),
        ]);
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn unicode_case_aliases_preserve_only_the_first_requested_path() {
        let first = PathBuf::from(r"C:\Данные\Файл");
        let paths = dedupe(vec![first.clone(), PathBuf::from(r"c:\данные\файл")]);
        assert_eq!(paths, vec![first]);
    }

    #[test]
    fn empty_placeholders_from_explorer_are_dropped() {
        // clap 4 отвергает пустые значения на входе, поэтому
        // проверяем линию обороны, которая собирает пути.
        let kept = drop_empty_placeholders(vec![
            PathBuf::from(""),
            PathBuf::from("C:\\tmp"),
            PathBuf::from(""),
        ]);
        assert_eq!(kept, vec![PathBuf::from("C:\\tmp")]);

        // Вызов из меню без цели — пустой набор.
        let cli = Cli::parse_from(["raze", "--from-shell"]);
        assert!(collect_paths(&cli).is_empty());
    }

    #[test]
    fn the_elevated_copy_gets_every_option_and_absolute_paths() {
        let cli = Cli::parse_from([
            "raze",
            "-p",
            "-c",
            "--keep-owners",
            "-L",
            "-vv",
            "relative-target",
        ]);
        let planned = job::plan(&collect_paths(&cli), &Options::default());
        let args = elevation_arguments(&cli, &planned);

        for expected in [
            "--elevated",
            "--yes",
            "--permanent",
            "--close-locks",
            "--keep-owners",
            "--find-locks",
            "--",
        ] {
            assert!(args.iter().any(|a| a == expected), "{expected}: {args:?}");
        }
        assert!(!args.iter().any(|a| a == "--recycle"));
        assert_eq!(args.iter().filter(|arg| *arg == "--verbose").count(), 2);
        // Пути идут после `--` и абсолютными, а не «как набрали».
        let separator = args.iter().position(|a| a == "--").unwrap();
        let paths = &args[separator + 1..];
        assert_eq!(paths.len(), 1);
        let path = paths[0].to_string_lossy();
        assert!(path.as_bytes()[1] == b':', "{path}");
        assert!(path.to_lowercase().ends_with("relative-target"));
    }

    #[test]
    fn reboot_and_bin_fallback_are_forwarded_to_the_elevated_copy() {
        let empty: Vec<job::Target> = Vec::new();
        let cli = Cli::parse_from(["raze", "--reboot", "target"]);
        let args = elevation_arguments(&cli, &empty);
        assert!(args.iter().any(|a| a == "--reboot"), "{args:?}");

        let cli = Cli::parse_from(["raze", "--permanent-on-bin-failure", "target"]);
        let args = elevation_arguments(&cli, &empty);
        assert!(
            args.iter().any(|a| a == "--permanent-on-bin-failure"),
            "{args:?}"
        );
    }

    #[test]
    fn irreconcilable_modes_cannot_be_combined() {
        assert!(Cli::try_parse_from(["raze", "--reboot", "--permanent", "x"]).is_err());
        assert!(Cli::try_parse_from(["raze", "--reboot", "--recycle", "x"]).is_err());
        assert!(
            Cli::try_parse_from(["raze", "--permanent", "--permanent-on-bin-failure", "x"])
                .is_err()
        );
    }

    #[test]
    fn install_and_uninstall_accept_the_internal_report_file() {
        // Повышенная копия запускается как `--install --lang xx --report-file PATH`:
        // раньше `--install` был exclusive и такой вызов не разбирался.
        let cli = Cli::parse_from([
            "raze",
            "--install",
            "--lang",
            "ru",
            "--report-file",
            "C:\\tmp\\r.txt",
        ]);
        assert!(cli.install);
        assert_eq!(cli.report_file, Some(PathBuf::from("C:\\tmp\\r.txt")));
        let cli = Cli::parse_from(["raze", "--uninstall", "--report-file", "r.txt"]);
        assert!(cli.uninstall);
    }

    #[test]
    fn the_report_file_name_is_unpredictable_and_never_overwrites() {
        assert_ne!(unpredictable_name(), unpredictable_name());
        assert_eq!(unpredictable_name().len(), 32);
        // Занятое имя (файл или ссылка) не перезаписывается: create_new откажет.
        let taken =
            std::env::temp_dir().join(format!("raze-report-test-{}.txt", unpredictable_name()));
        std::fs::write(&taken, b"keep").unwrap();
        assert!(write_install_report(&taken, &["replace".into()]).is_err());
        assert_eq!(std::fs::read(&taken).unwrap(), b"keep");
        let _ = std::fs::remove_file(&taken);
    }

    #[test]
    fn a_failed_step_makes_the_install_summary_fail() {
        let mut report = shell::InstallReport::default();
        let (_, code) = install_summary(&report, false);
        assert_eq!(code, 0);
        report.steps.push(shell::Step::Failed {
            what: "copy x".into(),
            error: "boom".into(),
        });
        let (lines, code) = install_summary(&report, false);
        assert_eq!(code, 1);
        assert!(lines.iter().any(|l| l == "[fail] copy x: boom"));
        assert_eq!(install_summary(&report, true).1, 1);
    }

    #[test]
    fn installation_and_uninstallation_cannot_be_requested_together() {
        assert!(Cli::try_parse_from(["raze", "--install", "--uninstall"]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_paths_are_not_deduplicated_or_elevated_through_lossy_display_text() {
        use std::os::windows::ffi::OsStringExt;
        let mut wide: Vec<u16> = r"C:\raze-cli-test\".encode_utf16().collect();
        wide.push(0xd800);
        let native = PathBuf::from(OsString::from_wide(&wide));
        let replacement = PathBuf::from(r"C:\raze-cli-test\�");
        let paths = dedupe(vec![native.clone(), replacement.clone(), native.clone()]);
        assert_eq!(paths, vec![native.clone(), replacement]);
        let path = NtPath::new(&native).unwrap();
        let planned = vec![job::Target {
            display: path.to_string_lossy(),
            path: Some(path),
            entries: Vec::new(),
            summary: Default::default(),
            verdict: raze_rs::engine::recycle::RecycleVerdict {
                reusable: true,
                reason: String::new(),
            },
            mode: Disposition::RecycleBin,
            refused: None,
        }];
        let cli = Cli::parse_from(["raze", "-y"]);
        let args = elevation_arguments(&cli, &planned);
        assert_eq!(args.last().unwrap(), native.as_os_str());
    }
}
