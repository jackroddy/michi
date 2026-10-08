//! The binary, driven as a user would.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn case(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/cases")
        .join(name)
}

fn michi(args: &[&str], cwd: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_michi"));
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A fresh directory for a run to write into.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("michi-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn sh_prints_the_golden_script() {
    let out = michi(&[case("shapes.michi").to_str().unwrap(), "--sh"], None);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        std::fs::read_to_string(case("shapes.sh")).unwrap()
    );
}

#[test]
fn a_pipeline_can_be_picked() {
    let path = case("shapes.michi");
    let out = michi(&["-p", "second", "--sh", path.to_str().unwrap()], None);
    assert_eq!(out.status.code(), Some(0));
    let script = text(&out.stdout);
    assert!(script.contains("# pipeline second"));
    assert!(!script.contains("# pipeline first"));

    let out = michi(&[path.to_str().unwrap(), "-p", "nope"], None);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("no pipeline named `nope`"));
}

#[test]
fn a_bad_file_exits_two() {
    let dir = scratch("bad");
    let path = dir.join("bad.michi");
    std::fs::write(&path, "pipeline p { step s { true } }").unwrap();
    let out = michi(&[path.to_str().unwrap()], None);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("missing its `;`"));

    let out = michi(&[dir.join("absent.michi").to_str().unwrap()], None);
    assert_eq!(out.status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn table_against_a_declared_one_is_an_error() {
    let out = michi(
        &[case("shapes.michi").to_str().unwrap(), "--table", "t.tbl"],
        None,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("declares a table"));
}

#[test]
fn dry_run_prints_the_plan_and_runs_nothing() {
    let dir = scratch("dry");
    let out = michi(
        &[case("shapes.michi").to_str().unwrap(), "--dry-run"],
        Some(&dir),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let plan = text(&out.stdout);
    assert!(plan.contains("# pipeline first"));
    assert!(plan.contains("# pipeline second"));
    assert!(plan.contains("echo"));
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_none(),
        "a dry run wrote files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_silent_run_prints_nothing_and_a_table_lands() {
    let dir = scratch("run");
    let out = michi(
        &[
            case("shapes.michi").to_str().unwrap(),
            "-p",
            "first",
            "--silent",
            "--table",
            "out.tbl",
        ],
        Some(&dir),
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "");
    let table = std::fs::read_to_string(dir.join("out.tbl")).unwrap();
    assert!(table.contains("echo<1,a_b>"), "{table}");
    let _ = std::fs::remove_dir_all(&dir);
}
