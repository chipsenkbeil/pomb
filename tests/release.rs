//! `.dev/release.sh` must cut a release from the remote's main: one commit that changes only the
//! version in `Cargo.toml` and `Cargo.lock`, checked on ci and pushed to main, and nothing else.
//! Each test builds a bare remote with a tagged release and a card commit after it, a clone that
//! holds copies of the `.dev` scripts, and stub `ci`, `dev`, `jj-git-guard` and `nix` scripts on
//! `PATH`. The stubs record which gates ran and push through plain git, so no test reaches a real
//! remote or ci.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RELEASE: [&str; 4] = ["--batch", "7", "--summary", "cut it"];

const SCRIPTS: [&str; 7] = [
    "release.sh",
    "__bump_version.sh",
    "__version_guard.sh",
    "__push_main.sh",
    "__push_tag.sh",
    "__gate.sh",
    "__remote.sh",
];

struct Fixture {
    _dir: tempfile::TempDir,
    remote: PathBuf,
    repo: PathBuf,
    bin: PathBuf,
    gates: PathBuf,
}

fn manifest(version: &str) -> String {
    format!("[package]\nname = \"pomb\"\nversion = \"{version}\"\nedition = \"2024\"\n")
}

fn lock(version: &str) -> String {
    format!(
        "[[package]]\nname = \"other\"\nversion = \"9.9.9\"\n\n[[package]]\nname = \"pomb\"\nversion = \"{version}\"\n"
    )
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit_version(repo: &Path, version: &str, message: &str) {
    fs::write(repo.join("Cargo.toml"), manifest(version)).expect("write manifest");
    fs::write(repo.join("Cargo.lock"), lock(version)).expect("write lock");
    git(repo, &["add", "Cargo.toml", "Cargo.lock"]);
    git(repo, &["commit", "-q", "-m", message]);
}

fn commit_card(repo: &Path, name: &str) {
    fs::write(repo.join(name), "card\n").expect("write card file");
    git(repo, &["add", name]);
    git(repo, &["commit", "-q", "-m", &format!("Add {name}")]);
}

fn fixture(tagged_version: &str, main_version: &str, cards_after_tag: usize) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let remote = dir.path().join("remote.git");
    let repo = dir.path().join("repo");
    let bin = dir.path().join("bin");
    let gates = dir.path().join("gates.log");
    fs::create_dir(&bin).expect("bin dir");
    git(
        dir.path(),
        &["init", "-q", "--bare", "-b", "main", "remote.git"],
    );
    git(dir.path(), &["init", "-q", "-b", "main", "repo"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().expect("utf8")],
    );
    commit_version(&repo, tagged_version, "Release batch-1: first");
    git(
        &repo,
        &["tag", "-a", &format!("v{tagged_version}"), "-m", "release"],
    );
    for n in 0..cards_after_tag {
        commit_card(&repo, &format!("card{n}"));
    }
    if main_version != tagged_version {
        commit_version(&repo, main_version, "Change the version by hand");
    }
    git(&repo, &["push", "-q", "origin", "main", "--tags"]);
    let dev = repo.join(".dev");
    fs::create_dir(&dev).expect("dev dir");
    for script in SCRIPTS {
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".dev")
                .join(script),
            dev.join(script),
        )
        .expect("copy script");
    }
    fs::write(repo.join(".git/info/exclude"), ".dev/\n").expect("exclude");
    let state = dir.path().join("runs.json");
    fs::write(&state, r#"{"protocol":48,"runs":[]}"#).expect("write runs");
    write_script(&bin.join("ci"), &format!("cat '{}'", state.display()));
    write_script(
        &bin.join("dev"),
        &format!(
            "echo \"$1\" >>'{gates}'\n\
             [ -z \"$MOVE_MAIN\" ] || git -C \"$MOVE_MAIN\" push -q origin HEAD:main\n\
             [ \"$1\" != \"$FAIL_GATE\" ] || exit 1\n\
             n=$(jq '.runs|length' '{state}')\n\
             jq --arg id \"run$n\" '.runs += [{{run:$id,repo:\"release-7-0123\",source:\"local\",kind:\"run\",status:\"passed\"}}]' '{state}' >'{state}.next'\n\
             mv '{state}.next' '{state}'\n\
             echo 'Summary [1s] 1 tests run: 1 passed'",
            gates = gates.display(),
            state = state.display(),
        ),
    );
    write_script(
        &bin.join("jj-git-guard"),
        "[ \"$1\" != push ] || shift\n[ \"$1\" != -- ] || shift\nexec git push \"$@\"",
    );
    write_script(&bin.join("nix"), "echo sha256-stubhash");
    Fixture {
        _dir: dir,
        remote,
        repo,
        bin,
        gates,
    }
}

fn run_release(fx: &Fixture, args: &[&str], fail_gate: &str) -> Output {
    run_release_moving(fx, args, fail_gate, "")
}

fn run_release_moving(fx: &Fixture, args: &[&str], fail_gate: &str, move_main: &str) -> Output {
    let path = format!(
        "{}:{}",
        fx.bin.display(),
        std::env::var("PATH").expect("PATH")
    );
    Command::new("bash")
        .arg(fx.repo.join(".dev/release.sh"))
        .args(args)
        .current_dir(&fx.repo)
        .env("PATH", path)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("FAIL_GATE", fail_gate)
        .env("MOVE_MAIN", move_main)
        .output()
        .expect("run release")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn gates_run(fx: &Fixture) -> Vec<String> {
    fs::read_to_string(&fx.gates)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn release_should_commit_only_the_version_bump_on_the_remote_main() {
    let fx = fixture("0.5.85", "0.5.85", 2);
    let card_head = git(&fx.remote, &["rev-parse", "main"]);
    let out = run_release(&fx, &RELEASE, "");
    assert!(out.status.success(), "{}", stderr(&out));
    let head = git(&fx.remote, &["rev-parse", "main"]);
    assert_eq!(git(&fx.remote, &["rev-parse", "main~1"]), card_head);
    assert_eq!(
        git(&fx.remote, &["log", "-1", "--format=%s", "main"]),
        "Release batch-7: cut it"
    );
    let files = git(&fx.remote, &["diff", "--name-only", "main~1", "main"]);
    assert_eq!(
        files.lines().collect::<Vec<_>>(),
        ["Cargo.lock", "Cargo.toml"]
    );
    let toml = git(&fx.remote, &["show", "main:Cargo.toml"]);
    assert!(toml.contains("version = \"0.5.86\""), "{toml}");
    assert_eq!(gates_run(&fx), ["lint", "test", "nixcheck", "build"]);
    let printed = stdout(&out);
    assert!(printed.contains(&format!("Pomb sha: {head}")), "{printed}");
    assert!(printed.contains("sha256-stubhash"), "{printed}");
}

#[test]
fn release_should_refuse_when_main_has_no_commit_since_the_last_tag() {
    let fx = fixture("0.5.85", "0.5.85", 0);
    let before = git(&fx.remote, &["rev-parse", "main"]);
    let out = run_release(&fx, &RELEASE, "");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no commit since v0.5.85"),
        "{}",
        stderr(&out)
    );
    assert!(gates_run(&fx).is_empty());
    assert_eq!(git(&fx.remote, &["rev-parse", "main"]), before);
}

#[test]
fn release_should_refuse_a_version_equal_to_the_last_tag() {
    let fx = fixture("0.5.85", "0.5.84", 1);
    let before = git(&fx.remote, &["rev-parse", "main"]);
    let out = run_release(&fx, &RELEASE, "");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("equals the base version 0.5.85"),
        "{}",
        stderr(&out)
    );
    assert!(gates_run(&fx).is_empty());
    assert_eq!(git(&fx.remote, &["rev-parse", "main"]), before);
}

#[test]
fn release_should_stop_before_the_push_when_a_gate_fails() {
    let fx = fixture("0.5.85", "0.5.85", 1);
    let before = git(&fx.remote, &["rev-parse", "main"]);
    let out = run_release(&fx, &RELEASE, "test");
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("dev test --remote"),
        "{}",
        stderr(&out)
    );
    assert_eq!(gates_run(&fx), ["lint", "test"]);
    assert_eq!(git(&fx.remote, &["rev-parse", "main"]), before);
}

#[test]
fn release_dry_run_should_skip_the_push_and_hash_only_should_print_the_block() {
    let fx = fixture("0.5.85", "0.5.85", 1);
    let before = git(&fx.remote, &["rev-parse", "main"]);
    let dry = run_release(&fx, &[&RELEASE[..], &["--dry-run"]].concat(), "");
    assert!(dry.status.success(), "{}", stderr(&dry));
    assert!(
        stdout(&dry).contains("dry run, not pushed"),
        "{}",
        stdout(&dry)
    );
    assert!(!stdout(&dry).contains("homelab request"));
    assert_eq!(gates_run(&fx).len(), 4);
    assert_eq!(git(&fx.remote, &["rev-parse", "main"]), before);
    let out = run_release(&fx, &["--hash-only", &before, "--env", "KEY=VALUE"], "");
    assert!(out.status.success(), "{}", stderr(&out));
    let printed = stdout(&out);
    assert!(
        printed.contains(&format!("Pomb sha: {before}")),
        "{printed}"
    );
    assert!(
        printed.contains("nix hash:     sha256-stubhash"),
        "{printed}"
    );
    assert!(printed.contains("  KEY=VALUE"), "{printed}");
}

#[test]
fn release_should_exit_2_with_the_usage_for_a_commit_argument() {
    let fx = fixture("0.5.85", "0.5.85", 1);
    let out = run_release(&fx, &["HEAD", "--batch", "7", "--summary", "cut it"], "");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("usage: dev release --batch N"),
        "{}",
        stderr(&out)
    );
    assert!(gates_run(&fx).is_empty());
}

#[test]
fn release_should_refuse_to_push_when_main_moved_during_the_gates() {
    let fx = fixture("0.5.85", "0.5.85", 1);
    let root = fx.repo.parent().expect("parent");
    let remote = fx.remote.to_str().expect("utf8");
    git(root, &["clone", "-q", "-b", "main", remote, "rival"]);
    let rival = root.join("rival");
    commit_card(&rival, "late");
    let out = run_release_moving(&fx, &RELEASE, "", rival.to_str().expect("utf8"));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("moved past"), "{}", stderr(&out));
    assert_eq!(
        git(&fx.remote, &["rev-parse", "main"]),
        git(&rival, &["rev-parse", "HEAD"])
    );
}

fn assert_tags_at(fx: &Fixture, sha: &str) {
    assert_eq!(
        git(&fx.remote, &["rev-parse", "refs/tags/batch-7^{commit}"]),
        sha
    );
    assert_eq!(
        git(&fx.remote, &["rev-parse", "refs/tags/v0.5.86^{commit}"]),
        sha
    );
}

#[test]
fn release_should_tag_batch_and_version_at_the_pushed_sha() {
    let fx = fixture("0.5.85", "0.5.85", 2);
    let out = run_release(&fx, &RELEASE, "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_tags_at(&fx, &git(&fx.remote, &["rev-parse", "main"]));
}

#[test]
fn release_should_not_tag_on_a_dry_run() {
    let fx = fixture("0.5.85", "0.5.85", 1);
    let out = run_release(&fx, &[&RELEASE[..], &["--dry-run"]].concat(), "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        git(&fx.remote, &["tag", "-l"]).lines().collect::<Vec<_>>(),
        ["v0.5.85"]
    );
    assert_eq!(
        git(&fx.repo, &["tag", "-l"]).lines().collect::<Vec<_>>(),
        ["v0.5.85"]
    );
}

#[test]
fn release_should_print_the_tag_rerun_command_when_tagging_fails() {
    let fx = fixture("0.5.85", "0.5.85", 1);
    git(&fx.remote, &["tag", "batch-7", "main"]);
    let out = run_release(&fx, &RELEASE, "");
    assert!(!out.status.success());
    let head = git(&fx.remote, &["rev-parse", "main"]);
    let err = stderr(&out);
    assert!(err.contains("rerun: "), "{err}");
    assert!(
        err.contains(&format!("__push_tag.sh batch-7 {head}")),
        "{err}"
    );
    assert!(err.contains("dev release --hash-only"), "{err}");
    assert!(!err.contains("__push_tag.sh v0.5.86"), "{err}");
    assert_eq!(
        git(&fx.remote, &["rev-parse", "refs/tags/v0.5.86^{commit}"]),
        head
    );
    assert!(
        !stdout(&out).contains("homelab request"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn release_should_print_the_tag_rerun_command_when_the_tag_push_fails() {
    let fx = fixture("0.5.85", "0.5.85", 1);
    write_script(
        &fx.remote.join("hooks/pre-receive"),
        "while read o n r; do [ \"$r\" != refs/tags/batch-7 ] || exit 1; done",
    );
    let out = run_release(&fx, &RELEASE, "");
    assert!(!out.status.success());
    let head = git(&fx.remote, &["rev-parse", "main"]);
    let err = stderr(&out);
    assert!(
        err.contains(&format!("__push_tag.sh batch-7 {head}")),
        "{err}"
    );
    assert!(git(&fx.repo, &["tag", "-l", "batch-7"]).is_empty());
    assert!(git(&fx.remote, &["tag", "-l", "batch-7"]).is_empty());
    assert_eq!(
        git(&fx.remote, &["rev-parse", "refs/tags/v0.5.86^{commit}"]),
        head
    );
}

#[test]
fn release_should_print_the_block_after_both_tags_are_pushed() {
    let fx = fixture("0.5.85", "0.5.85", 2);
    let out = run_release(&fx, &RELEASE, "");
    assert!(out.status.success(), "{}", stderr(&out));
    let head = git(&fx.remote, &["rev-parse", "main"]);
    assert_tags_at(&fx, &head);
    let printed = stdout(&out);
    assert!(printed.contains("homelab request"), "{printed}");
    assert!(printed.contains(&format!("Pomb sha: {head}")), "{printed}");
    assert!(printed.contains("batch:        7"), "{printed}");
}
