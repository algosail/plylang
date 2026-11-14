//! `plylang test` finds `?` checks in source files, compiles them, and runs them.
//!
//! A check is `? "name" < picture > [ program ]`. Both programs start from an
//! empty stack. The runner reads the name before the program runs. Comparison
//! is structural. One failed check does not cancel the others in the file.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::ast::Item;
use crate::build::{build, compile_checks, open_package, package_dirs, BuildError, PlannedCheck};
use crate::parser::parse;
use crate::paths::ParseMode;

const HARNESS: &str = r##"
import { pathToFileURL } from "node:url";
import { writeFileSync } from "node:fs";

const file = process.argv[2];
const out = process.argv[3];
const checks = JSON.parse(process.argv[4]);
const mod = await import(pathToFileURL(file).href);

function detail(error) {
  const text = error && error.stack ? error.stack : String(error);
  return text.endsWith("\n") ? text.slice(0, -1) : text;
}

function stack(value, arity) {
  if (arity === 0) return [];
  if (arity === 1) return [value];
  return Array.isArray(value) ? value : [value];
}

function same(left, right) {
  if (Object.is(left, right)) return true;
  if (typeof left !== typeof right) return false;
  if (left && right && typeof left === "object") {
    if (Array.isArray(left) || Array.isArray(right)) {
      if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) return false;
      return left.every((item, index) => same(item, right[index]));
    }
    const keys = Object.keys(left);
    if (keys.length !== Object.keys(right).length) return false;
    return keys.every((key) => Object.prototype.hasOwnProperty.call(right, key) && same(left[key], right[key]));
  }
  return false;
}

function show(value) {
  if (typeof value === "string") return JSON.stringify(value);
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (typeof value === "function") return "[quotation]";
  if (value === null || value === undefined) return String(value);
  if (Array.isArray(value)) {
    if (value.length === 0) return "<>";
    return "< " + value.map(show).join(" ") + " >";
  }
  if (typeof value.tag === "string") {
    const fields = Object.keys(value).filter((key) => key !== "tag");
    if (fields.length === 0) return value.tag;
    if (fields.length === 1 && fields[0] === "value") return value.tag + " " + show(value.value);
    return value.tag + " " + fields.map((key) => key + ": " + show(value[key])).join(" ");
  }
  return JSON.stringify(value);
}

function showStack(values) {
  if (values.length === 0) return "<>";
  return "< " + values.map(show).join(" ") + " >";
}

const report = checks.map((check) => {
  const wantFn = mod[check.want];
  const bodyFn = mod[check.body];
  if (typeof wantFn !== "function" || typeof bodyFn !== "function") {
    return { ok: false, name: check.name, text: "missing check" };
  }
  let want;
  try {
    want = stack(wantFn(), check.arity);
  } catch (error) {
    return { ok: false, name: check.name, text: detail(error) };
  }
  let got;
  try {
    got = stack(bodyFn(), check.arity);
  } catch (error) {
    return { ok: false, name: check.name, text: detail(error) };
  }
  if (same(got, want)) return { ok: true, name: check.name, text: "" };
  return {
    ok: false,
    name: check.name,
    text: "want " + showStack(want) + "\ngot " + showStack(got),
  };
});
writeFileSync(out, JSON.stringify(report));
"##;

#[derive(Debug)]
pub struct TestReport {
    pub files: Vec<FileReport>,
    pub duration: Duration,
}

#[derive(Debug)]
pub struct FileReport {
    pub path: String,
    pub checks: Vec<Check>,
    pub failure: Option<String>,
    pub duration: Duration,
}

#[derive(Debug)]
pub struct Check {
    pub name: String,
    /// 1-based line of the name. `0` when the line is unknown.
    pub line: u32,
    pub ok: bool,
    pub text: String,
}

impl TestReport {
    pub fn failed(&self) -> bool {
        self.files.iter().any(|file| file.failed())
    }

    pub fn render(&self, color: bool) -> String {
        let paint = Paint { color };
        let mut out = String::new();
        for (index, file) in self.files.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(&paint.bold(&file.path));
            out.push(' ');
            out.push_str(&paint.dim(&format_duration(file.duration)));
            out.push('\n');
            if let Some(failure) = &file.failure {
                out.push_str("  ");
                out.push_str(&paint.bad("✗"));
                out.push(' ');
                out.push_str(failure_title(failure));
                out.push('\n');
                for line in failure.lines() {
                    out.push_str("    ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
            if file.checks.is_empty() && file.failure.is_none() {
                out.push_str("  ");
                out.push_str(&paint.dim("(no checks)"));
                out.push('\n');
            }
            for check in &file.checks {
                out.push_str("  ");
                if check.ok {
                    out.push_str(&paint.good("✓"));
                } else {
                    out.push_str(&paint.bad("✗"));
                }
                out.push(' ');
                out.push_str(&check.name);
                if check.line > 0 {
                    out.push(' ');
                    out.push_str(&paint.dim(&format!("{}:{}", file.path, check.line)));
                }
                out.push('\n');
                if !check.ok && !check.text.is_empty() {
                    for line in check.text.lines() {
                        out.push_str("    ");
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let checks: Vec<&Check> = self.files.iter().flat_map(|file| &file.checks).collect();
        let failed_checks = checks.iter().filter(|check| !check.ok).count();
        let passed_checks = checks.len() - failed_checks;
        let failed_files = self.files.iter().filter(|file| file.failed()).count();
        let passed_files = self.files.len() - failed_files;
        out.push_str("Tests  ");
        out.push_str(&count_line(&paint, failed_checks, passed_checks));
        out.push('\n');
        out.push_str("Files  ");
        out.push_str(&count_line(&paint, failed_files, passed_files));
        out.push('\n');
        out.push_str("Time   ");
        out.push_str(&format_duration(self.duration));
        out.push('\n');
        out
    }
}

impl FileReport {
    fn failed(&self) -> bool {
        self.failure.is_some() || self.checks.iter().any(|check| !check.ok)
    }
}

fn failure_title(failure: &str) -> &str {
    let first = failure.lines().next().unwrap_or(failure);
    if first.contains("error: PL") {
        "failed to compile"
    } else {
        "failed to run"
    }
}

fn count_line(paint: &Paint, failed: usize, passed: usize) -> String {
    let total = failed + passed;
    if failed == 0 {
        return format!("{} ({total})", paint.good(&format!("{passed} passed")));
    }
    if passed == 0 {
        return format!("{} ({total})", paint.bad(&format!("{failed} failed")));
    }
    format!(
        "{} | {} ({total})",
        paint.bad(&format!("{failed} failed")),
        paint.good(&format!("{passed} passed"))
    )
}

fn format_duration(duration: Duration) -> String {
    let ms = duration.as_secs_f64() * 1000.0;
    if ms < 1000.0 {
        format!("{}ms", ms.round() as u64)
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}

struct Paint {
    color: bool,
}

impl Paint {
    fn good(&self, text: &str) -> String {
        self.wrap("32", text)
    }

    fn bad(&self, text: &str) -> String {
        self.wrap("31", text)
    }

    fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }

    fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }

    fn wrap(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\u{1b}[{code}m{text}\u{1b}[0m")
        } else {
            text.to_string()
        }
    }
}

/// Find `pattern` under `ply.source`, build the package, and run each file that
/// has a `?` check. The pattern is relative to the source directory.
pub fn test_package(start: &Path, pattern: &str) -> Result<TestReport, BuildError> {
    let started = Instant::now();
    let (root, _out) = package_dirs(start)?;
    let package = open_package(&root)?;
    let files = match_source(&package.source, pattern)?;
    let mut entries = Vec::new();
    for file in files {
        if file_has_probe(&file)? {
            entries.push(file);
        }
    }
    if entries.is_empty() {
        return Err(BuildError::plain(format!("no tests matched {pattern}")));
    }
    build(&root)?;
    let mut reports = Vec::new();
    for file in entries {
        reports.push(run_file(&root, &package.source, &file));
    }
    Ok(TestReport {
        files: reports,
        duration: started.elapsed(),
    })
}

fn match_source(source: &Path, pattern: &str) -> Result<Vec<PathBuf>, BuildError> {
    let mut found = Vec::new();
    if source.is_dir() {
        walk_source(source, source, pattern, &mut found)?;
    }
    found.sort();
    Ok(found)
}

fn walk_source(
    source: &Path,
    dir: &Path,
    pattern: &str,
    found: &mut Vec<PathBuf>,
) -> Result<(), BuildError> {
    let entries = fs::read_dir(dir).map_err(|error| crate::build::os_error(dir, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| crate::build::os_error(dir, error))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            walk_source(source, &path, pattern, found)?;
            continue;
        }
        if !name.ends_with(".ply") || name.ends_with(".d.ply") {
            continue;
        }
        let relative = path
            .strip_prefix(source)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if glob_match(pattern, &relative) {
            found.push(path);
        }
    }
    Ok(())
}

/// A file that fails to parse still counts, so the runner can show the diagnostic.
fn file_has_probe(path: &Path) -> Result<bool, BuildError> {
    let source = fs::read_to_string(path).map_err(|error| crate::build::os_error(path, error))?;
    match parse(&source, ParseMode::Source) {
        Err(_) => Ok(true),
        Ok(module) => Ok(module
            .items
            .iter()
            .any(|item| matches!(item, Item::Probe(_)))),
    }
}

fn run_file(root: &Path, source: &Path, file: &Path) -> FileReport {
    let started = Instant::now();
    let path = file
        .strip_prefix(source)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_nanos())
        .unwrap_or(0);
    let temp_root = root.join(format!(".ply-test-{}-{nanos}", std::process::id()));
    let _cleanup = DirCleanup {
        path: temp_root.clone(),
    };
    let relative = file
        .strip_prefix(source)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    let from_js = temp_root.join(&relative).with_extension("js");
    let compiled = match compile_checks(root, file, &from_js) {
        Ok(compiled) => compiled,
        Err(error) => {
            return FileReport {
                path,
                checks: Vec::new(),
                failure: Some(error.render()),
                duration: started.elapsed(),
            };
        }
    };
    let (checks, failure) = match execute(root, &from_js, &compiled.0, &compiled.1) {
        Ok(checks) => (checks, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    FileReport {
        path,
        checks,
        failure,
        duration: started.elapsed(),
    }
}

fn execute(
    root: &Path,
    from_js: &Path,
    js: &str,
    plans: &[PlannedCheck],
) -> Result<Vec<Check>, String> {
    if let Some(parent) = from_js.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(from_js, js).map_err(|error| error.to_string())?;
    let harness = std::env::temp_dir().join(format!(
        "plylang-test-harness-{}-{}.mjs",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or(0)
    ));
    fs::write(&harness, HARNESS).map_err(|error| error.to_string())?;
    let result = std::env::temp_dir().join(format!(
        "plylang-test-result-{}-{}.json",
        std::process::id(),
        harness
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("run")
    ));
    let payload = plans
        .iter()
        .map(|plan| {
            serde_json::json!({
                "name": plan.name,
                "want": plan.want,
                "body": plan.body,
                "arity": plan.arity,
            })
        })
        .collect::<Vec<_>>();
    let words = serde_json::to_string(&payload).map_err(|error| error.to_string())?;
    let output = Command::new("node")
        .arg(&harness)
        .arg(from_js)
        .arg(&result)
        .arg(words)
        .current_dir(root)
        .output();
    let _ = fs::remove_file(&harness);
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            let _ = fs::remove_file(&result);
            return Err(format!("node was not found: {error}"));
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        let bytes = fs::read(&result).map_err(|error| {
            let _ = fs::remove_file(&result);
            error.to_string()
        })?;
        let _ = fs::remove_file(&result);
        return read_checks(&bytes).map(|checks| locate(plans, checks));
    }
    let _ = fs::remove_file(&result);
    let mut detail = String::new();
    if !stderr.is_empty() {
        detail.push_str(&stderr);
    }
    if !stdout.is_empty() {
        if !detail.is_empty() {
            detail.push('\n');
        }
        detail.push_str(&stdout);
    }
    if detail.is_empty() {
        detail = format!("node exited with {}", output.status);
    }
    Err(detail)
}

struct DirCleanup {
    path: PathBuf,
}

impl Drop for DirCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches("./");
    let path = path.trim_start_matches("./");
    let pattern: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    glob_parts(&pattern, &path)
}

fn glob_parts(pattern: &[&str], path: &[&str]) -> bool {
    let Some(head) = pattern.first() else {
        return path.is_empty();
    };
    if *head == "**" {
        if glob_parts(&pattern[1..], path) {
            return true;
        }
        return !path.is_empty() && glob_parts(pattern, &path[1..]);
    }
    !path.is_empty() && segment_match(head, path[0]) && glob_parts(&pattern[1..], &path[1..])
}

fn segment_match(pattern: &str, text: &str) -> bool {
    segment_parts(pattern.as_bytes(), text.as_bytes())
}

fn read_checks(bytes: &[u8]) -> Result<Vec<Check>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let rows = value
        .as_array()
        .ok_or_else(|| "check report was not an array".to_string())?;
    Ok(rows
        .iter()
        .map(|row| Check {
            ok: row
                .get("ok")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            name: row
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
            line: 0,
            text: row
                .get("text")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect())
}

fn locate(plans: &[PlannedCheck], mut checks: Vec<Check>) -> Vec<Check> {
    for (check, plan) in checks.iter_mut().zip(plans) {
        check.line = plan.line;
        if check.name.is_empty() {
            check.name = plan.name.clone();
        }
    }
    checks
}

fn segment_parts(pattern: &[u8], text: &[u8]) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    match pattern[0] {
        b'*' => {
            segment_parts(&pattern[1..], text)
                || (!text.is_empty() && segment_parts(pattern, &text[1..]))
        }
        b'?' => !text.is_empty() && segment_parts(&pattern[1..], &text[1..]),
        byte => !text.is_empty() && text[0] == byte && segment_parts(&pattern[1..], &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{glob_match, Check, FileReport, TestReport};

    #[test]
    fn a_glob_crosses_directories_only_at_a_double_star() {
        assert!(glob_match("test/**/*.ply", "test/Stream.ply"));
        assert!(glob_match("test/**/*.ply", "test/Data/Clock.ply"));
        assert!(!glob_match("test/*.ply", "test/Data/Clock.ply"));
        assert!(glob_match("test/Stream.ply", "test/Stream.ply"));
        assert!(!glob_match("test/Stream.ply", "test/Other.ply"));
        assert!(glob_match("*.ply", "Check.ply"));
        assert!(!glob_match("*.ply", "test/Check.ply"));
    }

    #[test]
    fn the_report_lists_every_check_and_the_failure_text() {
        let report = TestReport {
            files: vec![FileReport {
                path: "test/Pass.ply".to_string(),
                checks: vec![
                    Check {
                        name: "yes".to_string(),
                        line: 4,
                        ok: true,
                        text: String::new(),
                    },
                    Check {
                        name: "no".to_string(),
                        line: 8,
                        ok: false,
                        text: "no: 1 !== 2".to_string(),
                    },
                ],
                failure: None,
                duration: Duration::from_millis(12),
            }],
            duration: Duration::from_millis(12),
        };
        assert_eq!(
            report.render(false),
            "\
test/Pass.ply 12ms
  ✓ yes test/Pass.ply:4
  ✗ no test/Pass.ply:8
    no: 1 !== 2

Tests  1 failed | 1 passed (2)
Files  1 failed (1)
Time   12ms
"
        );
    }

    #[test]
    fn a_compile_error_is_a_failed_file() {
        let report = TestReport {
            files: vec![FileReport {
                path: "test/Bad.ply".to_string(),
                checks: Vec::new(),
                failure: Some(
                    "test/Bad.ply:1:1: error: PL0061: type mismatch: expected Int, found Str"
                        .to_string(),
                ),
                duration: Duration::from_millis(3),
            }],
            duration: Duration::from_millis(3),
        };
        let text = report.render(false);
        assert!(text.contains("✗ failed to compile"), "{text}");
        assert!(text.contains("PL0061"), "{text}");
        assert!(text.contains("Tests  0 passed (0)"), "{text}");
        assert!(text.contains("Files  1 failed (1)"), "{text}");
        assert!(report.failed());
    }

    #[test]
    fn a_package_runs_checks_in_source_files_and_deletes_the_temp_directory() {
        let temp = Temp::new("ply-test-run");
        temp.write(
            "package.json",
            "{\n  \"name\": \"fixture\",\n  \"type\": \"module\",\n  \"ply\": { \"source\": \"src\", \"foreign\": \"foreign\", \"out\": \"dist\", \"language\": 1 }\n}\n",
        );
        temp.write(
            "src/Lib.ply",
            "\
@ answer

: answer ( -- Num )
  1

: hidden ( -- Num )
  1

? \"yes\" < 1 > [
  answer
]

? \"off\" < 1 > [
  2
]
",
        );
        temp.write(
            "src/Also.ply",
            "\
@

: foreign boom ( -- Num )

? \"named\" < 1 > [
  boom
]

? \"empty\" <> [
]
",
        );
        temp.write(
            "foreign/Also.js",
            "export function boom() { throw new Error(\"nope\"); }\n",
        );
        temp.write(
            "src/Bad.ply",
            "\
@

? \"bad\" < 1 > [
  \"no\"
]
",
        );
        temp.write(
            "src/Miss.ply",
            "\
@

: other ( -- Num )
  1
",
        );

        let missing = super::test_package(&temp.0, "missing/**/*.ply").unwrap_err();
        assert_eq!(missing.render(), "no tests matched missing/**/*.ply");
        assert!(!temp.0.join("dist").exists());

        let report = super::test_package(&temp.0, "**/*.ply")
            .unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(report.failed());
        assert_eq!(report.files.len(), 3, "{report:?}");
        let also = report
            .files
            .iter()
            .find(|file| file.path == "Also.ply")
            .unwrap();
        assert!(also.failure.is_none(), "{also:?}");
        assert_eq!(also.checks.len(), 2);
        assert_eq!(also.checks[0].name, "named");
        assert!(!also.checks[0].ok);
        assert!(also.checks[0].text.contains("nope"), "{also:?}");
        assert!(also.checks[1].ok, "{also:?}");
        let bad = report
            .files
            .iter()
            .find(|file| file.path == "Bad.ply")
            .unwrap();
        assert!(bad.checks.is_empty());
        let bad_text = bad.failure.as_deref().unwrap_or("");
        assert!(bad_text.contains("error: PL"), "{bad_text}");
        let pass = report
            .files
            .iter()
            .find(|file| file.path == "Lib.ply")
            .unwrap();
        assert!(pass.failure.is_none(), "{pass:?}");
        assert_eq!(pass.checks.len(), 2);
        assert_eq!(pass.checks[0].name, "yes");
        assert!(pass.checks[0].line > 0);
        assert!(pass.checks[0].ok);
        assert!(pass.checks[0].text.is_empty());
        assert_eq!(pass.checks[1].name, "off");
        assert!(!pass.checks[1].ok);
        assert!(pass.checks[1].text.contains("want < 1 >"), "{pass:?}");
        assert!(pass.checks[1].text.contains("got < 2 >"), "{pass:?}");
        let text = report.render(false);
        assert!(text.contains("✓ yes"), "{text}");
        assert!(text.contains("✗ off"), "{text}");
        assert!(text.contains("want < 1 >"), "{text}");
        assert!(text.contains("✗ failed to compile"), "{text}");
        assert!(text.contains("Tests  2 failed | 2 passed (4)"), "{text}");
        assert!(text.contains("Files  3 failed (3)"), "{text}");
        let lib_js = std::fs::read_to_string(temp.0.join("dist/Lib.js")).unwrap();
        assert!(!lib_js.contains("ply_check_"), "{lib_js}");
        assert!(!lib_js.contains("ply_want_"), "{lib_js}");
        assert!(lib_js.contains("function hidden"), "{lib_js}");
        assert!(!lib_js.contains("export function hidden"), "{lib_js}");
        let lib_iface = std::fs::read_to_string(temp.0.join("dist/Lib.d.ply")).unwrap();
        assert!(!lib_iface.contains('?'), "{lib_iface}");
        assert!(!temp.0.join("dist/Miss.js").is_file());
        assert!(!temp.0.join("dist/Miss.d.ply").is_file());
        assert!(
            std::fs::read_dir(&temp.0)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ply-test-")),
            "temp check directory was left behind"
        );
    }

    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("plylang-{label}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
