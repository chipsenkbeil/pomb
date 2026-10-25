use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};

fn start(args: &[&str]) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::new(env!("CARGO_BIN_EXE_pomb"))
        .args(args)
        .args(["--port", "0"])
        .arg("--database-path")
        .arg(dir.path().join("pomb.redb"))
        .env_remove("POMB_AUTH")
        .env_remove("POMB_PUBLIC_URL")
        .env_remove("HOST")
        .output()
        .expect("run pomb")
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
