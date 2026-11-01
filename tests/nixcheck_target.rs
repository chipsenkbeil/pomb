use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod stub");
}

fn run_nixcheck(target_dir: Option<&str>, unshare_body: &str) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).expect("bin dir");
    write_script(&bin.join("unshare"), unshare_body);
    write_script(&bin.join("cargo"), "exit 0");
    write_script(&bin.join("cargo-nextest"), "exit 0");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").expect("PATH"));
    let mut command = Command::new("bash");
    command
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/.dev/nixcheck.sh"))
        .env("PATH", path)
        .env_remove("CARGO_TARGET_DIR");
    if let Some(target_dir) = target_dir {
        command.env("CARGO_TARGET_DIR", target_dir);
    }
    command.output().expect("run nixcheck")
}

const SANDBOX_FAILS: &str = r#"if [ "$(eval echo \${$#})" = true ]; then exit 0; fi
exit 5"#;

#[test]
fn nixcheck_should_refuse_a_target_directory_under_var_tmp() {
    let out = run_nixcheck(Some("/var/tmp/pomb-nixcheck-target"), "exit 0");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("under /var/tmp"), "{stderr}");
    assert!(stderr.contains("CARGO_TARGET_DIR"), "{stderr}");
}

#[test]
fn nixcheck_should_refuse_var_tmp_reached_through_dot_dot() {
    let out = run_nixcheck(Some("/var/tmp/../tmp/pomb-nixcheck-target"), "exit 0");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("under /var/tmp"));
}

#[test]
fn nixcheck_should_run_the_sandbox_and_pass_on_its_failure_with_the_default_target_directory() {
    let out = run_nixcheck(None, SANDBOX_FAILS);
    assert_eq!(
        out.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn nixcheck_should_run_the_sandbox_for_a_target_directory_outside_var_tmp() {
    let out = run_nixcheck(Some("/tmp/pomb-nixcheck-target"), SANDBOX_FAILS);
    assert_eq!(
        out.status.code(),
        Some(5),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
