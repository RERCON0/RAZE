//! RAZE — консольный интерфейс.
//!
//! Работает и как обычная утилита (`raze C:\путь`), и как цель контекстного
//! меню (`raze --from-shell --mode "%1"`). Второй вариант не должен
//! требовать подтверждения: человек уже сказал «удалить» самим кликом.

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use clap::{ArgAction, Parser};

use raze_rs::dialog;
use raze_rs::engine::elevation::{
    build_arguments, is_elevated, needs_elevation, relaunch_elevated, run_elevated_and_wait,
};
use raze_rs::engine::error::NtError;
use raze_rs::engine::path::{comparison_key, eq_ignore_case};
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
        } else if cli.from_shell {
            // Цель отклонена (системная папка, корень диска): из меню об этом
            // иначе не узнать.
            let report = job::execute(&planned, &options);
            if let Some(spec) = failure_spec(&report, false) {
                ask(&spec);
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

    let mut report = job::execute(&planned, &options);
    // У процесса из контекстного меню нет ни консоли, ни окна: что не вышло,
    // человек узнает только из диалога.
    let mut declined = false;
    if cli.from_shell && !cli.dry_run {
        declined = settle_bin_refusals(&mut report, &planned, &options);
    }

    // A ShellExecute handoff has separate stdout and cannot return a report
    // to a script. JSON mode reports this attempt; run it elevated explicitly.
    if report.needs_elevation && !cli.dry_run && !cli.json && !cli.elevated && !is_elevated() {
        print_report(&report);
        if let Some(code) = relaunch_as_admin(&cli, &planned, &report) {
            return code;
        }
    }

    if cli.from_shell && !report.cancelled {
        if let Some(spec) = failure_spec(&report, declined) {
            ask(&spec);
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
    let mut seen: Vec<Vec<u16>> = Vec::new();
    let mut out = Vec::new();
    for path in paths {
        let key = comparison_key(&path);
        if seen.iter().any(|previous| eq_ignore_case(previous, &key)) {
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
                    && r.failures
                        .iter()
                        .any(|failure| NtError(failure.code).needs_elevation())
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
            println!("  + {}", note.describe());
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
        return ask(&confirm_spec(&irreversible));
    }
    confirm(targets)
}

/// Подтверждение для объектов, которые нельзя вернуть из Корзины, из
/// контекстного меню: у отсоединённого процесса нет консоли, вопрос задаётся
/// окном. Слова — те же, что в окне приложения.
fn confirm_spec(targets: &[&job::Target]) -> dialog::Spec {
    let items = limited(targets.iter().map(|target| target.display.clone()));
    // Отложенное и безвозвратное не смешиваются: режим один на весь запуск.
    if targets
        .iter()
        .all(|target| target.mode == Disposition::Reboot)
    {
        return dialog::Spec {
            head: tr!(
                "Objects will be deleted at the next Windows restart. Until then nothing changes.",
                "Объекты будут удалены при следующей перезагрузке Windows. До неё ничего не меняется."
            )
            .to_string(),
            items,
            foot: String::new(),
            yes: tr!("delete", "удалить").to_string(),
            no: Some(tr!("cancel", "отмена").to_string()),
            tone: dialog::Tone::Warning,
        };
    }
    dialog::Spec {
        head: tr!(
            "These objects cannot go to the Recycle Bin and will be deleted permanently:",
            "Эти объекты нельзя отправить в Корзину: они будут удалены безвозвратно:"
        )
        .to_string(),
        items,
        foot: tr!(
            "Permanently deleted objects cannot be restored from the Recycle Bin.",
            "Удалённые навсегда объекты нельзя восстановить из Корзины."
        )
        .to_string(),
        yes: tr!("delete", "удалить").to_string(),
        no: Some(tr!("cancel", "отмена").to_string()),
        tone: dialog::Tone::Error,
    }
}

/// Корзина отказала, а согласия на безвозвратное удаление не было: ничего не
/// тронуто. Спрашиваем так же, как окно приложения, и теми же словами.
fn fallback_spec(targets: &[&job::Target]) -> dialog::Spec {
    dialog::Spec {
        head: tr_format!(
            "{n} target(s) could not be sent to the Recycle Bin. Nothing has been deleted.",
            "{n} целей не удалось отправить в Корзину. Ничего не удалено.",
            n = targets.len()
        ),
        items: limited(targets.iter().map(|target| target.display.clone())),
        foot: tr!(
            "Deleting them permanently cannot be undone.",
            "Безвозвратное удаление отменить нельзя."
        )
        .to_string(),
        yes: tr!("delete permanently", "удалить безвозвратно").to_string(),
        no: Some(tr!("leave as is", "оставить как есть").to_string()),
        tone: dialog::Tone::Error,
    }
}

/// Корзина отказала целям из контекстного меню. Без согласия они остаются на
/// месте — иначе отказ прошёл бы молча; с согласием отказавшие цели удаляются
/// безвозвратно, остальные результаты не меняются. `true` — человек отказался.
fn settle_bin_refusals(report: &mut Report, planned: &[job::Target], options: &Options) -> bool {
    let refused: Vec<&job::Target> = planned
        .iter()
        .filter(|target| bin_refused(report, target))
        .collect();
    if refused.is_empty() {
        return false;
    }
    if !ask(&fallback_spec(&refused)) {
        return true;
    }
    let retry: Vec<job::Target> = refused.into_iter().cloned().collect();
    let again = job::execute(
        &retry,
        &Options {
            permanent: true,
            ..options.clone()
        },
    );
    report.targets.retain(|target| target.bin_refused.is_none());
    report.targets.extend(again.targets);
    report.refused.extend(again.refused);
    report.cancelled |= again.cancelled;
    report.refresh_needs_elevation();
    false
}

/// Отказала ли Корзина именно этой цели (цели сопоставляются по настоящему
/// пути: показанный текст у разных имён может совпасть).
fn bin_refused(report: &Report, target: &job::Target) -> bool {
    target.path.as_ref().is_some_and(|path| {
        let native = path.to_path();
        report
            .targets
            .iter()
            .any(|done| done.bin_refused.is_some() && done.native_path == native)
    })
}

/// Окно с тем, что не удалось. `skip_bin_refused` — человек уже отказался от
/// безвозвратного удаления тех целей, и повторять им это окно незачем.
fn failure_spec(report: &Report, skip_bin_refused: bool) -> Option<dialog::Spec> {
    let mut lines: Vec<String> = report
        .refused
        .iter()
        .map(|refused| format!("{}: {}", refused.path, refused.reason))
        .collect();
    for target in &report.targets {
        if skip_bin_refused && target.bin_refused.is_some() {
            continue;
        }
        lines.extend(
            target
                .failures
                .iter()
                .map(|failure| format!("{}: {}", failure.path, failure.message)),
        );
    }
    if lines.is_empty() {
        return None;
    }
    Some(dialog::Spec {
        head: report.summary_line(),
        items: limited(lines.into_iter()),
        foot: String::new(),
        yes: tr!("ok", "ок").to_string(),
        no: None,
        tone: dialog::Tone::Error,
    })
}

/// Первые несколько строк списка, остальное — счётчиком: окно не прокручивается.
fn limited(items: impl ExactSizeIterator<Item = String>) -> Vec<String> {
    const LISTED: usize = 6;
    let total = items.len();
    let mut list: Vec<String> = items.take(LISTED).collect();
    if total > LISTED {
        list.push(tr_format!(
            "… and {n} more",
            "… и ещё {n}",
            n = total - LISTED
        ));
    }
    list
}

/// Задаёт вопрос (или сообщает, если кнопка одна): окном в стиле приложения,
/// а если оно не открылось — системным. `true` — главная кнопка.
fn ask(spec: &dialog::Spec) -> bool {
    show_styled(spec).unwrap_or_else(|| native_box(spec))
}

/// Окно рисует `raze-app.exe` рядом с этой программой. `None` — окно не
/// открылось (нет файла, нет OpenGL): ответа нет, и молчание отказом не считается.
fn show_styled(spec: &dialog::Spec) -> Option<bool> {
    use std::process::{Command, Stdio};
    let app = std::env::current_exe().ok()?.with_file_name("raze-app.exe");
    let status = Command::new(app)
        .args(spec.to_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    match status.code()? {
        dialog::EXIT_YES => Some(true),
        dialog::EXIT_NO => Some(false),
        _ => None,
    }
}

/// Запасное окно: системное, поверх всех. Подписи кнопок ему недоступны,
/// поэтому вопрос договаривается в тексте.
fn native_box(spec: &dialog::Spec) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND,
        MB_TOPMOST, MB_YESNO,
    };
    let mut text = spec.head.clone();
    if !spec.items.is_empty() {
        text.push_str("\n\n");
        text.push_str(&spec.items.join("\n"));
    }
    if !spec.foot.is_empty() {
        text.push_str("\n\n");
        text.push_str(&spec.foot);
    }
    let icon = match spec.tone {
        dialog::Tone::Warning => MB_ICONWARNING,
        dialog::Tone::Error => MB_ICONERROR,
    };
    let style = if spec.no.is_some() {
        text.push_str(&format!("\n\n{}?", spec.yes));
        MB_YESNO | icon | MB_DEFBUTTON2
    } else {
        MB_OK | icon
    };
    let text = wide(&text);
    let title = wide("RAZE");
    let answer = unsafe {
        MessageBoxW(
            None,
            windows::core::PCWSTR(text.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            style | MB_SETFOREGROUND | MB_TOPMOST,
        )
    };
    spec.no.is_none() || answer == IDYES
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
    use raze_rs::engine::path::NtPath;

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

    fn target_at(path: &str) -> job::Target {
        let path = NtPath::new(std::path::Path::new(path)).unwrap();
        job::Target {
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
        }
    }

    fn outcome_for(
        target: &job::Target,
        bin_refused: bool,
        failures: &[(u32, &str)],
    ) -> raze_rs::report::TargetReport {
        let path = target.path.as_ref().unwrap();
        raze_rs::report::TargetReport {
            path: target.display.clone(),
            native_path: path.to_path(),
            disposition: Disposition::RecycleBin,
            reason: String::new(),
            bin_refused: bin_refused.then(|| "denied".to_string()),
            removed_files: 0,
            removed_directories: 0,
            removed_links: 0,
            freed_bytes: 0,
            notes: Vec::new(),
            failures: failures
                .iter()
                .map(|(code, message)| raze_rs::engine::remove::Failure {
                    path: target.display.clone(),
                    code: *code,
                    message: (*message).to_string(),
                    holders: Vec::new(),
                })
                .collect(),
            dry_run: false,
        }
    }

    #[test]
    fn a_bin_refusal_is_matched_to_its_own_target_only() {
        let refused = target_at(r"C:\raze-cli-test\locked");
        let fine = target_at(r"C:\raze-cli-test\fine");
        let report = Report {
            targets: vec![
                outcome_for(&refused, true, &[(32, "in use")]),
                outcome_for(&fine, false, &[]),
            ],
            ..Report::default()
        };
        assert!(bin_refused(&report, &refused));
        assert!(!bin_refused(&report, &fine));
    }

    #[test]
    fn the_failure_window_says_what_failed_and_stays_quiet_when_nothing_did() {
        assert_eq!(failure_spec(&Report::default(), false), None);

        let locked = target_at(r"C:\raze-cli-test\locked");
        let mut report = Report {
            targets: vec![outcome_for(&locked, true, &[(32, "in use")])],
            refused: vec![raze_rs::report::Refused {
                path: r"C:\Windows".into(),
                reason: "system".into(),
            }],
            ..Report::default()
        };
        let spec = failure_spec(&report, false).unwrap();
        assert_eq!(spec.head, report.summary_line());
        assert_eq!(
            spec.items,
            [
                r"C:\Windows: system".to_string(),
                r"C:\raze-cli-test\locked: in use".to_string()
            ]
        );
        assert!(spec.no.is_none(), "это сообщение, а не вопрос");

        // Человек отказался стирать то, что не принял Корзина: об этом повторно
        // не говорим, но остальное по-прежнему показываем.
        let spec = failure_spec(&report, true).unwrap();
        assert_eq!(spec.items, [r"C:\Windows: system".to_string()]);
        report.refused.clear();
        assert_eq!(failure_spec(&report, true), None);
    }

    #[test]
    fn long_lists_are_cut_and_counted() {
        let list = limited((0..9).map(|n| format!("item{n}")));
        assert_eq!(list.len(), 7, "{list:?}");
        assert_eq!(list[5], "item5");
        assert!(list[6].contains('3'), "{list:?}");
        assert_eq!(
            limited(["a", "b"].map(String::from).into_iter()),
            ["a", "b"]
        );
    }

    #[test]
    fn the_context_menu_questions_say_what_will_happen() {
        let mut reboot = target_at(r"C:\raze-cli-test\later");
        reboot.mode = Disposition::Reboot;
        let mut permanent = target_at(r"C:\raze-cli-test\now");
        permanent.mode = Disposition::Permanent;

        let spec = confirm_spec(&[&reboot]);
        assert_eq!(spec.tone, dialog::Tone::Warning);
        assert_eq!(spec.items, [reboot.display.clone()]);

        let spec = confirm_spec(&[&permanent]);
        assert_eq!(spec.tone, dialog::Tone::Error);
        assert!(!spec.foot.is_empty(), "необратимое предупреждает");
        assert!(spec.no.is_some(), "по умолчанию — отказ, кнопка есть");

        let spec = fallback_spec(&[&permanent, &reboot]);
        assert!(spec.head.contains('2'), "{}", spec.head);
        assert_eq!(spec.items.len(), 2);
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
