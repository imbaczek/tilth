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
        first.contains("# Similar suggestions") && first.contains("suggestions 1-1 of 2"),
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
        literal.contains("little brown fox") && literal.contains("# Similar suggestions"),
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

#[test]
fn default_scope_accepts_more_than_twenty_files_and_can_be_narrowed() {
    let dir = fixture();
    for n in 0..20 {
        std::fs::write(dir.path().join(format!("extra_{n}.txt")), "padding\n").unwrap();
    }
    let default = output(dir.path(), &["serach_collected"]);
    assert!(default.contains("Similar suggestions"), "{default}");
    let restricted = output(
        dir.path(),
        &["serach_collected", "--similarity-max-files", "20"],
    );
    assert!(!restricted.contains("Similar suggestions"), "{restricted}");
}

#[test]
fn similar_filenames_work_in_large_scopes_without_previewing_files() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..500 {
        std::fs::write(dir.path().join(format!("filler_{index}.txt")), "").unwrap();
    }
    let candidate = dir.path().join("catalog_digest.py");
    std::fs::File::create(&candidate)
        .unwrap()
        .set_len(1_000_001)
        .unwrap();
    for extra in [&[][..], &["--similarity-max-files", "1"][..]] {
        let mut args = vec!["catalog_digests", "--budget", "2000"];
        args.extend_from_slice(extra);
        let out = output(dir.path(), &args);
        assert!(
            out.contains("# Similar suggestions")
                && out.contains("[filename]")
                && out.contains("catalog_digest.py"),
            "{out}"
        );
        assert!(!out.contains("```"), "{out}");
    }
}

#[test]
fn filename_suggestions_respect_paging_globs_thresholds_and_exact_hits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("catalog_digest.py"),
        "SECRET_SOURCE_MARKER\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("catalog_digest.rs"),
        "OTHER_SOURCE_MARKER\n",
    )
    .unwrap();
    let first = output(dir.path(), &["catalog_digests", "--limit", "1"]);
    let second = output(
        dir.path(),
        &["catalog_digests", "--offset", "1", "--limit", "1"],
    );
    assert!(
        first.contains("suggestions 1-1 of 2") && second.contains("suggestions 2-2 of 2"),
        "{first}\n{second}"
    );
    assert!(
        !first.contains("SOURCE_MARKER") && !second.contains("SOURCE_MARKER"),
        "{first}\n{second}"
    );
    let dot = output(
        dir.path(),
        &["--query", "./catalog_digests.rs", "--kind", "symbol"],
    );
    assert!(
        dot.contains("catalog_digest.rs") && dot.contains("[filename]"),
        "{dot}"
    );
    let absolute_query = dir.path().join("catalog_digests.rs");
    let absolute = output(
        dir.path(),
        &[
            "--query",
            absolute_query.to_str().unwrap(),
            "--kind",
            "symbol",
        ],
    );
    assert!(
        absolute.contains("catalog_digest.rs") && absolute.contains("[filename]"),
        "{absolute}"
    );
    let filtered = output(dir.path(), &["catalog_digests", "--glob", "*.rs"]);
    assert!(
        filtered.contains("catalog_digest.rs") && !filtered.contains("catalog_digest.py"),
        "{filtered}"
    );
    let strict = output(
        dir.path(),
        &["catalog_digests", "--similarity-threshold", "0.99"],
    );
    assert!(!strict.contains("[filename]"), "{strict}");
    std::fs::write(dir.path().join("other.rs"), "fn catalog_digests() {}\n").unwrap();
    let exact = output(dir.path(), &["catalog_digests"]);
    assert!(!exact.contains("[filename]"), "{exact}");
}

#[test]
fn exact_filename_stem_suppresses_fuzzy_suggestions_per_query() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("catalog_digest.py"),
        "def unrelated():\n    return 42\n",
    )
    .unwrap();
    let exact = output(dir.path(), &["catalog_digest"]);
    assert!(
        exact.contains("File overview") && !exact.contains("Similar suggestions"),
        "{exact}"
    );
    let batch = output(
        dir.path(),
        &["-q", "catalog_digest", "-q", "catalog_digests"],
    );
    assert!(
        !batch.contains("Similar suggestions for \"catalog_digest\"")
            && batch.contains("Similar suggestions for \"catalog_digests\""),
        "{batch}"
    );
    std::fs::write(dir.path().join("catalog_export.bin"), [0, 255, 0]).unwrap();
    std::fs::write(dir.path().join("catalog_exports.py"), "").unwrap();
    let binary = output(
        dir.path(),
        &["--query", "catalog_export", "--kind", "symbol"],
    );
    assert!(!binary.contains("Similar suggestions"), "{binary}");
}

fn assert_final_token_summary(output: &str) {
    let output = output.trim_end_matches('\n');
    let (body, footer) = output.rsplit_once("\n\n(").expect("final token summary");
    let count = body.len().div_ceil(4);
    let expected = if count >= 1000 {
        format!("~{}.{}k tokens)", count / 1000, (count % 1000) / 100)
    } else {
        format!("~{count} tokens)")
    };
    assert_eq!(footer, expected, "{output}");
}

#[test]
fn token_summary_follows_all_suggestions_in_normal_paged_and_batch_searches() {
    let dir = fixture();
    for args in [
        vec!["serach_collected"],
        vec!["serach_collected", "--limit", "1"],
        vec!["--query", "search_collected", "--query", "serach_collected"],
        vec!["serach_collected", "--budget", "60"],
    ] {
        let out = output(dir.path(), &args);
        assert_final_token_summary(&out);
        assert!(!out.contains("End of listing."), "{out}");
        let body = out.rsplit_once("\n\n(").unwrap().0;
        assert!(!body.contains("\n\n(~"), "{out}");
        if out.contains("Page or navigation omitted") {
            assert!(!out.contains("Next page:"), "{out}");
        }
    }
}

#[test]
fn token_summary_counts_unicode_bytes_and_preserves_footer_shaped_source() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("résumé.rs"),
        "fn exact_symbol() {} // (~9 tokens)\nfn search_collected() {}\n",
    )
    .unwrap();
    let out = output(
        dir.path(),
        &["--query", "exact_symbol", "--query", "serach_collected"],
    );
    assert!(out.contains("(~9 tokens)"), "{out}");
    assert!(
        out.contains("résumé.rs") && out.contains("Similar suggestions"),
        "{out}"
    );
    assert_final_token_summary(&out);
}
