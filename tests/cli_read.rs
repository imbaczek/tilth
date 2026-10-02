use std::fs;
use std::process::{Command, Output};

fn read(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .arg("read")
        .args(args)
        .output()
        .unwrap()
}
fn text(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[test]
fn explicit_read_handles_extensionless_files_and_does_not_search_missing_paths() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("notes"), "unique text\n").unwrap();
    fs::write(root.path().join("other.rs"), "fn missing() {}\n").unwrap();
    assert!(text(&read(root.path(), &["notes"])).contains("unique text"));
    let missing = read(root.path(), &["missing"]);
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
}

#[test]
fn modes_and_hash_anchors_match_mcp_views() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("sample.rs"),
        "// ordinary comment\n/// Documentation\nfn sample() {\n    dbg!(123);\n    let value = 123;\n}\n").unwrap();
    let signature = text(&read(root.path(), &["sample.rs", "--mode", "signature"]));
    assert!(signature.contains("fn sample()"), "{signature}");
    assert!(!signature.contains("let value"), "{signature}");
    let stripped = text(&read(root.path(), &["sample.rs", "--mode", "stripped"]));
    assert!(stripped.contains("Documentation"), "{stripped}");
    assert!(stripped.contains("let value"), "{stripped}");
    assert!(!stripped.contains("ordinary comment"), "{stripped}");
    assert!(!stripped.contains("dbg!"), "{stripped}");
    let full = text(&read(root.path(), &["sample.rs", "--full", "--edit"]));
    assert!(full.contains("ordinary comment"), "{full}");
    assert!(full.contains("1:"), "{full}");
}

#[test]
fn repeated_sections_preserve_order_and_reject_ambiguous_combinations() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("notes.md"), "one\ntwo\nthree\nfour\n").unwrap();
    let output = text(&read(
        root.path(),
        &["notes.md", "--section", "4-4", "--section", "1-1"],
    ));
    assert!(
        output.find("four").unwrap() < output.find("one").unwrap(),
        "{output}"
    );
    assert!(!output.contains("three"), "{output}");
    for args in [
        vec!["notes.md", "--section", "1-1", "--mode", "signature"],
        vec!["notes.md", "notes.md", "--section", "1-1"],
        vec!["notes.md", "--mode", "invalid"],
    ] {
        assert!(!read(root.path(), &args).status.success(), "{args:?}");
    }
}

#[test]
fn batches_report_file_errors_and_obey_shared_budget() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("one.txt"), "alpha\n".repeat(200)).unwrap();
    fs::write(root.path().join("two.txt"), "beta\n".repeat(200)).unwrap();
    let output = text(&read(root.path(), &["one.txt", "missing.txt", "two.txt"]));
    assert!(
        output.contains("alpha") && output.contains("beta") && output.contains("error:"),
        "{output}"
    );
    let bounded = text(&read(
        root.path(),
        &["one.txt", "two.txt", "--full", "--budget", "100"],
    ));
    assert!(bounded.len() < 1000, "{} bytes", bounded.len());
}

#[test]
fn root_anchors_paths_and_markdown_headings_work() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    fs::write(
        other.path().join("guide.md"),
        "# First\nalpha\n# Second\nbeta\n",
    )
    .unwrap();
    let output = text(&read(
        root.path(),
        &[
            "guide.md",
            "--root",
            other.path().to_str().unwrap(),
            "--section",
            "# Second",
        ],
    ));
    assert!(
        output.contains("beta") && !output.contains("alpha"),
        "{output}"
    );
}

#[test]
fn section_before_read_is_applied_and_combines_with_repeated_sections() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("notes.md"), "one\ntwo\nthree\nfour\n").unwrap();
    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_tilth"))
            .current_dir(root.path())
            .args(["--section", "1-1", "read", "notes.md"])
            .args(extra)
            .output()
            .unwrap()
    };
    let single = text(&run(&[]));
    assert!(
        single.contains("one") && !single.contains("four"),
        "{single}"
    );
    let combined = text(&run(&["--section", "4-4"]));
    assert!(
        combined.find("one").unwrap() < combined.find("four").unwrap(),
        "{combined}"
    );
    assert!(!combined.contains("three"), "{combined}");
    assert!(!run(&["notes.md"]).status.success());
    assert!(!run(&["--mode", "signature"]).status.success());
}
