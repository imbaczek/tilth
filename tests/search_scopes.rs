use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("src/nested")).unwrap();
    for name in ["alpha.rs", "beta.rs", "nested/alpha.rs"] {
        fs::write(
            root.path().join("src").join(name),
            "fn needle() {}\nfn caller() { needle(); }\n// retention slot-wal\n",
        )
        .unwrap();
    }
    fs::write(root.path().join("src/other.py"), "def needle(): pass\n").unwrap();
    root
}

fn cli(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}
fn stdout(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn mcp(root: &std::path::Path, arguments: Value) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(["--mcp", "--no-overview"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"tilth_search","arguments":arguments}}),
    ] {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|reply| reply["id"] == 2)
        .unwrap()["result"]
        .clone()
}

#[test]
fn cli_file_scope_searches_exact_file_for_all_query_shapes() {
    let root = fixture();
    for query in [
        "needle",
        "retention,slot-wal",
        "retention slot-wal",
        "/needle/",
    ] {
        let out = stdout(cli(
            root.path(),
            &[query, "--scope", "src/alpha.rs", "--expand=0"],
        ));
        assert!(out.contains("alpha.rs:"), "{query}: {out}");
        assert!(
            !out.contains("beta.rs") && !out.contains("nested/"),
            "{out}"
        );
        assert!(!out.contains("### :"), "{out}");
    }
    let out = stdout(cli(
        root.path(),
        &["needle", "--scope", "src/alpha.rs", "--callers"],
    ));
    assert!(out.contains("## alpha.rs:2 [caller: caller]"), "{out}");
    for query in ["alpha.rs", "src/alpha.rs"] {
        let out = stdout(cli(root.path(), &[query, "--scope", "src/alpha.rs"]));
        assert!(out.contains("fn needle()"), "{out}");
    }
}

#[test]
fn qualified_query_globs_match_only_the_selected_file() {
    let root = fixture();
    for query in ["alpha*.rs", "src/alpha*.rs"] {
        let out = stdout(cli(root.path(), &[query, "--scope", "src/alpha.rs"]));
        assert!(out.contains("alpha.rs"), "{out}");
        assert!(
            !out.contains("nested/") && !out.contains("beta.rs"),
            "{out}"
        );
        assert!(!out.contains("— 0 files"), "{out}");
    }
    let out = stdout(cli(
        root.path(),
        &["wrong/alpha*.rs", "--scope", "src/alpha.rs"],
    ));
    assert!(out.contains("— 0 files"), "{out}");
}

#[test]
fn file_scope_and_additional_glob_intersect() {
    let root = fixture();
    for filter in ["*.rs", "alpha.rs", "!*.py"] {
        let out = stdout(cli(
            root.path(),
            &["needle", "--scope", "src/alpha.rs", "--glob", filter],
        ));
        assert!(out.contains("[definition]"), "{filter}: {out}");
    }
    for filter in ["*.py", "!alpha.rs"] {
        let output = cli(
            root.path(),
            &["needle", "--scope", "src/alpha.rs", "--glob", filter],
        );
        assert!(output.status.success() || output.status.code() == Some(2));
        let out = String::from_utf8(output.stdout).unwrap();
        assert!(!out.contains("[definition]"), "{filter}: {out}");
    }
}

#[test]
fn glob_scopes_are_stable_deduplicated_and_do_not_widen() {
    let root = fixture();
    let args = [
        "needle",
        "--scope",
        "src/*.rs",
        "--scope",
        "src/alpha.rs",
        "--expand=0",
    ];
    let out = stdout(cli(root.path(), &args));
    assert_eq!(out, stdout(cli(root.path(), &args)));
    assert!(out.contains("alpha.rs") && out.contains("beta.rs"), "{out}");
    assert!(
        !out.contains("nested/") && !out.contains("other.py"),
        "{out}"
    );
    assert_eq!(out.matches("[definition]").count(), 2, "{out}");
    let out = stdout(cli(
        root.path(),
        &["needle", "--scope", "src/*", "--expand=0"],
    ));
    assert!(out.contains("nested/alpha.rs"), "directory glob: {out}");
    let out = stdout(cli(
        root.path(),
        &[
            "needle",
            "--scope",
            "src/*.rs",
            "--glob",
            "!beta.rs",
            "--expand=0",
        ],
    ));
    assert_eq!(out.matches("[definition]").count(), 1, "{out}");
    let out = stdout(cli(
        root.path(),
        &[
            "needle",
            "--scope",
            "src/alpha.rs",
            "--scope",
            "src/missing*.rs",
        ],
    ));
    assert!(
        out.contains("[definition]") && out.contains("scope"),
        "{out}"
    );
    let out = stdout(cli(
        root.path(),
        &[
            "needle",
            "--scope",
            "src/alpha.rs",
            "--scope",
            "missing/*.rs",
        ],
    ));
    assert!(
        out.contains("[definition]") && out.contains("missing"),
        "{out}"
    );
    let reply = mcp(
        root.path(),
        json!({"query":"needle", "root":root.path(), "scopes":["src/alpha.rs", "missing/*.rs"]}),
    );
    assert_ne!(reply["isError"], true, "{reply}");
    assert!(reply["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("[definition]"));
    let missing = cli(root.path(), &["needle", "--scope", "src/missing*.rs"]);
    assert_eq!(missing.status.code(), Some(2));
}

#[test]
fn missing_filename_suggestions_are_ranked_thresholded_and_budgeted() {
    let root = fixture();
    for path in ["src/alph.rs", "alph.rs"] {
        if !path.contains('/') {
            fs::write(root.path().join("alpha.rs"), "fn needle() {}\n").unwrap();
        }
        let output = cli(root.path(), &["needle", "--scope", path, "--budget", "100"]);
        assert_eq!(output.status.code(), Some(2));
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("alpha.rs"), "{error}");
        assert!(!error.contains("beta.rs"), "{error}");
        assert!(error.len().div_ceil(4) <= 100, "{error}");
    }
    let output = cli(root.path(), &["needle", "--scope", "src/unrelated.xyz"]);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("did you mean files"));
}

#[test]
fn mcp_file_glob_paging_and_callers_scopes_work() {
    let root = fixture();
    for kind in ["symbol", "content", "regex", "callers"] {
        let reply = mcp(
            root.path(),
            json!({"query":"needle","root":root.path(),"scope":"src/alpha.rs","kind":kind,"offset":0,"limit":1}),
        );
        assert_ne!(reply["isError"], true, "{reply}");
        let text = reply["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("alpha.rs:"), "{kind}: {text}");
        assert!(
            !text.contains("beta.rs") && !text.contains("### :"),
            "{text}"
        );
    }
    for kind in ["symbol", "callers"] {
        let reply = mcp(
            root.path(),
            json!({"query":"needle","root":root.path(),"scope":"src/*.rs","kind":kind}),
        );
        assert_ne!(reply["isError"], true, "{reply}");
        let text = reply["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("alpha.rs") && text.contains("beta.rs"),
            "{text}"
        );
        assert!(!text.contains("other.py"), "{text}");
    }
    let reply = mcp(
        root.path(),
        json!({"query":"needle","root":root.path(),"scope":"src/alph.rs","budget":100}),
    );
    assert_eq!(reply["isError"], true, "{reply}");
    let text = reply["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("alpha.rs"), "{text}");
    assert!(text.len().div_ceil(4) <= 100, "{text}");
}

#[test]
fn glob_scopes_honor_ignore_rules() {
    let root = fixture();
    fs::write(root.path().join(".tilthignore"), "src/beta.rs\n").unwrap();
    let out = stdout(cli(
        root.path(),
        &["needle", "--scope", "src/*.rs", "--expand=0"],
    ));
    assert!(
        out.contains("alpha.rs") && !out.contains("beta.rs"),
        "{out}"
    );
}

#[test]
fn recursive_globs_and_explicit_files_respect_gitignore_setting() {
    let root = fixture();
    fs::write(root.path().join(".gitignore"), "src/beta.rs\n").unwrap();
    for scope in ["src/**/*.rs", "src/beta.rs"] {
        let output = cli(root.path(), &["needle", "--scope", scope, "--expand=0"]);
        let out = String::from_utf8(output.stdout).unwrap();
        assert!(!out.contains("beta.rs:"), "{out}");
        let out = stdout(cli(
            root.path(),
            &[
                "needle",
                "--scope",
                scope,
                "--no-respect-gitignore",
                "--expand=0",
            ],
        ));
        assert!(out.contains("beta.rs:"), "{out}");
    }
}

#[cfg(unix)]
#[test]
fn symlink_file_scope_searches_target_only() {
    let root = fixture();
    std::os::unix::fs::symlink(
        root.path().join("src/alpha.rs"),
        root.path().join("link.rs"),
    )
    .unwrap();
    let out = stdout(cli(
        root.path(),
        &["needle", "--scope", "link.rs", "--expand=0"],
    ));
    assert_eq!(out.matches("[definition]").count(), 1, "{out}");
    assert!(out.contains("alpha.rs:"), "{out}");
}

#[test]
fn related_missing_file_is_suggested_without_searching_it() {
    let root = fixture();
    fs::write(root.path().join("src/wal.lisp"), "(defun needle () nil)\n").unwrap();
    fs::write(root.path().join("src/other.lisp"), "").unwrap();
    let output = cli(
        root.path(),
        &[
            "needle",
            "--scope",
            "src/wal-checkpoint.lisp",
            "--budget",
            "100",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(
        text.contains("files (lower confidence)") && text.contains("wal.lisp"),
        "{text}"
    );
    assert!(!text.contains("other.lisp"), "{text}");
    assert!(text.len().div_ceil(4) <= 100, "{text}");
    let reply = mcp(
        root.path(),
        json!({"query":"needle", "root":root.path(), "scope":"src/wal-checkpoint.lisp", "budget":100}),
    );
    assert_eq!(reply["isError"], true, "{reply}");
    let text = reply["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("files (lower confidence)") && text.contains("wal.lisp"),
        "{text}"
    );
    assert!(!text.contains("[definition]"), "{text}");
    assert!(text.len().div_ceil(4) <= 100, "{text}");
}
