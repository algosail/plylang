use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use std::io::IsTerminal;

use plylang::{
    build_watch, build_with, check_package, compile_with, javascript_file, mode_from_path,
    module_name_from_path, parse, test_package, write_docs, BuildFlags, CompileFlags,
};

const USAGE: &[&str] = &[
    "usage: plylang check [dir]",
    "       plylang check --js <file>",
    "       plylang check --parse <file>",
    "       plylang build [dir] [--exports [--all] [--check] [<module>...]] [--watch] [--entry <module>] [--out <dir>] [--foreign <dir>]",
    "       plylang compile [dir] [--entry <module>] [--out <file>] [--foreign <dir>]",
    "       plylang test [pattern]",
    "       plylang docs [dir] [--out <file>]",
];

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        usage(true);
        return ExitCode::from(2);
    };
    if is_help(&command) {
        usage(false);
        return ExitCode::SUCCESS;
    }
    if command == "--version" || command == "-V" {
        println!("plylang {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    match command.as_str() {
        "build" => build_command(args),
        "compile" => compile_command(args),
        "check" => check_command(args),
        "docs" => docs_command(args),
        "test" => test_command(args),
        _ => {
            usage(true);
            ExitCode::from(2)
        }
    }
}

fn docs_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut dir = None;
    let mut out = None;
    while let Some(arg) = args.next() {
        if is_help(&arg) {
            usage(false);
            return ExitCode::SUCCESS;
        }
        if arg == "--out" {
            let Some(file) = args.next() else {
                usage(true);
                return ExitCode::from(2);
            };
            if out.is_some() || file.starts_with('-') {
                usage(true);
                return ExitCode::from(2);
            }
            out = Some(file);
            continue;
        }
        if arg.starts_with('-') || dir.is_some() {
            usage(true);
            return ExitCode::from(2);
        }
        dir = Some(arg);
    }
    let dir = dir.unwrap_or_else(|| ".".to_string());
    finish_package(write_docs(Path::new(&dir), out.as_deref().map(Path::new)))
}

fn test_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut pattern = None;
    while let Some(arg) = args.next() {
        if is_help(&arg) {
            usage(false);
            return ExitCode::SUCCESS;
        }
        if arg.starts_with('-') || pattern.is_some() {
            usage(true);
            return ExitCode::from(2);
        }
        pattern = Some(arg);
    }
    let pattern = pattern.unwrap_or_else(|| "**/*.ply".to_string());
    match test_package(Path::new("."), &pattern) {
        Ok(report) => {
            let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
            print!("{}", report.render(color));
            if report.failed() {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            eprintln!("{}", error.render());
            ExitCode::from(1)
        }
    }
}

fn compile_command(args: impl Iterator<Item = String>) -> ExitCode {
    let run = match read_run_args(args, false) {
        Ok(run) => run,
        Err(code) => return code,
    };
    finish_package(compile_with(
        Path::new(&run.dir),
        &CompileFlags {
            entry: run.entry,
            out: run.out,
            foreign: run.foreign,
        },
    ))
}

fn build_command(args: impl Iterator<Item = String>) -> ExitCode {
    let run = match read_run_args(args, true) {
        Ok(run) => run,
        Err(code) => return code,
    };
    if let Err(code) = reject_run(&run) {
        return code;
    }
    let flags = BuildFlags {
        entry: run.entry,
        out: run.out,
        foreign: run.foreign,
        exports: run.exports,
        export_all: run.export_all,
        export_modules: run.export_modules,
        export_check: run.export_check,
    };
    if run.watch {
        return finish_package(build_watch(Path::new(&run.dir), &flags));
    }
    finish_build(build_with(Path::new(&run.dir), &flags))
}

fn reject_run(run: &RunArgs) -> Result<(), ExitCode> {
    if (run.export_all || run.export_check || !run.export_modules.is_empty()) && !run.exports {
        usage(true);
        return Err(ExitCode::from(2));
    }
    if run.export_all && !run.export_modules.is_empty() {
        usage(true);
        return Err(ExitCode::from(2));
    }
    if run.watch && run.exports {
        eprintln!("--watch cannot be combined with --exports");
        return Err(ExitCode::from(2));
    }
    Ok(())
}

struct RunArgs {
    dir: String,
    entry: Option<String>,
    out: Option<String>,
    foreign: Option<String>,
    exports: bool,
    export_all: bool,
    export_check: bool,
    export_modules: Vec<String>,
    watch: bool,
}

fn read_run_args(
    mut args: impl Iterator<Item = String>,
    allow_exports: bool,
) -> Result<RunArgs, ExitCode> {
    let mut dir = None;
    let mut entry = None;
    let mut out = None;
    let mut foreign = None;
    let mut exports = false;
    let mut export_all = false;
    let mut export_check = false;
    let mut export_modules = Vec::new();
    let mut watch = false;
    while let Some(arg) = args.next() {
        if is_help(&arg) {
            usage(false);
            return Err(ExitCode::SUCCESS);
        }
        if arg == "--watch" {
            if !allow_exports || watch {
                usage(true);
                return Err(ExitCode::from(2));
            }
            watch = true;
            continue;
        }
        if arg == "--exports" {
            if !allow_exports || exports {
                usage(true);
                return Err(ExitCode::from(2));
            }
            exports = true;
            continue;
        }
        if arg == "--all" {
            if !allow_exports || export_all {
                usage(true);
                return Err(ExitCode::from(2));
            }
            export_all = true;
            continue;
        }
        if arg == "--check" {
            if !allow_exports || export_check {
                usage(true);
                return Err(ExitCode::from(2));
            }
            export_check = true;
            continue;
        }
        let slot = match arg.as_str() {
            "--entry" => &mut entry,
            "--out" => &mut out,
            "--foreign" => &mut foreign,
            _ => {
                if arg.starts_with('-') {
                    usage(true);
                    return Err(ExitCode::from(2));
                }
                if exports {
                    if let Some(name) = canonical_export_name(&arg) {
                        export_modules.push(name);
                        continue;
                    }
                }
                if dir.is_some() {
                    usage(true);
                    return Err(ExitCode::from(2));
                }
                dir = Some(arg);
                continue;
            }
        };
        let Some(value) = args.next() else {
            usage(true);
            return Err(ExitCode::from(2));
        };
        if slot.is_some() || value.starts_with('-') {
            usage(true);
            return Err(ExitCode::from(2));
        }
        *slot = Some(value);
    }
    Ok(RunArgs {
        dir: dir.unwrap_or_else(|| ".".to_string()),
        entry,
        out,
        foreign,
        exports,
        export_all,
        export_check,
        export_modules,
        watch,
    })
}

/// `Data/Sheet` and `./Data/Sheet` both name `Data/Sheet`.
fn canonical_export_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("./").unwrap_or(name);
    if rest.is_empty() {
        return None;
    }
    let segments = module_name_from_path(&format!("{rest}.ply")).ok()?;
    Some(segments.join("/"))
}

fn check_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(arg) = args.next() else {
        return finish_package(check_package(Path::new(".")));
    };
    if is_help(&arg) {
        usage(false);
        return ExitCode::SUCCESS;
    }
    match arg.as_str() {
        "--parse" => file_command(args, print_parse),
        "--js" => file_command(args, print_javascript),
        flag if flag.starts_with('-') => {
            usage(true);
            ExitCode::from(2)
        }
        dir => {
            if args.next().is_some() {
                usage(true);
                return ExitCode::from(2);
            }
            finish_package(check_package(Path::new(dir)))
        }
    }
}

fn file_command(
    mut args: impl Iterator<Item = String>,
    run: fn(&Path) -> Result<(), ExitCode>,
) -> ExitCode {
    let Some(path) = args.next() else {
        usage(true);
        return ExitCode::from(2);
    };
    if args.next().is_some() || path.starts_with('-') {
        usage(true);
        return ExitCode::from(2);
    }
    match run(Path::new(&path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

fn print_parse(path: &Path) -> Result<(), ExitCode> {
    let source = read_source(path)?;
    let module = parse(&source, mode_from_path(&path.to_string_lossy())).map_err(|error| {
        eprintln!("{}", error.render(&path.display().to_string(), &source));
        ExitCode::from(1)
    })?;
    println!("{module}");
    Ok(())
}

fn print_javascript(path: &Path) -> Result<(), ExitCode> {
    match javascript_file(path) {
        Ok(code) => {
            print!("{code}");
            Ok(())
        }
        Err(error) => {
            eprintln!("{}", error.render());
            Err(ExitCode::from(1))
        }
    }
}

fn read_source(path: &Path) -> Result<String, ExitCode> {
    fs::read_to_string(path).map_err(|error| {
        eprintln!("{}: {error}", path.display());
        ExitCode::from(1)
    })
}

fn finish_package(result: Result<(), plylang::BuildError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", error.render());
            ExitCode::from(1)
        }
    }
}

fn finish_build(result: Result<bool, plylang::BuildError>) -> ExitCode {
    match result {
        Ok(false) => ExitCode::SUCCESS,
        Ok(true) => ExitCode::from(1),
        Err(error) => {
            eprintln!("{}", error.render());
            ExitCode::from(1)
        }
    }
}

fn is_help(arg: &str) -> bool {
    arg == "--help" || arg == "-h" || arg == "help"
}

fn usage(to_stderr: bool) {
    for line in USAGE {
        if to_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical_export_name, read_run_args, reject_run, ExitCode};

    fn parse(args: &[&str]) -> Result<super::RunArgs, ExitCode> {
        read_run_args(args.iter().map(|arg| (*arg).to_string()), true)
    }

    #[test]
    fn a_module_after_exports_is_a_name() {
        let run = parse(&["--exports", "./Data/Sheet", "Css"]).unwrap();
        assert!(run.exports);
        assert_eq!(run.dir, ".");
        assert_eq!(run.export_modules, ["Data/Sheet", "Css"]);
        assert!(reject_run(&run).is_ok());
        assert_eq!(
            canonical_export_name("./Data/Sheet").as_deref(),
            Some("Data/Sheet")
        );
    }

    #[test]
    fn a_module_before_exports_is_the_directory() {
        let run = parse(&["Data/Sheet", "--exports"]).unwrap();
        assert_eq!(run.dir, "Data/Sheet");
        assert!(run.export_modules.is_empty());
        assert!(run.exports);
        let package = parse(&["--exports", "pkg"]).unwrap();
        assert_eq!(package.dir, "pkg");
        assert!(package.export_modules.is_empty());
        assert!(parse(&["--exports", "pkg", "other"]).is_err());
    }

    #[test]
    fn a_build_without_exports_keeps_one_directory() {
        let run = parse(&["Data/Sheet"]).unwrap();
        assert_eq!(run.dir, "Data/Sheet");
        assert!(!run.exports);
        assert!(reject_run(&run).is_ok());
    }

    #[test]
    fn all_and_check_need_exports_and_all_takes_no_names() {
        let bare = parse(&["--all"]).unwrap();
        assert_eq!(reject_run(&bare), Err(ExitCode::from(2)));
        let check = parse(&["--check"]).unwrap();
        assert_eq!(reject_run(&check), Err(ExitCode::from(2)));
        let both = parse(&["--exports", "--all", "--check"]).unwrap();
        assert!(both.export_all && both.export_check && both.exports);
        assert!(reject_run(&both).is_ok());
        let named = parse(&["--exports", "--all", "Data/Sheet"]).unwrap();
        assert_eq!(reject_run(&named), Err(ExitCode::from(2)));
        let watched = parse(&["--watch", "--exports"]).unwrap();
        assert_eq!(reject_run(&watched), Err(ExitCode::from(2)));
    }

    #[test]
    fn compile_rejects_export_flags() {
        let args = ["--exports", "--all", "--check"];
        for arg in args {
            assert!(matches!(
                read_run_args(std::iter::once(arg.to_string()), false),
                Err(code) if code == ExitCode::from(2)
            ));
        }
    }
}
