//! KF-507: a start that cannot bind its port leaves the database file
//! byte-identical.
//! The test holds a port, starts the binary on it against a version 1 file,
//! and checks the exit status and that the file was not stamped or repaired.

use std::net::TcpListener;
use std::process::{Command, Stdio};

use redb::{Database, TableDefinition};

const META: TableDefinition<'_, &str, u64> = TableDefinition::new("meta");

#[test]
fn busy_port_should_exit_nonzero_and_leave_a_version_1_file_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("v1.redb");
    {
        let db = Database::create(&path).expect("create");
        let txn = db.begin_write().expect("write");
        txn.open_table(META)
            .expect("meta")
            .insert("schema_version", 1u64)
            .expect("stamp");
        txn.commit().expect("commit");
    }
    let before = std::fs::read(&path).expect("read before");

    let busy = TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = busy.local_addr().expect("addr").port();
    let status = Command::new(env!("CARGO_BIN_EXE_chipflow"))
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .arg("--database-path")
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run chipflow");

    assert!(!status.success(), "start on a busy port must fail");
    assert_eq!(
        std::fs::read(&path).expect("read after"),
        before,
        "file changed"
    );
}
