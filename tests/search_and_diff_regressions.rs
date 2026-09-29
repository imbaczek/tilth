//! Regression coverage for independent search and directory-diff reports.

use std::fs;
use std::path::Path;
use std::process::Command;

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
