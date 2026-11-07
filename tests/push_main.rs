//! `.dev/__push_main.sh` refuses attribution and stray trailers with exit 1, pushes HEAD to main
//! on the remote of record, and exits 3 when the remote refuses the push as non-fast-forward,
//! to let a caller tell main moving on from a real failure. Each test builds a bare remote and a
//! clone in a tempdir and puts a stub `jj-git-guard` first on PATH that forwards to `git push`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const CHANGE_ID: &str = "Change-Id: I0123456789abcdef0123456789abcdef01234567";

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = Self { dir };
        git(fixture.dir.path(), &["init", "-q", "--bare", "remote.git"]);
        git(fixture.dir.path(), &["clone", "-q", "remote.git", "work"]);
        git(&fixture.work(), &["checkout", "-q", "-b", "main"]);
        fixture.commit("base");
        git(&fixture.work(), &["push", "-q", "origin", "main"]);
        let bin = fixture.dir.path().join("bin");
        fs::create_dir(&bin).expect("create bin");
        let stub = bin.join("jj-git-guard");
        fs::write(&stub, "#!/bin/sh\nshift 2\nexec git push \"$@\"\n").expect("write stub");
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        fixture
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn commit(&self, message: &str) {
        git(
            &self.work(),
            &["commit", "-q", "--allow-empty", "-m", message],
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").expect("PATH")
        );
        Command::new("bash")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/.dev/__push_main.sh"))
            .arg(self.work())
            .args(args)
            .env("PATH", path)
            .output()
            .expect("run push helper")
    }

    fn remote_main(&self) -> String {
        rev_parse(&self.dir.path().join("remote.git"), "refs/heads/main")
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rev_parse(dir: &Path, name: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", name])
        .output()
        .expect("run git rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn push_main_should_push_head_to_main_when_clean() {
    let fixture = Fixture::new();
    fixture.commit("add a change");
    let out = fixture.run(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fixture.remote_main(), rev_parse(&fixture.work(), "HEAD"));
}

#[test]
fn push_main_should_exit_three_when_the_remote_refuses_non_fast_forward() {
    let fixture = Fixture::new();
    let other = fixture.dir.path().join("other");
    git(
        fixture.dir.path(),
        &["clone", "-q", "-b", "main", "remote.git", "other"],
    );
    git(
        &other,
        &["commit", "-q", "--allow-empty", "-m", "main moved on"],
    );
    git(&other, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    fixture.commit("add a change");
    let out = fixture.run(&[]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert_eq!(fixture.remote_main(), rev_parse(&other, "HEAD"));
}

#[test]
fn push_main_should_exit_one_for_a_claude_session_trailer() {
    let fixture = Fixture::new();
    fixture.commit("add a change\n\nClaude-Session: abc123");
    let out = fixture.run(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("Claude-Session"), "{}", stderr(&out));
}

#[test]
fn push_main_should_exit_one_for_a_trailer_other_than_change_id() {
    let fixture = Fixture::new();
    fixture.commit("add a change\n\nReviewed-by: Someone <someone@example.com>");
    let out = fixture.run(&[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("Reviewed-by"), "{}", stderr(&out));
}

#[test]
fn push_main_should_accept_a_change_id_trailer() {
    let fixture = Fixture::new();
    fixture.commit(&format!("add a change\n\n{CHANGE_ID}"));
    let out = fixture.run(&["--check"]);
    assert!(out.status.success(), "{}", stderr(&out));
}
