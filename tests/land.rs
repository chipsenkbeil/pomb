//! `.dev/land.sh` applies one verified commit onto the remote's current main, refuses attribution
//! and package version changes before any gate, runs lint then test on ci through `__gate.sh`,
//! and pushes only after both pass. A landed card moves to Release, and the worktree and branch
//! its commit came from are removed. Each test copies the land script and its helpers into a clone
//! of a bare remote in a tempdir, and puts stub `dev`, `ci`, `jj` and `jj-git-guard` scripts
//! first on `PATH`: the stub `dev` records each verb and adds a ci run row that passed unless a
//! `fail-<verb>` file exists, and keeps each `dev board` call apart in `notes`. The stub
//! `jj-git-guard` logs its arguments in `pushes` and forwards to `git push`, except that it
//! first moves the remote's main and refuses the push as many times as the `refuse` file says,
//! and refuses with a plain denial while a `fail-push` file exists.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const CARD: &str = "abc123";
const RELEASE: &str = "release";

const HELPERS: [&str; 4] = ["land.sh", "__gate.sh", "__push_main.sh", "__remote.sh"];

fn manifest(version: &str) -> String {
    format!("[package]\nname = \"pomb\"\nversion = \"{version}\"\n")
}

fn lock(version: &str) -> String {
    format!("[[package]]\nname = \"pomb\"\nversion = \"{version}\"\n")
}

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = Self { dir };
        let root = fixture.dir.path();
        git(root, &["init", "-q", "--bare", "remote.git"]);
        git(root, &["clone", "-q", "remote.git", "work"]);
        let work = fixture.work();
        git(&work, &["checkout", "-q", "-b", "main"]);
        fs::write(work.join("Cargo.toml"), manifest("1.0.0")).expect("write manifest");
        fs::write(work.join("Cargo.lock"), lock("1.0.0")).expect("write lock");
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "base"]);
        git(&work, &["push", "-q", "origin", "main"]);
        fs::create_dir_all(work.join(".dev")).expect("dev dir");
        for helper in HELPERS {
            let from = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".dev")
                .join(helper);
            fs::copy(from, work.join(".dev").join(helper)).expect("copy helper");
        }
        fixture.stubs();
        fixture
    }

    fn stubs(&self) {
        let bin = self.dir.path().join("bin");
        fs::create_dir(&bin).expect("create bin");
        let state = self.dir.path().join("runs.jsonl");
        let calls = self.calls_path();
        let notes = self.dir.path().join("notes");
        let fails = self.dir.path();
        write_script(
            &bin.join("dev"),
            &format!(
                "if [ \"$1\" = board ]; then echo \"$*\" >>'{notes}'; exit 0; fi\ntouch '{state}'\necho \"$1\" >>'{calls}'\nstatus=passed\nexit_code=0\nif [ -e '{fails}/fail-'\"$1\" ]; then status=failed; exit_code=1; fi\nprintf '{{\"run\":\"r%s\",\"repo\":\"%s-0123456789abcdef\",\"source\":\"local\",\"status\":\"%s\"}}\\n' \"$(wc -l <'{state}')\" \"$(basename \"$PWD\")\" \"$status\" >>'{state}'\nexit $exit_code",
                calls = calls.display(),
                notes = notes.display(),
                fails = fails.display(),
                state = state.display(),
            ),
        );
        write_script(
            &bin.join("ci"),
            &format!(
                "touch '{state}'\njq -s '{{runs: .}}' '{state}'",
                state = state.display()
            ),
        );
        let root = self.dir.path().display();
        write_script(
            &bin.join("jj"),
            &format!(
                "echo \"$*\" >>'{root}/jjcalls'\n[ \"$1\" = --no-pager ] && shift\nif [ \"$1 $2\" = 'workspace list' ]; then cat '{root}/workspaces' 2>/dev/null; fi\nif [ \"$1 $2\" = 'bookmark list' ]; then cat '{root}/bookmarks' 2>/dev/null; fi\nexit 0"
            ),
        );
        write_script(
            &bin.join("jj-git-guard"),
            &format!(
                r#"echo "$*" >>'{root}/pushes'
if [ -e '{root}/fail-push' ]; then echo 'remote: denied' >&2; exit 1; fi
n=$(cat '{root}/refuse' 2>/dev/null || echo 0)
if [ "$n" -gt 0 ]; then
echo $((n - 1)) >'{root}/refuse'
g='{root}/remote.git'
c=$(git --git-dir="$g" commit-tree -p main -m moved "$(git --git-dir="$g" rev-parse 'main^{{tree}}')")
git --git-dir="$g" update-ref refs/heads/main "$c"
echo '! [rejected] main -> main (fetch first)' >&2
exit 1
fi
shift 2
exec git push "$@""#
            ),
        );
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn calls_path(&self) -> PathBuf {
        self.dir.path().join("calls")
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.calls_path())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn lines(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.dir.path().join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn refuse_pushes(&self, count: u32) {
        fs::write(self.dir.path().join("refuse"), count.to_string()).expect("write refuse count");
    }

    fn fail_gate(&self, verb: &str) {
        fs::write(self.dir.path().join(format!("fail-{verb}")), "").expect("write fail marker");
    }

    fn deny_push(&self) {
        fs::write(self.dir.path().join("fail-push"), "").expect("write push denial marker");
    }

    fn candidate(&self, files: &[(&str, &str)], message: &str) -> String {
        let work = self.work();
        git(&work, &["checkout", "-q", "-B", "card", "origin/main"]);
        for (name, body) in files {
            fs::write(work.join(name), body).expect("write file");
            git(&work, &["add", "--", name]);
        }
        git(&work, &["commit", "-q", "-m", message]);
        let sha = rev_parse(&work, "HEAD");
        git(&work, &["checkout", "-q", "main"]);
        sha
    }

    fn advance_remote_main(&self, name: &str) {
        let other = self.dir.path().join("other");
        git(
            self.dir.path(),
            &["clone", "-q", "-b", "main", "remote.git", "other"],
        );
        fs::write(other.join(name), "x").expect("write file");
        git(&other, &["add", "."]);
        git(&other, &["commit", "-q", "-m", "main moved on"]);
        git(&other, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    }

    fn land(&self, commit: &str) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.path().join("bin").display(),
            std::env::var("PATH").expect("PATH")
        );
        Command::new("bash")
            .arg(self.work().join(".dev/land.sh"))
            .args([commit, "--card", CARD])
            .env("PATH", path)
            .env("TMPDIR", self.dir.path())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("run land")
    }

    fn jj_repo(&self, workspaces: &[(&str, &Path)], bookmarks: &[&str]) {
        let root = self.dir.path();
        fs::create_dir(self.work().join(".jj")).expect("jj dir");
        let rows: String = workspaces
            .iter()
            .map(|(name, path)| format!("{name} {}\n", path.display()))
            .collect();
        fs::write(root.join("workspaces"), rows).expect("write workspaces");
        let names: String = bookmarks.iter().map(|name| format!("{name}\n")).collect();
        fs::write(root.join("bookmarks"), names).expect("write bookmarks");
    }

    fn card_worktree(&self, name: &str, branch: &str) -> PathBuf {
        let path = self.work().join(".dev/worktrees").join(name);
        let arg = path.to_str().expect("utf8");
        git(&self.work(), &["worktree", "add", "-q", arg, branch]);
        path
    }

    fn remote(&self) -> PathBuf {
        self.dir.path().join("remote.git")
    }

    fn remote_main(&self) -> String {
        rev_parse(&self.remote(), "refs/heads/main")
    }
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
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

fn show(dir: &Path, spec: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["show", spec])
        .output()
        .expect("run git show");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn branch_exists(dir: &Path, name: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "-q", &format!("refs/heads/{name}")])
        .status()
        .expect("run git rev-parse")
        .success()
}

fn refused_candidate(refusals: u32) -> (Fixture, String) {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    fixture.refuse_pushes(refusals);
    (fixture, cand)
}

#[test]
fn land_should_push_the_commit_onto_current_main_after_lint_and_test_pass() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    fixture.advance_remote_main("other.txt");
    let moved = fixture.remote_main();
    let out = fixture.land(&cand);
    assert!(out.status.success(), "{}", stderr(&out));
    let remote = fixture.remote();
    assert_eq!(rev_parse(&remote, "refs/heads/main^"), moved);
    assert_eq!(show(&remote, "refs/heads/main:feature.txt"), "feature");
    assert_eq!(show(&remote, "refs/heads/main:other.txt"), "x");
    assert_eq!(fixture.calls(), ["lint", "test"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("Landed {}.", fixture.remote_main())),
        "{stdout}"
    );
    assert!(
        !fixture
            .work()
            .join(format!(".dev/worktrees/land-{CARD}"))
            .exists()
    );
}

#[test]
fn land_should_run_lint_before_test_and_stop_at_the_first_failure() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    fixture.fail_gate("lint");
    let before = fixture.remote_main();
    let out = fixture.land(&cand);
    assert!(!out.status.success());
    assert_eq!(fixture.calls(), ["lint"]);
    assert_eq!(fixture.remote_main(), before);
}

#[test]
fn land_should_refuse_a_commit_with_an_attribution_trailer_before_any_gate() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(
        &[("feature.txt", "feature")],
        "add feature\n\nClaude-Session: abc123",
    );
    let before = fixture.remote_main();
    let out = fixture.land(&cand);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Claude-Session"), "{}", stderr(&out));
    assert!(fixture.calls().is_empty());
    assert_eq!(fixture.remote_main(), before);
}

#[test]
fn land_should_refuse_a_commit_that_bumps_the_package_version() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(
        &[
            ("Cargo.toml", &manifest("1.0.1")),
            ("Cargo.lock", &lock("1.0.1")),
        ],
        "bump the version",
    );
    let before = fixture.remote_main();
    let out = fixture.land(&cand);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("changes the package version"),
        "{}",
        stderr(&out)
    );
    assert!(fixture.calls().is_empty());
    assert_eq!(fixture.remote_main(), before);
}

#[test]
fn land_should_not_push_when_a_gate_fails() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    fixture.fail_gate("test");
    let before = fixture.remote_main();
    let out = fixture.land(&cand);
    assert!(!out.status.success());
    assert_eq!(fixture.calls(), ["lint", "test"]);
    assert_eq!(fixture.remote_main(), before);
    assert!(fixture.lines("notes").is_empty());
}

#[test]
fn land_should_refuse_a_commit_that_is_more_than_one_commit_past_main() {
    let fixture = Fixture::new();
    fixture.candidate(&[("first.txt", "first")], "add first");
    let work = fixture.work();
    git(&work, &["checkout", "-q", "card"]);
    fs::write(work.join("second.txt"), "second").expect("write file");
    git(&work, &["add", "--", "second.txt"]);
    git(&work, &["commit", "-q", "-m", "add second"]);
    let cand = rev_parse(&work, "HEAD");
    git(&work, &["checkout", "-q", "main"]);
    let before = fixture.remote_main();
    let out = fixture.land(&cand);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("expected exactly one"),
        "{}",
        stderr(&out)
    );
    assert!(fixture.calls().is_empty());
    assert_eq!(fixture.remote_main(), before);
}

#[test]
fn land_should_reapply_and_rerun_both_gates_when_the_first_push_is_refused() {
    let (fixture, cand) = refused_candidate(1);
    let out = fixture.land(&cand);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(fixture.calls(), ["lint", "test", "lint", "test"]);
    assert_eq!(fixture.lines("pushes").len(), 2);
    let remote = fixture.remote();
    assert_eq!(show(&remote, "refs/heads/main:feature.txt"), "feature");
    assert!(show(&remote, "refs/heads/main^").contains("moved"));
    assert!(
        !fixture
            .work()
            .join(format!(".dev/worktrees/land-{CARD}"))
            .exists()
    );
}

#[test]
fn land_should_give_up_after_three_refused_pushes_without_forcing() {
    let (fixture, cand) = refused_candidate(3);
    let out = fixture.land(&cand);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("moved on 3 times"),
        "{}",
        stderr(&out)
    );
    assert_eq!(fixture.calls().len(), 6);
    assert_eq!(fixture.lines("pushes").len(), 3);
    assert!(fixture.lines("notes").is_empty());
    let tree = Command::new("git")
        .arg("-C")
        .arg(fixture.remote())
        .args(["ls-tree", "--name-only", "refs/heads/main"])
        .output()
        .expect("run git ls-tree");
    assert!(!String::from_utf8_lossy(&tree.stdout).contains("feature.txt"));
}

#[test]
fn land_should_never_pass_a_force_flag_to_the_push() {
    let (fixture, cand) = refused_candidate(2);
    let out = fixture.land(&cand);
    assert!(out.status.success(), "{}", stderr(&out));
    let pushes = fixture.lines("pushes");
    assert_eq!(pushes.len(), 3);
    for push in pushes {
        let args: Vec<&str> = push.split_whitespace().collect();
        assert!(
            args.iter()
                .all(|arg| !arg.starts_with('+') && !arg.contains("force") && *arg != "-f"),
            "{push}"
        );
    }
}

#[test]
fn land_should_note_the_landed_sha_on_the_card() {
    let (fixture, cand) = refused_candidate(1);
    let out = fixture.land(&cand);
    assert!(out.status.success(), "{}", stderr(&out));
    let notes = fixture.lines("notes");
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[0].starts_with(&format!(
            "board note {CARD} landed {} ",
            fixture.remote_main()
        )),
        "{notes:?}"
    );
    assert_eq!(notes[1], format!("board move {CARD} {RELEASE}"));
}

#[test]
fn land_should_remove_the_card_worktree_and_delete_its_branch() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    let worktree = fixture.card_worktree("feat", "card");
    let out = fixture.land(&cand);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!worktree.exists());
    assert!(!branch_exists(&fixture.work(), "card"));
    assert_eq!(fixture.calls(), ["lint", "test"]);
    assert_eq!(
        fixture.lines("notes")[1],
        format!("board move {CARD} {RELEASE}")
    );
    assert!(fixture.lines("jjcalls").is_empty());
}

#[test]
fn land_should_forget_the_jj_workspace_and_delete_the_bookmark_of_the_card_worktree() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    let worktree = fixture.card_worktree("feat", "card");
    fixture.jj_repo(
        &[
            ("default", fixture.work().as_path()),
            ("feat", worktree.as_path()),
        ],
        &["main", "card"],
    );
    let out = fixture.land(&cand);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!worktree.exists());
    assert!(!branch_exists(&fixture.work(), "card"));
    let jj = fixture.lines("jjcalls");
    assert!(
        jj.contains(&"--no-pager workspace forget feat".to_owned()),
        "{jj:?}"
    );
    assert!(
        !jj.iter().any(|call| call.contains("forget default")),
        "{jj:?}"
    );
    assert!(
        jj.contains(&"--no-pager bookmark delete card".to_owned()),
        "{jj:?}"
    );
    assert!(
        !jj.iter().any(|call| call.contains("bookmark delete main")),
        "{jj:?}"
    );
}

#[test]
fn land_should_keep_the_worktrees_it_did_not_land_from() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    let work = fixture.work();
    git(&work, &["branch", "-q", "other", "origin/main"]);
    git(&work, &["branch", "-q", "outside", "card"]);
    let landed = fixture.card_worktree("feat", "card");
    let other = fixture.card_worktree("other", "other");
    let detached = work.join(".dev/worktrees/detached");
    git(
        &work,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().expect("utf8"),
            cand.as_str(),
        ],
    );
    let outside = fixture.dir.path().join("outside");
    git(
        &work,
        &[
            "worktree",
            "add",
            "-q",
            outside.to_str().expect("utf8"),
            "outside",
        ],
    );
    let out = fixture.land(&cand);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!landed.exists());
    assert!(!branch_exists(&work, "card"));
    assert!(other.exists());
    assert!(branch_exists(&work, "other"));
    assert!(detached.exists());
    assert!(outside.exists());
    assert!(branch_exists(&work, "outside"));
}

#[test]
fn land_should_keep_a_card_worktree_with_uncommitted_changes() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    let worktree = fixture.card_worktree("feat", "card");
    fs::write(worktree.join("scratch.txt"), "wip").expect("write file");
    let out = fixture.land(&cand);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(worktree.exists());
    assert!(branch_exists(&fixture.work(), "card"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("uncommitted changes"), "{stdout}");
}

#[test]
fn land_should_leave_the_card_worktree_and_column_alone_when_the_push_is_denied() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    let worktree = fixture.card_worktree("feat", "card");
    fixture.deny_push();
    let out = fixture.land(&cand);
    assert!(!out.status.success());
    assert!(fixture.lines("notes").is_empty());
    assert!(worktree.exists());
    assert!(branch_exists(&fixture.work(), "card"));
}

#[test]
fn land_should_not_retry_on_an_exit_one_push_failure() {
    let fixture = Fixture::new();
    let cand = fixture.candidate(&[("feature.txt", "feature")], "add feature");
    fixture.deny_push();
    let before = fixture.remote_main();
    let out = fixture.land(&cand);
    assert!(!out.status.success());
    assert_eq!(fixture.calls(), ["lint", "test"]);
    assert_eq!(fixture.lines("pushes").len(), 1);
    assert!(fixture.lines("notes").is_empty());
    assert_eq!(fixture.remote_main(), before);
}
