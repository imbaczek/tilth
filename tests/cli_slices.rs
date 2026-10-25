use std::process::Command;
fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .arg("read")
        .args(args)
        .output()
        .unwrap()
}
fn text(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
#[test]
fn slices_merge_overlaps_and_keep_files_in_first_seen_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha1\nalpha2\nalpha3\nalpha4\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta1\nbeta2\n").unwrap();
    let out = text(run(
        dir.path(),
        &[
            "--slice",
            "b.txt:2-2",
            "--slice",
            "a.txt:2-3",
            "--slice",
            "a.txt:1-2",
        ],
    ));
    assert!(
        out.find("beta2").unwrap() < out.find("alpha1").unwrap(),
        "{out}"
    );
    assert_eq!(out.matches("alpha2").count(), 1, "{out}");
    assert!(!out.contains("alpha4") && !out.contains("beta1"), "{out}");
}
#[test]
fn slices_validate_combinations_keep_file_errors_and_hash_anchors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
    let out = text(run(
        dir.path(),
        &["--slice", "a.txt:1-1", "--slice", "missing:1-1", "--edit"],
    ));
    assert!(
        out.contains("1:") && out.contains("error:") && out.contains("alpha"),
        "{out}"
    );
    for args in [
        vec!["--slice", "a.txt:0-1"],
        vec!["--slice", "a.txt:2-1"],
        vec!["--slice", "a.txt"],
        vec!["--slice", "a.txt:1-1", "--section", "1-1"],
        vec!["a.txt", "--slice", "a.txt:1-1"],
        vec!["--slice", "a.txt:1-1", "--symbol", "alpha"],
        vec!["--slice", "a.txt:1-1", "--mode", "signature"],
    ] {
        assert!(!run(dir.path(), &args).status.success(), "{args:?}");
    }
}
#[test]
fn slice_budgets_apply_to_combined_response() {
    let dir = tempfile::tempdir().unwrap();
    for file in ["a.txt", "b.txt"] {
        std::fs::write(
            dir.path().join(file),
            "some fairly long source line\n".repeat(100),
        )
        .unwrap();
    }
    let out = text(run(
        dir.path(),
        &[
            "--slice",
            "a.txt:1-100",
            "--slice",
            "b.txt:1-100",
            "--budget",
            "150",
        ],
    ));
    assert!(out.len() <= 700, "{}", out.len());
    assert!(
        out.contains("omitted") || out.contains("truncated"),
        "{out}"
    );
}
