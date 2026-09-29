//! Regression coverage for independent search and directory-diff reports.

use std::fs;
use std::path::Path;
use std::process::Command;
use tilth::cache::OutlineCache;

#[test]
fn callers_disclose_the_display_cap() {
    let dir = tempfile::tempdir().unwrap();
    let source: String = (0..12)
        .map(|i| format!("fn caller_{i}() {{ target_fn(); }}\n"))
        .collect();
    fs::write(
        dir.path().join("calls.rs"),
        format!("fn target_fn() {{}}\n{source}"),
    )
    .unwrap();
    let full = tilth::run_callers("target_fn", dir.path(), 0, None, None, true).unwrap();
    assert_eq!(full.matches("[caller:").count(), 12);
    let limited = tilth::run_callers("target_fn", dir.path(), 0, None, None, false).unwrap();
    assert_eq!(limited.matches("[caller:").count(), 10);
    assert!(
        limited.contains("10 of 12")
            || limited.contains("2 more call sites")
            || limited.contains("2 call sites omitted"),
        "caller search must disclose the two undisplayed sites: {limited}"
    );
}

#[test]
fn repo_search_finds_a_method_inside_an_inline_module() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("service.rs"),
        "mod service {\n    pub struct Worker;\n    impl Worker {\n        pub fn nested_needle() {}\n    }\n}\n\nfn flat_needle() {}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("usage.rs"),
        "fn consumer() { Worker::nested_needle(); }\n",
    )
    .unwrap();
    let cache = OutlineCache::new();
    let flat = tilth::run("flat_needle", dir.path(), None, None, None, &cache).unwrap();
    assert!(
        flat.contains("[definition]"),
        "fixture must be parsed as Rust"
    );
    let nested = tilth::run("nested_needle", dir.path(), None, None, None, &cache).unwrap();
    assert!(
        nested.contains("[definition]"),
        "repo-wide search missed the nested method definition: {nested}"
    );
}

fn git(root: &Path, args: &[&str]) {
    let result = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn diff_directory_scope_includes_descendants_without_prefix_siblings() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    for (file, name) in [
        ("src/first.rs", "first"),
        ("src/nested/second.rs", "second"),
        ("src_other/excluded.rs", "excluded"),
    ] {
        let path = dir.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("fn {name}() -> u8 {{ 1 }}\n")).unwrap();
    }
    git(dir.path(), &["add", "."]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "initial",
        ],
    );
    for (file, name) in [
        ("src/first.rs", "first"),
        ("src/nested/second.rs", "second"),
        ("src_other/excluded.rs", "excluded"),
    ] {
        fs::write(
            dir.path().join(file),
            format!("fn {name}() -> u8 {{ 2 }}\n"),
        )
        .unwrap();
    }
    // Compare changed files with the initial commit without changing process cwd.
    let source = tilth::diff::DiffSource::GitUncommitted;
    let file = tilth::diff::diff(
        &source,
        Some(dir.path()),
        Some("src/first.rs"),
        None,
        false,
        0,
        None,
    )
    .unwrap();
    assert!(file.contains("first"));
    for scope in ["src", "src/"] {
        let output =
            tilth::diff::diff(&source, Some(dir.path()), Some(scope), None, false, 0, None)
                .unwrap_or_else(|error| panic!("directory scope {scope:?} failed: {error}"));
        assert!(output.contains("src/first.rs"));
        assert!(output.contains("src/nested/second.rs"));
        assert!(!output.contains("src_other"));
        assert!(!output.contains("excluded"));
    }
}

#[test]
fn scoped_diff_preserves_cross_file_symbol_moves() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    fs::write(
        dir.path().join("a.rs"),
        "fn moved() { println!(\"hello\"); }\n",
    )
    .unwrap();
    fs::write(dir.path().join("b.rs"), "fn stays() {}\n").unwrap();
    fs::write(dir.path().join("unrelated.rs"), "fn unrelated() {}\n").unwrap();
    commit_diff_regression_fixture(dir.path(), "initial");

    fs::write(dir.path().join("a.rs"), "").unwrap();
    fs::write(
        dir.path().join("b.rs"),
        "fn stays() {}\nfn moved() { println!(\"hello\"); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("unrelated.rs"),
        "fn unrelated() { let _ = 1; }\n",
    )
    .unwrap();

    let source = tilth::diff::DiffSource::GitUncommitted;
    for scope in ["a.rs", "b.rs"] {
        let output =
            tilth::diff::diff(&source, Some(dir.path()), Some(scope), None, false, 0, None)
                .unwrap();
        assert!(output.contains("[↦] moved"), "scope {scope}: {output}");
        assert!(!output.contains("unrelated"), "scope {scope}: {output}");
    }

    commit_diff_regression_fixture(dir.path(), "move symbol");
    let source = tilth::diff::DiffSource::GitRef("HEAD^..HEAD".to_string());
    let output = tilth::diff::diff(
        &source,
        Some(dir.path()),
        Some("a.rs"),
        None,
        false,
        0,
        None,
    )
    .unwrap();
    assert!(output.contains("[↦] moved"), "committed range: {output}");
}

fn commit_diff_regression_fixture(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            message,
        ],
    );
}

#[test]
fn diff_directory_scope_includes_renames_from_the_source_path_in_diff_and_log() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);

    for (file, content) in [
        ("old/a.rs", "fn moved() -> u8 { 1 }\n"),
        ("old/keeper.rs", "fn keeper() -> u8 { 1 }\n"),
    ] {
        let path = dir.path().join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    commit_diff_regression_fixture(dir.path(), "initial");

    fs::create_dir_all(dir.path().join("new")).unwrap();
    git(dir.path(), &["mv", "old/a.rs", "new/a.rs"]);

    let staged = tilth::diff::DiffSource::GitStaged;
    let output =
        tilth::diff::diff(&staged, Some(dir.path()), Some("old"), None, false, 0, None).unwrap();
    assert!(
        output.contains("new/a.rs"),
        "source directory scope omitted staged rename: {output}"
    );

    commit_diff_regression_fixture(dir.path(), "move from old");
    let log_source = tilth::diff::DiffSource::Log("HEAD^..HEAD".to_string());
    let log = tilth::diff::diff(
        &log_source,
        Some(dir.path()),
        Some("old"),
        None,
        false,
        0,
        None,
    )
    .unwrap();
    assert!(
        log.contains("move from old"),
        "source directory scope omitted rename from log: {log}"
    );
}

#[test]
fn diff_directory_scope_supports_removed_directories_and_component_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);

    fs::create_dir_all(dir.path().join("gone")).unwrap();
    fs::create_dir_all(dir.path().join("gone-other")).unwrap();
    fs::write(dir.path().join("gone/a.rs"), "fn removed() -> u8 { 1 }\n").unwrap();
    fs::write(
        dir.path().join("gone-other/a.rs"),
        "fn affected() -> u8 { 1 }\nfn unrelated() -> u8 { 1 }\n",
    )
    .unwrap();
    commit_diff_regression_fixture(dir.path(), "initial");

    fs::remove_file(dir.path().join("gone/a.rs")).unwrap();
    fs::remove_dir(dir.path().join("gone")).unwrap();
    fs::write(
        dir.path().join("gone-other/a.rs"),
        "fn affected() -> u8 { 2 }\nfn unrelated() -> u8 { 2 }\n",
    )
    .unwrap();

    let source = tilth::diff::DiffSource::GitUncommitted;
    let absolute_scope = dir.path().join("gone").to_string_lossy().into_owned();
    for scope in ["gone/", absolute_scope.as_str()] {
        let output =
            tilth::diff::diff(&source, Some(dir.path()), Some(scope), None, false, 0, None)
                .unwrap();
        assert!(
            output.contains("gone/a.rs"),
            "removed directory scope {scope:?} omitted deletion: {output}"
        );
        assert!(
            !output.contains("gone-other/a.rs"),
            "removed directory scope {scope:?} included a prefix sibling: {output}"
        );
    }

    let file_scope = tilth::diff::diff(
        &source,
        Some(dir.path()),
        Some("gone-other/a.rs"),
        None,
        false,
        0,
        None,
    )
    .unwrap();
    assert!(file_scope.contains("gone-other/a.rs"), "{file_scope}");
    assert!(!file_scope.contains("gone/a.rs"), "{file_scope}");

    let function_scope = tilth::diff::diff(
        &source,
        Some(dir.path()),
        Some("gone-other/a.rs:affected"),
        None,
        false,
        0,
        None,
    )
    .unwrap();
    assert!(
        function_scope.contains("gone-other/a.rs"),
        "file:function scope was not preserved: {function_scope}"
    );
}
