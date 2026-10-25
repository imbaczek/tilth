use std::process::Command;
fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}
#[test]
fn repeated_queries_projections_and_parent_options() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn alpha() {}\nfn beta() { alpha(); }\n// alpha,beta\n",
    )
    .unwrap();
    for args in [
        vec![
            "search",
            "--query",
            "alpha",
            "--query",
            "beta",
            "--select",
            "definitions",
        ],
        vec![
            "--select",
            "definitions",
            "search",
            "--query",
            "alpha",
            "--query",
            "beta",
        ],
    ] {
        let out = run(dir.path(), &args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = String::from_utf8(out.stdout).unwrap();
        assert!(
            out.contains("fn alpha()") && out.contains("fn beta()"),
            "{out}"
        );
    }
    let out = run(
        dir.path(),
        &[
            "search",
            "-q",
            "alpha,beta",
            "--kind",
            "content",
            "--select",
            "count",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8(out.stdout).unwrap().contains("1 matches"));
    for args in [
        vec!["search", "alpha", "--query", "beta"],
        vec!["--select", "count", "search", "alpha", "--callers"],
    ] {
        assert!(!run(dir.path(), &args).status.success(), "{args:?}");
    }
}
#[test]
fn projection_pages_filter_before_offset_and_keep_partial_scope_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn needle() {}\nfn caller() { needle(); }\n",
    )
    .unwrap();
    let out = run(
        dir.path(),
        &[
            "needle",
            "--select",
            "definitions",
            "--offset",
            "1",
            "--limit",
            "1",
        ],
    );
    assert!(out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(out.contains("definitions 0 of 1"), "{out}");
    let out = run(
        dir.path(),
        &[
            "needle", "--select", "count", "--scope", "a.rs", "--scope", "missing",
        ],
    );
    assert!(out.status.success());
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(
        out.contains("missing") && out.contains("2 matches"),
        "{out}"
    );
}

#[test]
fn cli_json_and_navigation_are_preserved() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn needle() {}\nfn caller() { needle(); }\n",
    )
    .unwrap();
    let out = run(dir.path(), &["-q", "needle", "--json", "--limit", "1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["output"]
        .as_str()
        .unwrap()
        .contains("Next page: --offset 1"));
    assert!(!run(dir.path(), &["-q", "needle", "--expand=1"])
        .status
        .success());
}

#[test]
fn file_scopes_and_same_line_signatures_keep_identity() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn foo() {} fn bar() {}\n").unwrap();
    for select in ["files", "definitions", "signatures"] {
        let out = run(dir.path(), &["bar", "--scope", "a.rs", "--select", select]);
        assert!(out.status.success());
        let out = String::from_utf8(out.stdout).unwrap();
        assert!(out.contains("a.rs"), "{out}");
        if select == "signatures" {
            assert!(
                out.contains("fn bar()") && !out.contains("fn foo()"),
                "{out}"
            );
        }
    }
    let out = run(dir.path(), &["bar", "--select", "count", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["query"], "bar");
}
