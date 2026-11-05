//! `.dev/__bump_version.sh DIR` must raise the patch number of the package version in `Cargo.toml`
//! and the pomb entry in `Cargo.lock`, touch nothing else, print the new version, and refuse a lock
//! without a pomb entry before it writes either file. Each test writes both files into a tempdir,
//! runs the helper on it and checks the files, its output and its exit status.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn manifest(version: &str) -> String {
    format!(
        "[package]\n\
         name = \"pomb\"\n\
         version = \"{version}\"\n\
         edition = \"2024\"\n\n\
         [dependencies.helper]\n\
         version = \"0.5.85\"\n"
    )
}

fn lock(version: &str) -> String {
    format!(
        "version = 4\n\n\
         [[package]]\n\
         name = \"other\"\n\
         version = \"0.5.85\"\n\
         source = \"registry+https://github.com/rust-lang/crates.io-index\"\n\n\
         [[package]]\n\
         name = \"pomb\"\n\
         version = \"{version}\"\n\
         dependencies = [\n \"other\",\n]\n\n\
         [[package]]\n\
         name = \"extra\"\n\
         version = \"9.9.9\"\n"
    )
}

fn lock_without_pomb() -> String {
    "version = 4\n\n[[package]]\nname = \"other\"\nversion = \"0.5.85\"\n".to_string()
}

fn write_tree(manifest_text: &str, lock_text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("Cargo.toml"), manifest_text).expect("write manifest");
    fs::write(dir.path().join("Cargo.lock"), lock_text).expect("write lock");
    dir
}

fn read(dir: &Path, name: &str) -> String {
    fs::read_to_string(dir.join(name)).expect("read file")
}

fn run_bump(dir: &Path) -> Output {
    Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/.dev/__bump_version.sh"
        ))
        .arg(dir)
        .output()
        .expect("run bump")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn bump_should_raise_the_patch_in_manifest_and_lock() {
    let dir = write_tree(&manifest("0.5.85"), &lock("0.5.85"));
    let out = run_bump(dir.path());
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(read(dir.path(), "Cargo.toml"), manifest("0.5.86"));
    assert_eq!(read(dir.path(), "Cargo.lock"), lock("0.5.86"));
}

#[test]
fn bump_should_leave_other_lock_packages_and_dependency_versions_alone() {
    let dir = write_tree(&manifest("0.5.85"), &lock("0.5.85"));
    let out = run_bump(dir.path());
    assert!(out.status.success(), "{}", stderr(&out));
    let manifest_after = read(dir.path(), "Cargo.toml");
    let lock_after = read(dir.path(), "Cargo.lock");
    assert!(
        manifest_after.contains("[dependencies.helper]\nversion = \"0.5.85\"\n"),
        "{manifest_after}"
    );
    assert_eq!(lock_after, lock("0.5.86"));
}

#[test]
fn bump_should_print_the_new_version() {
    let dir = write_tree(&manifest("0.5.85"), &lock("0.5.85"));
    let out = run_bump(dir.path());
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "0.5.86\n");
}

#[test]
fn bump_should_refuse_a_lock_without_a_pomb_entry() {
    let dir = write_tree(&manifest("0.5.85"), &lock_without_pomb());
    let out = run_bump(dir.path());
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no pomb entry in"),
        "{}",
        stderr(&out)
    );
    assert_eq!(read(dir.path(), "Cargo.toml"), manifest("0.5.85"));
    assert_eq!(read(dir.path(), "Cargo.lock"), lock_without_pomb());
}

#[test]
fn bump_should_refuse_a_version_that_is_not_three_numbers() {
    let dir = write_tree(&manifest("0.5"), &lock("0.5"));
    let out = run_bump(dir.path());
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("is not MAJOR.MINOR.PATCH"),
        "{}",
        stderr(&out)
    );
    assert_eq!(read(dir.path(), "Cargo.toml"), manifest("0.5"));
    assert_eq!(read(dir.path(), "Cargo.lock"), lock("0.5"));
}

#[test]
fn bump_output_should_pass_the_version_guard_against_the_old_manifest() {
    let dir = write_tree(&manifest("0.5.85"), &lock("0.5.85"));
    let base = dir.path().join("base.toml");
    fs::write(&base, manifest("0.5.85")).expect("write base manifest");
    let out = run_bump(dir.path());
    assert!(out.status.success(), "{}", stderr(&out));
    let guard = Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/.dev/__version_guard.sh"
        ))
        .arg(&base)
        .arg(dir.path().join("Cargo.toml"))
        .arg(dir.path().join("Cargo.lock"))
        .output()
        .expect("run guard");
    assert!(guard.status.success(), "{}", stderr(&guard));
}
