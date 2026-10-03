use std::fs;
use std::process::{Command, Output};

fn run(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn success(root: &std::path::Path, args: &[&str]) -> String {
    let out = run(root, args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    // Reverse creation order must not affect pagination.
    for i in (0..57).rev() {
        fs::write(root.path().join(format!("entry{i:03}.txt")), "data").unwrap();
    }
    root
}

#[test]
fn directory_queries_and_read_share_stable_pages() {
    let root = fixture();
    for prefix in [vec!["."], vec!["search", "."], vec!["read", "."]] {
        let default = success(root.path(), &prefix);
        assert_eq!(
            default.lines().filter(|l| l.starts_with("  entry")).count(),
            50
        );
        assert!(default.contains("1-50 of 57"));
        assert!(
            default.contains("Next page: --offset 50 --limit 50"),
            "{default}"
        );
        assert!(!default.contains(r#""offset":50"#), "{default}");
        let mut first_args = prefix.clone();
        first_args.extend(["--limit", "30"]);
        let first = success(root.path(), &first_args);
        let mut second_args = prefix.clone();
        second_args.extend(["--limit", "30", "--offset", "30"]);
        let second = success(root.path(), &second_args);
        assert!(second.contains("31-57 of 57") && second.contains("End of listing."));
        for i in 0..57 {
            let name = format!("  entry{i:03}.txt ");
            assert_eq!(
                first.matches(&name).count() + second.matches(&name).count(),
                1,
                "{name}"
            );
        }
        let mut beyond_args = prefix.clone();
        beyond_args.extend(["--offset", "999999", "--limit", "1"]);
        let beyond = success(root.path(), &beyond_args);
        assert!(beyond.contains("Showing items 0 of 57") && beyond.contains("offset 999999"));
        assert!(!beyond.contains("  entry") && !beyond.contains("Next page:"));
        let mut full_args = prefix;
        full_args.push("--full");
        let full = success(root.path(), &full_args);
        assert_eq!(
            full.lines().filter(|l| l.starts_with("  entry")).count(),
            50
        );
    }
    let before_read = success(
        root.path(),
        &["--offset", "55", "--limit", "2", "read", "."],
    );
    assert!(before_read.contains("56-57 of 57"));
}

#[test]
fn directory_pagination_rejects_unsupported_queries_and_bad_limits() {
    let root = fixture();
    for args in [
        vec![".", "--limit", "0"],
        vec!["read", ".", "--limit", "0"],
        vec![".", "--offset", "-1"],
        vec!["entry000.txt", "--limit", "2"],
        vec!["search", "entry000.txt", "--limit", "2"],
        vec!["read", "entry000.txt", "--offset", "0"],
        vec!["unknown_symbol", "--limit", "2"],
        vec!["*.txt", "--offset", "0"],
        vec![".", "--map", "--offset", "0"],
        vec![".", "--deps", "--limit", "1"],
        vec!["read", ".", ".", "--limit", "1"],
        vec!["read", ".", "--mode", "signature"],
        vec![".", "--section", "1-2"],
    ] {
        let out = run(root.path(), &args);
        assert!(
            !out.status.success(),
            "accepted {args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn directory_budget_cannot_offer_a_continuation_past_hidden_entries() {
    let root = fixture();
    for args in [
        vec![".", "--limit", "55", "--budget", "150"],
        vec!["read", ".", "--limit", "55", "--budget", "150"],
    ] {
        let out = success(root.path(), &args);
        assert!(out.contains("truncated"), "{out}");
        assert!(!out.contains("Next page:"), "{out}");
        assert!(
            out.contains("retry the same offset with a smaller limit"),
            "{out}"
        );
        assert!(!out.contains("entry054"), "{out}");
    }
}

#[test]
fn directory_pages_apply_per_scope_and_keep_missing_scope_errors() {
    let root = tempfile::tempdir().unwrap();
    for dir in ["first", "second"] {
        fs::create_dir(root.path().join(dir)).unwrap();
        for i in 0..4 {
            fs::write(root.path().join(dir).join(format!("{dir}{i}.txt")), "data").unwrap();
        }
    }
    let out = success(
        root.path(),
        &[
            ".", "--scope", "first", "--scope", "second", "--scope", "missing", "--offset", "1",
            "--limit", "2",
        ],
    );
    assert_eq!(out.matches("Showing items 2-3 of 4").count(), 2, "{out}");
    assert!(
        out.contains("## Scope errors") && out.contains("missing"),
        "{out}"
    );
    assert!(
        out.contains("first1.txt") && out.contains("second2.txt"),
        "{out}"
    );
}

#[test]
fn directory_listing_counts_subdirectories_and_symlinks_as_entries() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("a_dir")).unwrap();
    fs::write(root.path().join("b_file.txt"), "data").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("a_dir"), root.path().join("c_link")).unwrap();
    let out = success(root.path(), &[".", "--limit", "1"]);
    assert!(
        out.contains("  a_dir/") && !out.contains("  b_file.txt"),
        "{out}"
    );
    let out = success(root.path(), &["read", ".", "--offset", "1", "--limit", "1"]);
    assert!(
        out.contains("  b_file.txt") && !out.contains("  a_dir/"),
        "{out}"
    );
    #[cfg(unix)]
    {
        let out = success(root.path(), &[".", "--offset", "2", "--limit", "1"]);
        assert!(
            out.contains("  c_link →") && out.contains("End of listing."),
            "{out}"
        );
    }
}

#[test]
fn empty_listing_and_extreme_offset_have_clear_terminal_pages() {
    let root = tempfile::tempdir().unwrap();
    let offset = usize::MAX.to_string();
    for prefix in [vec!["."], vec!["read", "."]] {
        let mut args = prefix;
        args.extend(["--offset", &offset, "--limit", "1"]);
        let out = success(root.path(), &args);
        assert!(
            out.contains("Showing items 0 of 0") && out.contains("End of listing."),
            "{out}"
        );
        assert!(!out.contains("Next page:"), "{out}");
    }
}
