//! RAZE — консольный интерфейс.
//!
//! Работает и как обычная утилита (`raze C:\путь`), и как цель контекстного
//! меню (`raze --from-shell --mode "%1"`). Второй вариант не должен
//! требовать подтверждения: человек уже сказал «удалить» самим кликом.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use clap::{ArgAction, Parser};

use raze_rs::engine::elevation::{build_arguments, is_elevated, needs_elevation, relaunch_elevated};
use raze_rs::engine::path::NtPath;
use raze_rs::engine::scan::ScanOptions;
use raze_rs::job::{self, Options};
use raze_rs::report::{Report, human_bytes};
use raze_rs::shell;
use raze_rs::{TELEGRAM_URL, VERSION, banner, i18n, tr, tr_format};

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

    /// Force the Recycle Bin even for objects that normally bypass it.
    #[arg(long, conflicts_with = "permanent")]
    recycle: bool,

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
    #[arg(short, long, default_value = "en")]
    lang: String,

    /// Internal: this process was started by the Explorer context menu.
    #[arg(long, hide = true)]
    from_shell: bool,

    /// Internal: target passed by Explorer (%1 for items, %V for background).
    #[arg(short, long, value_name = "PATH", hide = true)]
    mode: Vec<PathBuf>,

    /// Internal: this process already runs elevated.
    #[arg(long, hide = true)]
    elevated: bool,

    /// Install the Explorer context-menu integration.
    #[arg(long, exclusive = true)]
    install: bool,

    /// Remove the Explorer context-menu integration.
    #[arg(long, exclusive = true)]
    uninstall: bool,

    /// Register the modern Windows 11 menu package (needs PowerShell).
    #[arg(long, exclusive = true)]
    register_package: bool,

    /// Print the machine-readable plan and exit.
    #[arg(long)]
    plan_only: bool,

    /// Print extra detail.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

fn main() {
    let cli = Cli::parse();
    match raze_rs::i18n::Language::parse(&cli.lang) {
        Ok(language) => i18n::set_language(language),
        Err(message) => {
            eprintln!("--lang: {message}");
            std::process::exit(2);
        }
    }

    let code = run(cli);
    std::process::exit(code);
}

fn run(cli: Cli) -> i32 {
    if cli.install {
        return shell_install(false);
    }
    if cli.uninstall {
        return shell_install(true);
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
        print_usage();
        return 2;
    }

    let options = Options {
        prefer_recycle: cli.recycle,
        permanent: cli.permanent,
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

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&summary).unwrap_or_default());
    } else {
        print_plan(&planned, &summary, cli.verbose);
    }

    if !summary.refused.is_empty() {
        for refused in &summary.refused {
            eprintln!("{}: {}", refused.path, refused.reason);
        }
    }
    if !job::has_work(&planned) {
        return if summary.refused.is_empty() { 0 } else { 3 };
    }
    if cli.plan_only {
        return 0;
    }

    // Подтверждение: из контекстного меню и с --yes вопросов не задаём.
    // Пробный прогон ничего не меняет, спрашивать нечего.
    if !cli.dry_run && !cli.yes && !cli.from_shell && !confirm(&planned) {
        println!("{}", tr!("Cancelled.", "Отменено."));
        return 1;
    }

    let report = job::execute(&planned, &options);

    // Один раз предлагаем повышение и повторяем всё с начала.
    if report.needs_elevation && !cli.elevated && !is_elevated() {
        if let Some(code) = relaunch_as_admin(&cli, &planned) {
            return code;
        }
    }

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print_report(&report);
    }

    if report.failed() {
        1
    } else {
        0
    }
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
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        // Сравниваем по разобранному пути: `C:\a\` и `C:\a` — одно и то же.
        let key = NtPath::new(&path)
            .map(|p| p.to_string_lossy().to_lowercase())
            .unwrap_or_else(|_| path.to_string_lossy().to_lowercase());
        if seen.insert(key) {
            out.push(path);
        }
    }
    out
}

fn shell_install(uninstall: bool) -> i32 {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let report = shell::install(&exe_dir, uninstall);
    for step in &report.steps {
        match step {
            shell::Step::Done { what } => println!("[ok]   {what}"),
            shell::Step::Skipped { what } => println!("[skip] {what}"),
            shell::Step::Failed { what, error } => {
                eprintln!("[fail] {what}: {error}")
            }
        }
    }
    if uninstall {
        println!(
            "{}",
            tr_format!(
                "Done. The context menu is back to normal.",
                "Готово. Контекстное меню снова обычное."
            )
        );
        return 0;
    }
    if !report.ok() {
        eprintln!(
            "{}",
            tr_format!(
                "Some steps failed; the classic menu verb is still registered if it succeeded.",
                "Часть шагов не удалась; если verb зарегистрирован, он работает."
            )
        );
        return 1;
    }
    println!(
        "{}",
        tr_format!(
            "Installed into {}. If the Windows 11 menu does not show RAZE yet, open a new Explorer window or sign out and back in.",
            "Установлено в {}. Если RAZE ещё нет в меню Windows 11, откройте новое окно Проводника или перезайдите в сеанс.",
            report.install_dir.display()
        )
    );
    0
}

fn relaunch_as_admin(cli: &Cli, planned: &[job::Target]) -> Option<i32> {
    if !needs_elevation() {
        return None;
    }
    let Ok(exe) = std::env::current_exe() else {
        return None;
    };
    let line = build_arguments(&elevation_arguments(cli, planned));
    if cli.verbose > 0 {
        eprintln!("{}", tr_format!("Requesting admin rights…", "Запрашиваю права администратора…"));
    }
    match relaunch_elevated(&exe, &line) {
        Ok(true) => Some(0),
        Ok(false) => {
            eprintln!(
                "{}",
                tr!("Administrator rights were not granted.", "Права администратора не выданы.")
            );
            Some(1)
        }
        Err(error) => {
            eprintln!("{}: {error}", tr!("Cannot restart", "Не удалось перезапустить"));
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
fn elevation_arguments(cli: &Cli, planned: &[job::Target]) -> Vec<String> {
    let mut args = vec!["--elevated".to_string(), "--yes".to_string()];
    for (enabled, flag) in [
        (cli.permanent, "--permanent"),
        (cli.recycle, "--recycle"),
        (cli.close_locks, "--close-locks"),
        (cli.keep_owners, "--keep-owners"),
        (cli.json, "--json"),
    ] {
        if enabled {
            args.push(flag.to_string());
        }
    }
    args.extend(["--lang".to_string(), i18n::language().code().to_string()]);
    // `--`: путь, начинающийся с «-», не должен читаться как параметр.
    args.push("--".to_string());
    args.extend(
        planned
            .iter()
            .filter(|target| target.refused.is_none())
            .map(|target| target.display.clone()),
    );
    args
}

fn print_plan(targets: &[job::Target], summary: &job::Summary, verbose: u8) {
    println!("\n{}", banner());
    for target in &summary.targets {
        let mode = match target.mode {
            raze_rs::engine::recycle::Disposition::RecycleBin => tr!("Recycle Bin", "Корзина"),
            raze_rs::engine::recycle::Disposition::Permanent => tr!("permanent", "безвозвратно"),
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
            println!("  {}", tr_format!("why: {reason}", "почему: {reason}", reason = target.reason));
        }
        for problem in target.problems.iter().take(if verbose > 0 { 50 } else { 5 }) {
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
            raze_rs::engine::recycle::Disposition::RecycleBin => tr!("Recycle Bin", "Корзина"),
            raze_rs::engine::recycle::Disposition::Permanent => tr!("permanently", "безвозвратно"),
        };
        println!(
            "{}",
            tr_format!(
                "{path}: {mode}, removed {count}, freed {size}",
                "{path}: {mode}, удалено {count}, освобождено {size}",
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
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes" | "д" | "да")
}

fn print_usage() {
    println!("\n{}", banner());
    println!("{}", tr_format!("Usage: raze [OPTIONS] <PATH>...", "Использование: raze [ПАРАМЕТРЫ] <ПУТЬ>..."));
    println!("  {}", tr!("Example: raze C:\\Users\\me\\Desktop\\Seller", "Пример: raze C:\\Users\\me\\Desktop\\Seller"));
    println!("  {}", tr!("Example: raze -n C:\\tmp", "Пример: raze -n C:\\tmp"));
    println!("\n{}", tr!("Run raze --help for the full list.", "Полный список — raze --help."));
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
        let cli = Cli::parse_from(["raze", "-p", "-c", "--keep-owners", "relative-target"]);
        let planned = job::plan(&collect_paths(&cli), &Options::default());
        let args = elevation_arguments(&cli, &planned);

        for expected in ["--elevated", "--yes", "--permanent", "--close-locks", "--keep-owners", "--"] {
            assert!(args.iter().any(|a| a == expected), "{expected}: {args:?}");
        }
        assert!(!args.iter().any(|a| a == "--recycle"));
        // Пути идут после `--` и абсолютными, а не «как набрали».
        let separator = args.iter().position(|a| a == "--").unwrap();
        let paths = &args[separator + 1..];
        assert_eq!(paths.len(), 1);
        assert!(paths[0].as_bytes()[1] == b':', "{}", paths[0]);
        assert!(paths[0].to_lowercase().ends_with("relative-target"));
    }

    #[test]
    fn a_dry_run_never_asks_for_confirmation() {
        let cli = Cli::parse_from(["raze", "-n", "C:\tmp"]);
        assert!(cli.dry_run && !cli.yes && !cli.from_shell);
    }

    #[test]
    fn cli_parses_the_documented_flags() {
        let cli = Cli::parse_from(["raze", "-n", "-y", "C:\\tmp"]);
        assert!(cli.dry_run);
        assert!(cli.yes);
        assert_eq!(cli.paths, vec![PathBuf::from("C:\\tmp")]);
    }
}