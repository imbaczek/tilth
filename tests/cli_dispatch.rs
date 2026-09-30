use std::fmt::Write as _;
use std::fs;
use std::process::Command;

#[test]
fn decorator_grok_and_symbol_search_keep_one_correct_definition() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("service.ts");
    fs::write(
        &path,
        "@Injectable()\nexport class Service {\n @First()\n method() {}\n}\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .arg("grok")
        .arg(format!("{}:3", path.display()))
        .arg("--scope")
        .arg(root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let grok = String::from_utf8(output.stdout).unwrap();
    assert!(grok.starts_with("# grok: method ["), "{grok}");
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .arg("Service")
        .arg("--scope")
        .arg(root.path())
        .arg("--expand=0")
        .output()
        .unwrap();
    assert!(output.status.success());
    let search = String::from_utf8(output.stdout).unwrap();
    assert!(search.contains("1 definitions"), "{search}");
    assert_eq!(search.matches("[definition]").count(), 1, "{search}");
}

#[test]
fn grok_non_ts_caller_totals_survive_default_and_full_display_caps() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target.rs");
    fs::write(&target, "pub fn target() {}\n").unwrap();
    let mut callers = String::new();
    for i in 0..75 {
        writeln!(callers, "fn caller{i:02}() {{ target(); }}").unwrap();
    }
    fs::write(root.path().join("callers.rs"), callers).unwrap();
    let mut tests = String::new();
    for i in 0..65 {
        writeln!(tests, "#[test]\nfn case{i:02}() {{ target(); }}").unwrap();
    }
    fs::write(root.path().join("cases_test.rs"), tests).unwrap();
    for full in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tilth"));
        command
            .arg("grok")
            .arg(format!("{}:1", target.display()))
            .arg("--scope")
            .arg(root.path());
        if full {
            command.arg("--full");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(
            output.contains(if full {
                "## callers (50 of 75)"
            } else {
                "## callers (5 of 75)"
            }),
            "{output}"
        );
        assert!(
            output.contains(if full {
                "## tests (30 of 65)"
            } else {
                "## tests (8 of 65)"
            }),
            "{output}"
        );
    }
}

#[test]
fn deps_full_expands_preview_and_obeys_budget() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("target.ts");
    let mut imports = String::new();
    for i in 0..12 {
        fs::write(
            root.path().join(format!("dep{i:02}.ts")),
            "export const value = 1;\n",
        )
        .unwrap();
        writeln!(imports, "import {{ value as v{i} }} from './dep{i:02}';").unwrap();
    }
    fs::write(&path, imports).unwrap();
    let run = |extra: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .arg(&path)
            .arg("--deps")
            .arg("--scope")
            .arg(root.path())
            .args(extra)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let preview = run(&[]);
    assert!(preview.contains("12 local"), "{preview}");
    assert!(!preview.contains("dep11.ts"), "{preview}");
    let full = run(&["--full"]);
    assert!(full.contains("dep11.ts"), "{full}");
    let limited = run(&["--full", "--budget", "30"]);
    assert!(limited.len() < full.len());
}

#[test]
fn repeated_scopes_full_expand_zero_still_obey_final_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();

    for scope in [&first, &second] {
        for i in 0..15 {
            fs::write(
                scope.join(format!("file_{i:02}.rs")),
                "pub fn WidgetThing() {}\n",
            )
            .unwrap();
        }
    }

    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(tmp.path())
        .arg("WidgetThing")
        .arg("--scope")
        .arg(&first)
        .arg("--scope")
        .arg(&second)
        .arg("--full")
        .arg("--expand=0")
        .arg("--budget")
        .arg("80")
        .output()
        .expect("run tilth CLI");

    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("CLI output is UTF-8");
    let search_header = stdout
        .lines()
        .find(|line| line.starts_with("# Search:"))
        .unwrap_or_else(|| panic!("missing search header: {stdout}"));
    let count = search_header
        .rsplit_once("— ")
        .expect("search header count separator")
        .1
        .split_whitespace()
        .next()
        .expect("search header count")
        .parse::<usize>()
        .expect("numeric match count");

    assert!(
        count > 15,
        "--full --expand=0 with both scopes should retain more than one scope's default cap; header: {search_header}"
    );
    assert!(
        stdout.contains("... total omitted: ") && stdout.contains("tokens (budget: 80)"),
        "--budget must cap the final output after both scopes are joined: {stdout}"
    );
}

fn large_cli_source(dir: &std::path::Path) -> std::path::PathBuf {
    let mut source = String::new();
    for i in 0..60 {
        source.push_str(&format!("pub fn operation_{i}() {{\n"));
        source.push_str(&"    let body_only_marker = 123456789;\n".repeat(20));
        source.push_str("}\n\n");
    }
    let path = dir.join("large.rs");
    fs::write(&path, source).unwrap();
    path
}

#[test]
fn captured_file_read_keeps_automatic_outline() {
    let tmp = tempfile::tempdir().unwrap();
    let path = large_cli_source(tmp.path());
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .arg(&path)
        .output()
        .expect("run captured CLI");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("[outline]"),
        "captured file read should use the normal smart view: {:?}",
        stdout.lines().next()
    );
    assert!(
        stdout.contains("operation_59"),
        "last function must be visible"
    );
    assert!(
        !stdout.contains("body_only_marker"),
        "outline must omit bodies"
    );
}

#[test]
fn captured_file_read_honors_explicit_full() {
    let tmp = tempfile::tempdir().unwrap();
    let path = large_cli_source(tmp.path());
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .arg(&path)
        .arg("--full")
        .output()
        .expect("run captured CLI with --full");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("[full]"));
    assert!(stdout.contains("body_only_marker"));
    assert!(stdout.contains("operation_59"));
}

#[test]
fn captured_file_read_honors_explicit_section() {
    let tmp = tempfile::tempdir().unwrap();
    let path = large_cli_source(tmp.path());
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .arg(&path)
        .args(["--section", "1-2"])
        .output()
        .expect("run captured CLI with --section");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("[section]"));
    assert!(stdout.contains("operation_0"));
    assert!(stdout.contains("body_only_marker"));
    assert!(!stdout.contains("operation_59"));
}
