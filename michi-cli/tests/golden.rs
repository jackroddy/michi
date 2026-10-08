//! Every `tests/cases/x.michi` compiles to the `x.sh` beside it, and that
//! script, run under bash in an empty directory, prints the `x.out` beside
//! them. Run with `MICHI_UPDATE_GOLDEN=1` to rewrite both instead.

use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn cases_compile_to_their_scripts() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    let update = std::env::var_os("MICHI_UPDATE_GOLDEN").is_some();
    let mut failed = Vec::new();
    let mut seen = 0;
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    for path in &paths {
        if path.extension().is_some_and(|e| e == "sh") && !path.with_extension("michi").exists() {
            failed.push(format!(
                "{}: a script with no case beside it",
                path.display()
            ));
        }
        if path.extension().is_none_or(|e| e != "michi") {
            continue;
        }
        seen += 1;
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(path).unwrap();
        let script = match michi_cli::compile(&source, &name) {
            Ok(script) => script,
            Err(err) => panic!("{}", err.render(&name, &source)),
        };
        if let Some(problem) = bash_rejects(&script) {
            failed.push(format!("{name}: bash -n rejects the script:\n{problem}"));
        }
        let output = match run(&script, &name) {
            Ok(output) => output,
            Err(problem) => {
                failed.push(format!("{name}: the script failed:\n{problem}"));
                String::new()
            }
        };
        for (ext, actual) in [("sh", &script), ("out", &output)] {
            let expected_path = path.with_extension(ext);
            if update {
                std::fs::write(&expected_path, actual).unwrap();
                continue;
            }
            let Ok(expected) = std::fs::read_to_string(&expected_path) else {
                failed.push(format!(
                    "{name}: no .{ext} beside it. Run with MICHI_UPDATE_GOLDEN=1 to write one, then read it"
                ));
                continue;
            };
            if *actual != expected {
                failed.push(format!("{name}.{ext}:\n{}", diff(&expected, actual)));
            }
        }
    }
    assert!(seen > 0, "no cases under {}", dir.display());
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The first run of differing lines, with a little context.
fn diff(expected: &str, actual: &str) -> String {
    let a: Vec<&str> = expected.lines().collect();
    let b: Vec<&str> = actual.lines().collect();
    let first = (0..a.len().max(b.len()))
        .find(|&i| a.get(i) != b.get(i))
        .unwrap_or(0);
    let from = first.saturating_sub(2);
    let mut out = String::new();
    for i in from..(first + 6).min(a.len().max(b.len())) {
        match (a.get(i), b.get(i)) {
            (Some(x), Some(y)) if x == y => out.push_str(&format!("  {x}\n")),
            (x, y) => {
                if let Some(x) = x {
                    out.push_str(&format!("- {x}\n"));
                }
                if let Some(y) = y {
                    out.push_str(&format!("+ {y}\n"));
                }
            }
        }
    }
    out
}

/// The script's stdout, run under bash in a fresh directory with a fixed
/// locale, or its stderr and status when it fails.
fn run(script: &str, name: &str) -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!("michi-golden-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("run.sh");
    std::fs::write(&path, script).unwrap();
    let out = Command::new("bash")
        .arg(&path)
        .current_dir(&dir)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .expect("the golden test needs bash on the path");
    let _ = std::fs::remove_dir_all(&dir);
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// What `bash -n` says about the script, or `None` when it accepts it.
fn bash_rejects(script: &str) -> Option<String> {
    use std::io::Write;
    let mut child = Command::new("bash")
        .arg("-n")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the golden test needs bash on the path");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    if out.status.success() {
        None
    } else {
        Some(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}
