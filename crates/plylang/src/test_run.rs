//! `plylang test` finds test files, compiles them in the package, and runs them.
//!
//! A test is a public word `( -- Verdict )`. The runner calls each one and uses
//! the word as its name. A file with none of those words may export `suite`
//! `( -- (Array Verdict) )` instead. Each verdict has `ok`, `name`, and `text`.
//! `text` is empty when the check passed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::build::{
    build, compile_test_file, file_is_test_entry, package_dirs, reject_shadowed_test_modules,
    BuildError, CompiledTest,
};

const HARNESS: &str = r##"
import { pathToFileURL } from "node:url";
import { writeFileSync } from "node:fs";

const file = process.argv[2];
const out = process.argv[3];
const names = JSON.parse(process.argv[4]);
const mod = await import(pathToFileURL(file).href);

function detail(error) {
  const text = error && error.stack ? error.stack : String(error);
  return text.endsWith("\n") ? text.slice(0, -1) : text;
}

function verdict(name, row) {
  if (row == null || typeof row !== "object" || Array.isArray(row)) {
    return { ok: false, name, text: "test did not return a verdict" };
  }
  return {
    ok: row.ok === true,
    name,
    text: typeof row.text === "string" ? row.text : "",
  };
}

let report;
if (names.length > 0) {
  report = names.map((name) => {
    const run = mod[name];
    if (typeof run !== "function") {
      return { ok: false, name, text: "missing test " + name };
    }
    try {
      return verdict(name, run());
    } catch (error) {
      return { ok: false, name, text: detail(error) };
    }
  });
} else if (typeof mod.suite === "function") {
  let rows;
  try {
    rows = mod.suite();
  } catch (error) {
    process.stderr.write(detail(error) + "\n");
    process.exit(1);
  }
  if (!Array.isArray(rows)) {
    process.stderr.write("suite must return an array of verdicts\n");
    process.exit(3);
  }
  report = rows.map((row, index) => {
    const name = typeof row?.name === "string" && row.name.length > 0 ? row.name : "#" + (index + 1);
    return verdict(name, row);
  });
} else {
  process.stderr.write("no tests\n");
  process.exit(3);
}
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
    /// 1-based line of the word. `0` when the check came from `suite`.
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
    } else if first.starts_with("missing suite") || first.starts_with("suite must return") {
        "failed to run"
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

/// Find `pattern` under the package that contains `start`, build that package,
/// compile each test, and run its `suite`.
pub fn test_package(start: &Path, pattern: &str) -> Result<TestReport, BuildError> {
    let started = Instant::now();
    let (root, out) = package_dirs(start)?;
    let files = match_tests(&root, &out, pattern)?;
    if files.is_empty() {
        return Err(BuildError::plain(format!("no tests matched {pattern}")));
    }
    build(&root)?;
    reject_shadowed_test_modules(&root)?;
    let mut entries = Vec::new();
    for file in files {
        if file_is_test_entry(&file)? {
            entries.push(file);
        }
    }
    if entries.is_empty() {
        return Err(BuildError::plain(format!("no tests matched {pattern}")));
    }
    let mut reports = Vec::new();
    for file in entries {
        reports.push(run_file(&root, &out, &file));
    }
    Ok(TestReport {
        files: reports,
        duration: started.elapsed(),
    })
}

fn match_tests(root: &Path, out: &Path, pattern: &str) -> Result<Vec<PathBuf>, BuildError> {
    let mut found = Vec::new();
    walk_tests(root, root, out, pattern, &mut found)?;
    found.sort();
    Ok(found)
}

fn walk_tests(
    root: &Path,
    dir: &Path,
    out: &Path,
    pattern: &str,
    found: &mut Vec<PathBuf>,
) -> Result<(), BuildError> {
    if dir.starts_with(out) && dir != root {
        return Ok(());
    }
    let entries = fs::read_dir(dir).map_err(|error| crate::build::os_error(dir, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| crate::build::os_error(dir, error))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name.starts_with('.') || name == "node_modules" || path == *out {
                continue;
            }
            walk_tests(root, &path, out, pattern, found)?;
            continue;
        }
        if !name.ends_with(".ply") || name.ends_with(".d.ply") {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if glob_match(pattern, &relative) {
            found.push(path);
        }
    }
    Ok(())
}

fn run_file(root: &Path, out: &Path, file: &Path) -> FileReport {
    let started = Instant::now();
    let path = file
        .strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    let compiled = match compile_test_file(root, file) {
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
    let (checks, failure) = match execute(root, out, &compiled) {
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

fn execute(root: &Path, out: &Path, compiled: &CompiledTest) -> Result<Vec<Check>, String> {
    if let Some(parent) = compiled.js_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    fs::write(&compiled.js_path, &compiled.js).map_err(|error| error.to_string())?;
    // Stop at `ply.out`. Empty parents under it go away. The output directory stays.
    let _cleanup = (!compiled.keep).then(|| JsCleanup {
        file: compiled.js_path.clone(),
        stop: out.to_path_buf(),
    });
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
        harness.file_name().and_then(|name| name.to_str()).unwrap_or("run")
    ));
    let names: Vec<&str> = compiled.words.iter().map(|word| word.name.as_str()).collect();
    let words = serde_json::to_string(&names).map_err(|error| error.to_string())?;
    let output = Command::new("node")
        .arg(&harness)
        .arg(&compiled.js_path)
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
        return read_checks(&bytes).map(|checks| locate(&compiled.words, checks));
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

struct JsCleanup {
    file: PathBuf,
    stop: PathBuf,
}

impl Drop for JsCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.file);
        let mut dir = self.file.parent().map(Path::to_path_buf);
        while let Some(current) = dir {
            if current == self.stop || !current.starts_with(&self.stop) {
                break;
            }
            if fs::remove_dir(&current).is_err() {
                break;
            }
            dir = current.parent().map(Path::to_path_buf);
        }
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
        .ok_or_else(|| "suite report was not an array".to_string())?;
    Ok(rows
        .iter()
        .map(|row| Check {
            ok: row.get("ok").and_then(|value| value.as_bool()).unwrap_or(false),
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

fn locate(words: &[crate::build::TestWord], mut checks: Vec<Check>) -> Vec<Check> {
    for (check, word) in checks.iter_mut().zip(words) {
        check.line = word.line;
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
    fn a_package_runs_matching_files_and_deletes_the_ephemeral_javascript() {
        let temp = Temp::new("ply-test-run");
        temp.write(
            "package.json",
            "{\n  \"name\": \"fixture\",\n  \"type\": \"module\",\n  \"ply\": { \"source\": \"src\", \"foreign\": \"foreign\", \"out\": \"dist\", \"language\": 1 }\n}\n",
        );
        temp.write("src/Lib.ply", ": answer ( -- Num )\n  1\n");
        temp.write(
            "src/Also.ply",
            "\
; Verdict = Verdict ok : Bool, name : Str, text : Str

: foreign push ( (Array Verdict) Verdict -- (Array Verdict) )

: suite ( -- (Array Verdict) )
  <>
  true \"kept\" \"\" Verdict push
",
        );
        temp.write(
            "foreign/Also.js",
            "export function push(array, value) {\n  return array.concat([value]);\n}\n",
        );
        temp.write(
            "test/Pass.ply",
            "\
~ \"Lib\" (answer)

; Verdict = Verdict ok : Bool, name : Str, text : Str

: yes ( -- Verdict )
  answer drop
  true \"ignored\" \"\" Verdict

: no ( -- Verdict )
  false \"\" \"no: 1 !== 2\" Verdict
",
        );
        temp.write("test/Bad.ply", ": suite ( -- Int )\n  \"nope\"\n");
        temp.write("test/Miss.ply", ": other ( -- Num )\n  1\n");

        let missing = super::test_package(&temp.0, "missing/**/*.ply").unwrap_err();
        assert_eq!(missing.render(), "no tests matched missing/**/*.ply");
        assert!(!temp.0.join("dist").exists());

        let report = super::test_package(&temp.0, "test/**/*.ply")
            .unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(report.failed());
        assert_eq!(report.files.len(), 2, "{report:?}");
        let bad = &report.files[0];
        assert_eq!(bad.path, "test/Bad.ply");
        assert!(bad.checks.is_empty());
        let bad_text = bad.failure.as_deref().unwrap_or("");
        assert!(bad_text.contains("error: PL"), "{bad_text}");
        let pass = &report.files[1];
        assert_eq!(pass.path, "test/Pass.ply");
        assert!(pass.failure.is_none(), "{pass:?}");
        assert_eq!(pass.checks.len(), 2);
        assert_eq!(pass.checks[0].name, "yes");
        assert_eq!(pass.checks[0].line, 5);
        assert!(pass.checks[0].ok);
        assert!(pass.checks[0].text.is_empty());
        assert_eq!(pass.checks[1].name, "no");
        assert_eq!(pass.checks[1].line, 9);
        assert!(!pass.checks[1].ok);
        assert_eq!(pass.checks[1].text, "no: 1 !== 2");
        let text = report.render(false);
        assert!(text.contains("✓ yes"), "{text}");
        assert!(text.contains("✗ no"), "{text}");
        assert!(text.contains("no: 1 !== 2"), "{text}");
        assert!(text.contains("✗ failed to compile"), "{text}");
        assert!(text.contains("Tests  1 failed | 1 passed (2)"), "{text}");
        assert!(text.contains("Files  2 failed (2)"), "{text}");
        assert!(!temp.0.join("dist/test/Pass.js").exists());
        assert!(!temp.0.join("dist/test/Miss.js").exists());
        assert!(temp.0.join("dist/Lib.js").is_file());
        assert!(temp.0.join("dist/Also.js").is_file());
        assert!(!temp.0.join("src/Run.ply").exists());

        let kept = super::test_package(&temp.0, "src/Also.ply")
            .unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!kept.failed());
        assert_eq!(kept.files.len(), 1);
        assert_eq!(kept.files[0].checks[0].name, "kept");
        assert!(kept.files[0].checks[0].ok);
        assert!(temp.0.join("dist/Also.js").is_file());
    }

    struct Temp(std::path::PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "plylang-{label}-{}-{nanos}",
                std::process::id()
            ));
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
