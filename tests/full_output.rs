//! ENOSPC regression tests without filling a filesystem. /dev/full implements
//! the failing write syscall; unit tests also cover buffered flush failures.
#![cfg(target_os = "linux")]

use std::fs::OpenOptions;
use std::process::{Command, Stdio};

fn full() -> std::fs::File {
    OpenOptions::new().write(true).open("/dev/full").unwrap()
}

#[test]
fn full_stdout_exits_with_io_error_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file.rs");
    std::fs::write(&file, "fn example() {}\n").unwrap();
    let path = file.to_str().unwrap();
    for args in [
        vec![path],
        vec!["read", path],
        vec!["--json", path],
        vec!["--completions", "bash"],
        vec!["overview"],
        vec!["--map"],
        vec!["diff", "--a", path, "--b", path],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .args(&args)
            .stdout(full())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains("output error:"),
            "{args:?}: {diagnostic}"
        );
        assert!(!diagnostic.contains("panicked"), "{diagnostic}");
    }
}

#[test]
fn full_stderr_and_stdout_do_not_abort_error_reporting() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec![],
        vec!["diff"],
        vec!["read", "missing.rs"],
        vec!["--completions", "bash"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .args(&args)
            .current_dir(dir.path())
            .stdout(full())
            .stderr(full())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(if args.is_empty() { 3 } else { 1 }),
            "{args:?}"
        );
    }
}

#[test]
fn mcp_full_stdout_and_stderr_exit_cleanly() {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .args(["--mcp", "--no-overview"])
        .stdin(Stdio::piped())
        .stdout(full())
        .stderr(full())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(1));
}
