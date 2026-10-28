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
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn cli_search_pages_cover_all_kinds_and_preserve_rank_order() {
    let root = tempfile::tempdir().unwrap();
    for i in (0..125).rev() {
        fs::write(
            root.path().join(format!("unit{i:03}.rs")),
            "pub fn Needle() {}\n// unique phrase\n",
        )
        .unwrap();
    }
    for query in ["Needle", "/Needle/", "/Needl[e]/", "unique phrase"] {
        for prefix in [vec![query], vec!["search", query]] {
            let mut pages = Vec::new();
            for offset in (0..125).step_by(30) {
                let offset_text = offset.to_string();
                let mut args = prefix.clone();
                args.extend([
                    "--offset",
                    &offset_text,
                    "--limit",
                    "30",
                    "--budget",
                    "20000",
                ]);
                let output = run(root.path(), &args);
                assert!(output.contains("of 125"), "{output}");
                assert!(!output.contains("ignored"), "{output}");
                pages.push(output);
            }
            let joined = pages.join("\n");
            for i in 0..125 {
                assert_eq!(
                    joined.matches(&format!("unit{i:03}.rs:")).count(),
                    1,
                    "{query}: {i}"
                );
            }
            let mut args = prefix.clone();
            args.extend(["--offset", "0", "--full", "--expand=0", "--budget", "20000"]);
            assert!(run(root.path(), &args).contains("Showing matches 1-100 of 125"));
            let mut args = prefix.clone();
            args.extend(["--offset", "0", "--limit", "125", "--budget", "150"]);
            let tight = run(root.path(), &args);
            assert!(
                !tight.contains("Next page:") && !tight.contains("End of listing."),
                "{tight}"
            );
            assert!(
                tight.contains("retry the same offset with a smaller limit"),
                "{tight}"
            );
        }
    }
}

#[test]
fn cli_concept_cascade_and_multi_symbol_paging_work() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("one.rs"),
        "pub fn needle() {}\npub fn other() {}\n// alpha separated beta\n",
    )
    .unwrap();
    let concept = run(root.path(), &["needle", "--limit", "1"]);
    assert!(
        concept.contains("Showing matches 1-1 of 1") && concept.contains("[definition]"),
        "{concept}"
    );
    let relaxed = run(root.path(), &["alpha beta", "--limit", "1"]);
    assert!(
        relaxed.contains("Showing matches 1-1 of 1") && relaxed.contains("one.rs:"),
        "{relaxed}"
    );
    let multi = run(root.path(), &["needle,other", "--limit", "1"]);
    assert_eq!(
        multi.matches("Showing matches 1-1 of 1").count(),
        2,
        "{multi}"
    );
    assert!(multi.contains("```"), "{multi}");
    let explicit_zero = run(root.path(), &["needle,other", "--limit", "1", "--expand=0"]);
    assert!(explicit_zero.contains("```"), "{explicit_zero}");
    let beyond = run(
        root.path(),
        &[
            "Needle",
            "--offset",
            &usize::MAX.to_string(),
            "--limit",
            "1",
        ],
    );
    assert!(
        beyond.contains("Showing matches 0 of 0") && !beyond.contains("Next page:"),
        "{beyond}"
    );
}

#[test]
fn cli_comma_literals_are_not_reclassified_as_multi_symbol_queries() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("one.rs"),
        "pub fn needle() {}\n// needle, literal\n",
    )
    .unwrap();
    for query in ["needle,", ","] {
        let output = run(root.path(), &[query, "--limit", "1"]);
        assert!(
            output.contains("Showing matches 1-1 of 1"),
            "{query}: {output}"
        );
        assert!(
            output.contains(&format!("# Search: \"{query}\"")),
            "{output}"
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root.path())
        .args(["a,b,c,d,e,f", "--limit", "1"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
