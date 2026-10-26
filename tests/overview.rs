use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for path in [
        "src/lisp/deep/core.lisp",
        "src/lisp/deep/query.lisp",
        "src/c/a.c",
        "src/c/b.c",
        "src/c/c.c",
        "node_modules/pkg/bad.rs",
        "git-ignored/bad.rs",
        "custom-ignored/bad.rs",
        "ignore-ignored/bad.rs",
    ] {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }
    fs::write(dir.path().join(".gitignore"), "git-ignored/\n").unwrap();
    fs::write(dir.path().join(".tilthignore"), "custom-ignored/\n").unwrap();
    fs::write(dir.path().join(".ignore"), "ignore-ignored/\n").unwrap();
    dir
}

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .env_remove("TILTH_RESPECT_GITIGNORE")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn overview_scopes_recursion_and_ignores() {
    let dir = fixture();
    let scope = dir.path().to_str().unwrap();
    let positional = run(dir.path(), &["overview", scope, "-r"]);
    let flagged = run(dir.path(), &["overview", "--scope", scope, "--recursive"]);
    assert!(
        positional.status.success(),
        "{}",
        String::from_utf8_lossy(&positional.stderr)
    );
    assert_eq!(positional.stdout, flagged.stdout);
    let before_command = run(dir.path(), &["--scope", scope, "overview", "-r"]);
    assert_eq!(flagged.stdout, before_command.stdout);
    assert!(!run(dir.path(), &["--scope", scope, "overview", scope])
        .status
        .success());
    assert!(!run(dir.path(), &["--scope", "missing", "overview"])
        .status
        .success());
    let output = String::from_utf8(positional.stdout).unwrap();
    assert!(output.contains("src/lisp/"), "{output}");
    assert!(output.contains("2 source files"), "{output}");
    assert!(!output.contains("Rust"), "{output}");
    let shallow = run(dir.path(), &["overview", scope]);
    assert!(!String::from_utf8_lossy(&shallow.stdout).contains("src/lisp/"));
    assert!(!run(dir.path(), &["overview", scope, "--scope", scope])
        .status
        .success());
    assert!(!run(dir.path(), &["overview", "missing"]).status.success());
    let multiple = run(dir.path(), &["overview", "src/c", "src/lisp", "-r"]);
    assert!(multiple.status.success());
    assert!(String::from_utf8_lossy(&multiple.stdout).contains("## src/lisp"));
}

#[test]
fn mcp_initialization_is_recursive() {
    let dir = fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(dir.path())
        .env_remove("TILTH_NO_OVERVIEW")
        .env_remove("TILTH_RESPECT_GITIGNORE")
        .arg("--mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(
        input,
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{}}}}"
    )
    .unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    let instructions = response["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("src/lisp/"), "{instructions}");
    assert!(!instructions.contains("Rust"), "{instructions}");
}

#[cfg(unix)]
#[test]
fn overview_does_not_follow_directory_symlinks() {
    let dir = fixture();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("bad.rs"), "").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("external")).unwrap();
    let result = run(dir.path(), &["overview", "-r"]);
    assert!(result.status.success());
    assert!(!String::from_utf8_lossy(&result.stdout).contains("Rust"));
}

fn displayed_directories(output: &str) -> Vec<&str> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("  dirs: "))
        .map(|line| line.split_whitespace().collect())
        .unwrap_or_default()
}

#[test]
fn overview_pages_directories_shallowest_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut expected = vec!["./".to_owned()];
    for i in 0..11 {
        expected.push(format!("z{i:02}/"));
    }
    expected.extend(["a/deep/".into(), "b/deep/deeper/".into()]);
    for directory in &expected {
        let path = dir.path().join(directory);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("source.c"), "").unwrap();
    }
    let default = run(dir.path(), &["overview", "-r"]);
    assert!(default.status.success());
    let default = String::from_utf8(default.stdout).unwrap();
    assert_eq!(displayed_directories(&default), expected[..10]);
    assert!(default.contains("14 directories"), "{default}");
    assert!(
        default.contains("next page: --offset 10 --limit 10"),
        "{default}"
    );

    let mut all = Vec::new();
    for offset in [0, 5, 10] {
        let result = run(
            dir.path(),
            &[
                "overview",
                "-r",
                "--offset",
                &offset.to_string(),
                "--limit",
                "5",
            ],
        );
        assert!(result.status.success());
        let output = String::from_utf8(result.stdout).unwrap();
        assert!(
            output.contains("14 source files, 14 directories"),
            "{output}"
        );
        all.extend(
            displayed_directories(&output)
                .into_iter()
                .map(str::to_owned),
        );
    }
    assert_eq!(all, expected);
    let empty = run(dir.path(), &["overview", "-r", "--offset", "999"]);
    assert!(empty.status.success());
    let empty = String::from_utf8(empty.stdout).unwrap();
    assert!(displayed_directories(&empty).is_empty());
    assert!(empty.contains("0 shown of 14"), "{empty}");
    assert!(!empty.contains("next page:"));
    assert!(!run(dir.path(), &["overview", "--limit", "0"])
        .status
        .success());

    let before = run(
        dir.path(),
        &["--offset", "10", "--limit", "5", "overview", "-r"],
    );
    let after = run(
        dir.path(),
        &["overview", "-r", "--offset", "10", "--limit", "5"],
    );
    assert_eq!(before.stdout, after.stdout);
    assert!(before.status.success());
}

#[test]
fn overview_pages_share_one_directory_limit_across_languages() {
    let dir = fixture();
    let result = run(dir.path(), &["overview", "-r", "--limit", "1"]);
    assert!(result.status.success());
    let output = String::from_utf8(result.stdout).unwrap();
    assert_eq!(displayed_directories(&output), vec!["src/c/"]);
    assert!(output.contains("Common Lisp: 2 source files"), "{output}");
    assert!(
        output.contains("Common Lisp: 2 source files (under src/lisp/deep/)"),
        "{output}"
    );

    let result = run(
        dir.path(),
        &["overview", "-r", "--offset", "1", "--limit", "1"],
    );
    let output = String::from_utf8(result.stdout).unwrap();
    assert_eq!(displayed_directories(&output), vec!["src/lisp/deep/"]);
    assert!(
        output.contains("Common Lisp: 2 source files (under src/lisp/deep/)"),
        "{output}"
    );
}
