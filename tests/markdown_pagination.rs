use std::fmt::Write as _;
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
    let output = run(root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut content = String::from("# Root\n");
    for i in 0..144 {
        writeln!(
            content,
            "## Heading {i}\n{}",
            "padding line with enough text\n".repeat(150)
        )
        .unwrap();
    }
    assert!(content.len() > 500_000);
    fs::write(dir.path().join("guide.md"), content).unwrap();
    dir
}

fn rows(output: &str) -> Vec<Vec<&str>> {
    output
        .lines()
        .filter(|line| line.starts_with('['))
        .map(|line| line.split_whitespace().collect())
        .collect()
}

#[test]
fn explicit_budget_replaces_fixed_large_file_outline_cap() {
    let dir = fixture();
    for prefix in [vec!["guide.md"], vec!["read", "guide.md"]] {
        let default = success(dir.path(), &prefix);
        assert_eq!(rows(&default).len(), 100);
        assert!(default.contains("--budget <tokens>"), "{default}");
        let mut args = prefix.clone();
        args.extend(["--budget", "20000"]);
        let roomy = success(dir.path(), &args);
        assert_eq!(rows(&roomy).len(), 145);
        assert!(roomy.contains("Heading 143"), "{roomy}");
        assert!(!roomy.contains("below the cap"), "{roomy}");
        let mut args = prefix;
        args.extend(["--budget", "400"]);
        let tight = success(dir.path(), &args);
        assert!(rows(&tight).len() < 100, "{tight}");
        assert!(tight.len() <= 400 * 4 + 1, "{tight}");
        assert!(tight.contains("--section toc:<TOC>"), "{tight}");
    }
}

#[test]
fn markdown_pages_are_disjoint_with_global_addresses() {
    let dir = fixture();
    for prefix in [
        vec!["guide.md"],
        vec!["search", "guide.md"],
        vec!["read", "guide.md"],
    ] {
        let mut first_args = prefix.clone();
        first_args.extend(["--offset", "0", "--limit", "80"]);
        let first = success(dir.path(), &first_args);
        let mut second_args = prefix.clone();
        second_args.extend(["--offset", "80", "--limit", "80"]);
        let second = success(dir.path(), &second_args);
        assert_eq!(rows(&first).len(), 80);
        assert_eq!(rows(&second).len(), 65);
        assert!(
            first.contains("Next page: --offset 80 --limit 80"),
            "{first}"
        );
        assert!(
            second.contains("Requested headings 81-145 of 145"),
            "{second}"
        );
        assert!(second.contains("End of listing."), "{second}");
        let second_rows = rows(&second);
        assert_eq!(second_rows[0][1], "1.80");
        let selected = success(
            dir.path(),
            &["guide.md", "--section", "toc:1.80", "--budget", "2000"],
        );
        assert!(selected.contains("## Heading 79"), "{selected}");
        let mut beyond_args = prefix;
        beyond_args.extend(["--offset", "999999", "--limit", "2"]);
        let beyond = success(dir.path(), &beyond_args);
        assert!(beyond.contains("Requested headings 0 of 145"), "{beyond}");
        assert!(
            rows(&beyond).is_empty() && !beyond.contains("Next page:"),
            "{beyond}"
        );
    }
}

#[test]
fn budgeted_page_does_not_skip_headings_hidden_by_truncation() {
    let dir = fixture();
    for prefix in [vec!["guide.md"], vec!["read", "guide.md"]] {
        let mut args = prefix;
        args.extend(["--offset", "0", "--limit", "80", "--budget", "200"]);
        let output = success(dir.path(), &args);
        assert!(rows(&output).len() < 80, "{output}");
        assert!(!output.contains("Next page:"), "{output}");
        assert!(
            output.contains("retry the same offset with a smaller limit"),
            "{output}"
        );
        assert!(output.len() <= 200 * 4 + 1, "{output}");
    }
}

#[test]
fn markdown_pages_reject_content_modes_and_batches() {
    let dir = fixture();
    for prefix in [vec!["guide.md"], vec!["read", "guide.md"]] {
        for option in ["--full", "--section"] {
            let mut args = prefix.clone();
            args.extend(["--limit", "2", option]);
            if option == "--section" {
                args.push("Root");
            }
            assert!(!run(dir.path(), &args).status.success(), "{args:?}");
        }
    }
    for args in [
        vec!["read", "guide.md", "--mode", "signature", "--limit", "2"],
        vec!["read", "guide.md", "guide.md", "--limit", "2"],
        vec!["guide.md", "--limit", "0"],
    ] {
        assert!(!run(dir.path(), &args).status.success(), "{args:?}");
    }
}

#[test]
fn markdown_extension_and_small_file_pages_work() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("small.markdown"),
        "# Root\n## Child\nbody\n",
    )
    .unwrap();
    for args in [
        vec!["small.markdown", "--offset", "1", "--limit", "1"],
        vec!["read", "small.markdown", "--offset", "1", "--limit", "1"],
    ] {
        let output = success(dir.path(), &args);
        assert_eq!(rows(&output).len(), 1, "{output}");
        assert!(
            output.contains("[outline]") && output.contains("1.1"),
            "{output}"
        );
        assert!(output.contains("Requested headings 2-2 of 2"), "{output}");
    }
}

#[test]
fn markdown_paging_rejects_multiple_relative_files_but_allows_one_absolute_file() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["one", "two"] {
        fs::create_dir(dir.path().join(name)).unwrap();
        fs::write(
            dir.path().join(name).join("guide.md"),
            "# Root\n## Child\nbody\n",
        )
        .unwrap();
    }
    let path = dir.path().join("one/guide.md");
    let mut args = vec![
        "guide.md", "--scope", "one", "--scope", "two", "--limit", "1",
    ];
    assert!(!run(dir.path(), &args).status.success());
    args[0] = path.to_str().unwrap();
    let output = success(dir.path(), &args);
    assert_eq!(rows(&output).len(), 1, "{output}");
}
