use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("quant/transformers")).unwrap();
    fs::create_dir_all(root.path().join("quant/transport")).unwrap();
    fs::write(
        root.path().join("quant/transformers/hit.rs"),
        "// extrema marker\npub fn extrema() {}\n",
    )
    .unwrap();
    root
}

#[test]
fn cli_suggests_each_failed_scope_and_preserves_valid_results() {
    let root = fixture();
    for query in ["extrema", "extrema marker", "/extrema/", "*.rs", "hit.rs"] {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .current_dir(root.path())
            .args([
                query,
                "--scope",
                "quant/transformer",
                "--scope",
                "quant/transprt",
                "--scope",
                "quant/transformers",
                "--budget",
                "2500",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{query}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            stdout.matches("did you mean directories:").count(),
            2,
            "{query}: {stdout}"
        );
        assert!(stdout.contains("hit.rs"), "{query}: {stdout}");
        assert!(stdout.contains("quant/transformer:"), "{stdout}");
        assert!(stdout.contains("quant/transprt:"), "{stdout}");
    }
}

#[test]
fn cli_all_failed_scopes_exit_two_with_separate_ranked_hints() {
    let root = fixture();
    for extra in [None, Some("--callers"), Some("--deps")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tilth"));
        command.current_dir(root.path()).args([
            "extrema",
            "--scope",
            "quant/transformer",
            "--scope",
            "quant/transprt",
        ]);
        if let Some(flag) = extra {
            command.arg(flag);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            stderr.matches("did you mean directories:").count(),
            2,
            "{stderr}"
        );
        assert!(
            stderr.contains("did you mean directories: \"quant/transformers\""),
            "{stderr}"
        );
        assert!(
            stderr.contains("did you mean directories: \"quant/transport\""),
            "{stderr}"
        );
    }
}

#[test]
fn cli_single_missing_scope_exits_two_and_intermediate_typo_is_repaired() {
    let root = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root.path())
        .args([
            "extrema",
            "--scope",
            "qunat/transformers",
            "--budget",
            "2500",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("did you mean directories: \"quant/transformers\""),
        "{stderr}"
    );
}

#[test]
fn mcp_singular_warning_and_plural_scope_errors_include_hints() {
    let root = fixture();
    let requests = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tilth_search","arguments":{"query":"extrema","root":root.path(),"scope":"quant/transformer","budget":2500}}}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"tilth_search","arguments":{"query":"extrema","root":root.path(),"scopes":["quant/transformer","quant/transprt","quant/transformers"],"budget":2500}}}),
    ];
    let mut child = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root.path())
        .args(["--mcp", "--no-overview"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (id, count) in [(2, 1), (3, 2)] {
        let reply = replies.iter().find(|reply| reply["id"] == id).unwrap();
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            text.matches("did you mean directories:").count(),
            count,
            "{reply}"
        );
        assert!(text.contains("hit.rs"), "{reply}");
    }
}

#[test]
fn map_reports_each_missing_scope_and_parent_relative_hints_are_usable() {
    let root = fixture();
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root.path())
        .args([
            "--map",
            "--scope",
            "quant/transformer",
            "--scope",
            "quant/transprt",
            "--scope",
            "quant/transformers",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.matches("did you mean directories:").count(),
        2,
        "{stdout}"
    );
    assert!(stdout.contains("hit.rs"), "{stdout}");
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root.path().join("quant"))
        .args(["extrema", "--scope", "../qunat/transformers"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("did you mean directories: \"../quant/transformers\""),
        "{stderr}"
    );
}
