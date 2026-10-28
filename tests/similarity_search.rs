use std::path::Path;
use std::process::Command;

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}
fn output(root: &Path, args: &[&str]) -> String {
    let result = run(root, args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn search_collected() {}\nfn caller() { search_collected(); }\n// the little brown fox sleeps\n").unwrap();
    dir
}
#[test]
fn single_search_defaults_threshold_and_pages() {
    let dir = fixture();
    let first = output(
        dir.path(),
        &["serach_collected", "--scope", "a.rs", "--limit", "1"],
    );
    assert!(
        first.contains("WARNING: No exact matches") && first.contains("suggestions 1-1 of 2"),
        "{first}"
    );
    let second = output(
        dir.path(),
        &[
            "serach_collected",
            "--scope",
            "a.rs",
            "--offset",
            "1",
            "--limit",
            "1",
        ],
    );
    assert!(second.contains("suggestions 2-2 of 2"), "{second}");
    let exact_empty_page = output(
        dir.path(),
        &["search_collected", "--scope", "a.rs", "--offset", "20"],
    );
    assert!(
        !exact_empty_page.contains("Similar suggestions"),
        "{exact_empty_page}"
    );
    let high = output(
        dir.path(),
        &[
            "serach_collected",
            "--scope",
            "a.rs",
            "--similarity-threshold",
            "0.99",
        ],
    );
    assert!(!high.contains("Similar suggestions"), "{high}");
    let bad = run(
        dir.path(),
        &["serach_collected", "--similarity-threshold", "NaN"],
    );
    assert!(!bad.status.success());
}
#[test]
fn compact_and_literal_modes_use_the_same_limits() {
    let dir = fixture();
    let literal = output(dir.path(), &["little brown box", "--kind", "content"]);
    assert!(
        literal.contains("little brown fox") && literal.contains("WARNING: No exact matches"),
        "{literal}"
    );
    std::fs::write(dir.path().join("b.txt"), "extra\n").unwrap();
    let tight = output(
        dir.path(),
        &["serach_collected", "--similarity-max-files", "1"],
    );
    assert!(!tight.contains("Similar suggestions"), "{tight}");
    let relaxed = output(
        dir.path(),
        &["serach_collected", "--similarity-max-files", "2"],
    );
    assert!(relaxed.contains("Similar suggestions"), "{relaxed}");
    let batch = output(
        dir.path(),
        &["-q", "search_collected", "-q", "serach_collected"],
    );
    assert_eq!(batch.matches("# Similar suggestions").count(), 1, "{batch}");
    let regex = output(dir.path(), &["/serach_collecte./", "--kind", "content"]);
    assert!(!regex.contains("Similar suggestions"), "{regex}");
}
