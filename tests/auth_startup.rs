use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use pomb::db::{Db, ModeSwitch};
use pomb::models::InstanceMode;

fn start(args: &[&str]) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    start_on(&dir.path().join("pomb.redb"), args)
}

fn start_on(database: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pomb"))
        .args(args)
        .args(["--port", "0"])
        .arg("--database-path")
        .arg(database)
        .env_remove("POMB_AUTH")
        .env_remove("POMB_MODE")
        .env_remove("POMB_PUBLIC_URL")
        .env_remove("HOST")
        .output()
        .expect("run pomb")
}

#[test]
fn multi_mode_should_refuse_auth_none() {
    let output = start(&["--mode", "multi", "--auth", "none"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mode multi needs --auth passkey"),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("listening"));
}

#[test]
fn multi_to_single_should_be_refused_with_two_accounts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pomb.redb");
    {
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        db.create_user("admin").expect("admin");
        db.create_user("bob").expect("member");
        assert_eq!(
            db.apply_mode(InstanceMode::Multi).expect("multi"),
            ModeSwitch::Switched {
                from: InstanceMode::Single
            }
        );
    }
    let output = start_on(&path, &["--mode", "single", "--auth", "none"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--mode single serves one account, but this instance has 2"),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("listening"));
    let db = Db::connect(path.to_str().expect("utf8 path")).expect("reconnect");
    assert_eq!(
        db.apply_mode(InstanceMode::Multi).expect("multi"),
        ModeSwitch::Kept,
        "the refusal keeps the stored mode"
    );
}

#[test]
fn multi_to_single_should_switch_with_one_account() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pomb.redb");
    {
        let db = Db::connect(path.to_str().expect("utf8 path")).expect("connect");
        db.create_user("admin").expect("admin");
        db.apply_mode(InstanceMode::Multi).expect("multi");
        assert_eq!(
            db.apply_mode(InstanceMode::Single).expect("single"),
            ModeSwitch::Switched {
                from: InstanceMode::Multi
            }
        );
        assert_eq!(
            db.apply_mode(InstanceMode::Single).expect("single"),
            ModeSwitch::Kept
        );
    }
}

#[test]
fn auth_none_should_refuse_to_listen_beyond_loopback() {
    let output = start(&["--auth", "none", "--host", "0.0.0.0"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--auth none needs a loopback --host"),
        "{stderr}"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("listening"));
}

#[test]
fn auth_passkey_should_refuse_to_start_without_a_public_url() {
    let output = start(&["--auth", "passkey", "--host", "127.0.0.1"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("needs POMB_PUBLIC_URL"), "{stderr}");
}

#[test]
fn default_auth_should_refuse_an_all_interfaces_host_from_the_environment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_pomb"))
        .args(["--port", "0", "--database-path"])
        .arg(dir.path().join("pomb.redb"))
        .env_remove("POMB_AUTH")
        .env("HOST", "0.0.0.0")
        .output()
        .expect("run pomb");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("loopback"));
}

#[test]
fn help_should_explain_passkey_recovery() {
    let output = Command::new(env!("CARGO_BIN_EXE_pomb"))
        .arg("--help")
        .output()
        .expect("run pomb");
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("Lost every passkey"), "{help}");
    assert!(help.contains("[env: POMB_AUTH=]"), "{help}");
    assert!(help.contains("[env: POMB_MODE=]"), "{help}");
    assert!(help.contains("--mode multi"), "{help}");
    assert!(help.contains("[default: 127.0.0.1]"), "{help}");
}

#[test]
fn default_auth_should_refuse_a_public_url_beyond_loopback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_pomb"))
        .args(["--port", "0", "--database-path"])
        .arg(dir.path().join("pomb.redb"))
        .env_remove("POMB_AUTH")
        .env_remove("HOST")
        .env("POMB_PUBLIC_URL", "https://pomb.example.com")
        .output()
        .expect("run pomb");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no sign-in mode was chosen"), "{stderr}");
}

#[test]
fn explicit_auth_none_should_serve_a_public_url_beyond_loopback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut child = Command::new(env!("CARGO_BIN_EXE_pomb"))
        .args(["--port", "0", "--database-path"])
        .arg(dir.path().join("pomb.redb"))
        .env("POMB_AUTH", "none")
        .env_remove("HOST")
        .env("POMB_PUBLIC_URL", "https://pomb.example.com")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("run pomb");
    let stdout = child.stdout.take().expect("stdout");
    let listening = BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .any(|line| line.starts_with("pomb listening on"));
    let _ = child.kill();
    let _ = child.wait();
    assert!(listening, "the server never started");
}
