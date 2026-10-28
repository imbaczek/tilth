use std::path::Path;
use std::process::Command;

fn search(root: &Path, args: &[&str]) -> String {
    let result = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

#[test]
fn zero_hit_substrings_rank_frequency_and_similarity_and_keep_filename_suggestions() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = String::from("fn parcel_load_a() {}\n");
    source.push_str(&"parcel_load_common_extra();\n".repeat(12));
    source.push_str("parcel_load_longer_name();\nparcel_load_z();\n");
    std::fs::write(dir.path().join("parcel_load_a.rs"), source).unwrap();
    let out = search(dir.path(), &["parcel_load", "--similarity-max-files", "1"]);
    assert!(
        out.contains("# Substring matches") && out.contains("15 matching lines"),
        "{out}"
    );
    assert!(out.contains("[filename]"), "{out}");
    assert!(
        out.find("parcel_load_common_extra  12 matching lines")
            .unwrap()
            < out
                .find("parcel_load_longer_name  1 matching lines")
                .unwrap_or(usize::MAX),
        "{out}"
    );
    let page = search(
        dir.path(),
        &["parcel_load", "--limit", "1", "--offset", "1"],
    );
    assert!(
        page.contains("candidates 2-2 of 3") && page.contains("15 matching lines"),
        "{page}"
    );
    let exact = search(dir.path(), &["parcel_load_a", "--offset", "30"]);
    assert!(!exact.contains("# Substring matches"), "{exact}");
    let missing = search(dir.path(), &["nonexistent_needle"]);
    assert!(!missing.contains("# Substring matches"), "{missing}");
    let regex = search(dir.path(), &["/parcel_load/", "--kind", "regex"]);
    assert!(!regex.contains("# Substring matches"), "{regex}");
}

#[test]
fn substring_names_support_lisp_and_deduplicated_scopes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("sample.lisp"),
        "(compile-file-pathname)\n(compile-file-pathname)\n",
    )
    .unwrap();
    let out = search(
        dir.path(),
        &[
            "-q",
            "compile-file-path",
            "--kind",
            "symbol",
            "--scope",
            ".",
            "--scope",
            "sample.lisp",
        ],
    );
    assert!(
        out.contains("2 matching lines in 1 files")
            && out.contains("compile-file-pathname  2 matching lines"),
        "{out}"
    );
}

#[test]
fn exact_filename_does_not_hide_independent_substring_results() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("needle.rs"), "fn needle_alpha() {}\n").unwrap();
    let out = search(dir.path(), &["-q", "needle", "--kind", "symbol"]);
    assert!(
        out.contains("# Substring matches") && out.contains("needle_alpha"),
        "{out}"
    );
    assert!(!out.contains("# Similar suggestions"), "{out}");
}

#[test]
fn substring_locations_use_full_counts_and_direct_directories() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["alpha", "beta", "alpha/nested"] {
        std::fs::create_dir_all(dir.path().join(name)).unwrap();
    }
    for (name, count) in [
        ("alpha/a.rs", 4),
        ("alpha/b.rs", 2),
        ("beta/c.rs", 3),
        ("alpha/nested/d.rs", 1),
    ] {
        std::fs::write(dir.path().join(name), "needle_suffix();\n".repeat(count)).unwrap();
    }
    let out = search(
        dir.path(),
        &[
            "-q", "needle", "--kind", "symbol", "--limit", "1", "--scope", ".", "--scope", "alpha",
        ],
    );
    assert!(
        out.contains("> Files: alpha/a.rs (4), beta/c.rs (3), alpha/b.rs (2)"),
        "{out}"
    );
    assert!(
        out.contains("> Directories (direct): alpha (6), beta (3), alpha/nested (1)"),
        "{out}"
    );
    assert!(out.contains("10 matching lines in 4 files"), "{out}");
    let page = search(
        dir.path(),
        &[
            "-q", "needle", "--kind", "symbol", "--limit", "1", "--offset", "10",
        ],
    );
    assert!(
        page.contains("> Files: alpha/a.rs (4), beta/c.rs (3), alpha/b.rs (2)"),
        "{page}"
    );
    let scoped = search(
        dir.path(),
        &["-q", "needle", "--kind", "symbol", "--scope", "alpha/a.rs"],
    );
    assert!(
        scoped.contains("> Files: a.rs (4)") && scoped.contains("> Directories (direct): . (4)"),
        "{scoped}"
    );
}
