//! `.dev/__gate.sh` must accept a gate only when ci ran it as exactly one new passed run of the
//! release worktree, so a gate that dev sent to another tree stops the release before the push.
//! Stub `ci` and `dev` scripts on `PATH` play the run list before and after the gate. Each test
//! feeds one shape of run list and checks the helper exits non-zero unless it is one new passed
//! run with source local.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const WORKTREE: &str = "release-1";
const REPO: &str = "release-1-0123456789abcdef";

fn run_row(id: &str, status: &str, source: &str) -> String {
    repo_row(REPO, id, status, source)
}

fn repo_row(repo: &str, id: &str, status: &str, source: &str) -> String {
    format!(
        r#"{{"run":"{id}","repo":"{repo}","source":"{source}","kind":"run","status":"{status}"}}"#
    )
}

fn runs_doc(rows: &[String]) -> String {
    format!(r#"{{"protocol":48,"runs":[{}]}}"#, rows.join(","))
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

fn run_gate(before: &[String], after: &[String], dev_exit: i32) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).expect("bin dir");
    let worktree = dir.path().join(WORKTREE);
    fs::create_dir(&worktree).expect("worktree dir");
    let state = dir.path().join("runs.json");
    let next = dir.path().join("next.json");
    fs::write(&state, runs_doc(before)).expect("write before");
    fs::write(&next, runs_doc(after)).expect("write after");
    write_script(&bin.join("ci"), &format!("cat '{}'", state.display()));
    write_script(
        &bin.join("dev"),
        &format!(
            "cp '{}' '{}'\nexit {dev_exit}",
            next.display(),
            state.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").expect("PATH"));
    let log = dir.path().join("gate.log");
    Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/.dev/__gate.sh"))
        .args([
            worktree.to_str().expect("utf8"),
            "lint",
            log.to_str().expect("utf8"),
        ])
        .env("PATH", path)
        .output()
        .expect("run gate")
}

#[test]
fn gate_should_accept_one_new_passed_local_run() {
    let old = run_row("aaaaaa", "passed", "local");
    let after = [old.clone(), run_row("bbbbbb", "passed", "local")];
    let out = run_gate(&[old], &after, 0);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("bbbbbb {REPO} local")
    );
}

#[test]
fn gate_should_ignore_new_runs_of_other_repos() {
    let after = [
        repo_row("questions-0123456789abcdef", "aaaaaa", "running", "local"),
        run_row("bbbbbb", "passed", "local"),
        repo_row("release-10-0123456789abcdef", "cccccc", "passed", "local"),
    ];
    let out = run_gate(&[], &after, 0);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("bbbbbb {REPO} local")
    );
}

#[test]
fn gate_should_refuse_when_no_new_run_appears_under_the_worktree() {
    let runs = [run_row("aaaaaa", "passed", "local")];
    let out = run_gate(&runs, &runs, 0);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("did not test this worktree"));
}

#[test]
fn gate_should_refuse_two_new_runs() {
    let out = run_gate(
        &[],
        &[
            run_row("aaaaaa", "passed", "local"),
            run_row("bbbbbb", "passed", "local"),
        ],
        0,
    );
    assert!(!out.status.success());
}

#[test]
fn gate_should_refuse_a_run_that_did_not_pass() {
    let out = run_gate(&[], &[run_row("aaaaaa", "failed", "local")], 0);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected passed from local"));
}

#[test]
fn gate_should_refuse_a_dirty_source() {
    let out = run_gate(&[], &[run_row("aaaaaa", "passed", "local(dirty)")], 0);
    assert!(!out.status.success());
}

#[test]
fn gate_should_refuse_when_dev_fails() {
    let out = run_gate(&[], &[run_row("aaaaaa", "passed", "local")], 1);
    assert!(!out.status.success());
}
