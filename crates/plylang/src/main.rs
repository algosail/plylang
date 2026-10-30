use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use std::io::IsTerminal;

use plylang::{
    build_exports, check_package, javascript_file, mode_from_path, parse, test_package,
    write_docs,
};

const USAGE: &[&str] = &[
    "usage: plylang check [dir]",
    "       plylang check --js <file>",
    "       plylang check --parse <file>",
    "       plylang build [dir] [--exports]",
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
    finish_package(write_docs(
        Path::new(&dir),
        out.as_deref().map(Path::new),
    ))
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
    let pattern = pattern.unwrap_or_else(|| "test/**/*.ply".to_string());
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

fn build_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut dir = None;
    let mut exports = false;
    while let Some(arg) = args.next() {
        if is_help(&arg) {
            usage(false);
            return ExitCode::SUCCESS;
        }
        if arg == "--exports" {
            if exports {
                usage(true);
                return ExitCode::from(2);
            }
            exports = true;
            continue;
        }
        if arg.starts_with('-') || dir.is_some() {
            usage(true);
            return ExitCode::from(2);
        }
        dir = Some(arg);
    }
    let dir = dir.unwrap_or_else(|| ".".to_string());
    let dir = Path::new(&dir);
    // `--exports` stays accepted. Both forms align `exports`.
    let _exports = exports;
    finish_package(build_exports(dir))
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
