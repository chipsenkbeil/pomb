//! `.dev/__version_guard.sh` must stop a release whose `Cargo.toml` version equals the base's or
//! whose `Cargo.lock` pomb entry disagrees with `Cargo.toml`, so a bump that was edited but never
//! committed cannot ship under the old version. The helper reads three files and needs no git, so
//! each test writes them into a tempdir and checks the helper's exit status and its message.

use std::fs;
use std::process::{Command, Output};

fn manifest(version: &str) -> String {
    format!("[package]\nname = \"pomb\"\nversion = \"{version}\"\nedition = \"2024\"\n")
}

fn lock(version: &str) -> String {
    format!(
        "[[package]]\nname = \"other\"\nversion = \"9.9.9\"\n\n[[package]]\nname = \"pomb\"\nversion = \"{version}\"\n"
    )
}

fn run_guard(base: &str, cand: (&str, &str)) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    let base_toml = dir.path().join("base.toml");
    let cand_toml = dir.path().join("cand.toml");
    let cand_lock = dir.path().join("cand.lock");
    fs::write(&base_toml, manifest(base)).expect("write base manifest");
    fs::write(&cand_toml, manifest(cand.0)).expect("write candidate manifest");
    fs::write(&cand_lock, lock(cand.1)).expect("write candidate lock");
    Command::new("bash")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/.dev/__version_guard.sh"
        ))
        .args([&base_toml, &cand_toml, &cand_lock])
        .output()
        .expect("run guard")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn guard_should_accept_a_bumped_version_with_a_matching_lock() {
    let out = run_guard("0.5.81", ("0.5.82", "0.5.82"));
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn guard_should_refuse_a_version_equal_to_the_base() {
    let out = run_guard("0.5.81", ("0.5.81", "0.5.81"));
    assert!(!out.status.success());
    let message = stderr(&out);
    assert!(
        message.contains("Cargo.toml version 0.5.81 equals the base version 0.5.81"),
        "{message}"
    );
}

#[test]
fn guard_should_refuse_a_lock_that_differs_from_the_manifest() {
    let out = run_guard("0.5.81", ("0.5.82", "0.5.81"));
    assert!(!out.status.success());
    let message = stderr(&out);
    assert!(
        message.contains("Cargo.lock pomb version 0.5.81 differs from Cargo.toml version 0.5.82"),
        "{message}"
    );
}
