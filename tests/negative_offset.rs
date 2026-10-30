use std::fs;
use std::process::Command;

fn run(root: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn negative_offsets_select_from_end_and_continue_forward() {
    let dir = tempfile::tempdir().unwrap();
    let source = (1..=30)
        .map(|i| format!("line{i:02}\n"))
        .collect::<String>();
    fs::write(dir.path().join("data.txt"), source).unwrap();
    for args in [
        vec![
            "read", "data.txt", "--full", "--offset", "-20", "--limit", "10",
        ],
        vec![
            "--offset=-20",
            "--limit",
            "10",
            "read",
            "data.txt",
            "--full",
        ],
        vec!["data.txt", "--full", "--offset", "-20", "--limit", "10"],
    ] {
        let out = run(dir.path(), &args);
        assert!(out.contains("line11") && out.contains("line20"), "{out}");
        assert!(
            !out.contains("line10\n") && !out.contains("line21\n"),
            "{out}"
        );
        assert!(out.contains("--offset 20"), "{out}");
    }
    let out = run(
        dir.path(),
        &[
            "read", "data.txt", "--full", "--offset", "-999", "--limit", "2",
        ],
    );
    assert!(out.contains("line01") && out.contains("line02"), "{out}");
}

#[test]
fn negative_search_offsets_use_the_complete_ranked_set() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("data.txt"),
        (1..=30)
            .map(|i| format!("needle {i:02}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let out = run(
        dir.path(),
        &[
            "search", "needle", "--kind", "content", "--offset", "-20", "--limit", "10",
        ],
    );
    assert!(out.contains("11-20 of 30"), "{out}");
    assert!(out.contains("--offset 20"), "{out}");
}

#[test]
fn mcp_negative_offsets_apply_independently_to_batch_paths() {
    use std::io::Write;
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    for (name, count) in [("a.txt", 30), ("b.txt", 15)] {
        fs::write(
            dir.path().join(name),
            (1..=count)
                .map(|i| format!("line{i:02}\n"))
                .collect::<String>(),
        )
        .unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(dir.path())
        .args(["--mcp", "--scope", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let request = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
        "name":"tilth_read","arguments":{"root":dir.path(),"paths":["a.txt","b.txt"],"mode":"full","offset":-5,"limit":2}
    }});
    writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
    let output = child.wait_with_output().unwrap();
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("26-27 of 30") && text.contains("11-12 of 15"),
        "{text}"
    );
    assert!(
        text.contains(r#""offset":27"#) && text.contains(r#""offset":12"#),
        "{text}"
    );
}
