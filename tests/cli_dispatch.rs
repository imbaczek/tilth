use std::fmt::Write as _;
use std::fs;
use std::process::Command;

#[test]
fn caller_search_counts_all_files_before_default_and_full_previews() {
    let root = tempfile::tempdir().unwrap();
    for file in 0..25 {
        let mut source = String::new();
        for i in 0..25 {
            writeln!(source, "fn caller_{file}_{i}() {{ hot(); }}").unwrap();
        }
        fs::write(root.path().join(format!("part{file:02}.rs")), source).unwrap();
    }
    fs::write(
        root.path().join("wide.rs"),
        format!("fn late() {{ hot(); }}\n//{}", "x".repeat(500_001)),
    )
    .unwrap();
    let run = |full: bool, glob: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tilth"));
        command
            .args(["hot", "--callers", "--scope"])
            .arg(root.path());
        if full {
            command.arg("--full");
        }
        if let Some(glob) = glob {
            command.args(["--glob", glob]);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    for full in [false, true] {
        let output = run(full, None);
        assert!(output.contains("— 626 call sites"), "{output}");
        assert_eq!(
            output.matches("[caller:").count(),
            if full { 100 } else { 10 },
            "{output}"
        );
        assert!(
            output
                .lines()
                .find(|line| line.starts_with("## "))
                .unwrap()
                .contains("part00.rs:1"),
            "{output}"
        );
        assert_eq!(run(full, None), output);
        let filtered = run(full, Some("wide.rs"));
        assert!(
            filtered.contains("— 1 call site") && filtered.contains("[caller: late]"),
            "{filtered}"
        );
    }
}

#[test]
fn multiline_docs_are_readable_in_outline_and_grok() {
    let root = tempfile::tempdir().unwrap();
    for (filename, declaration_line, source) in [
        (
            "docs.go",
            4,
            "package demo\n// First sentence.\n// Second sentence.\nfunc Public() {}\n",
        ),
        (
            "docs.ts",
            5,
            "/**\n * First sentence.\n * Second sentence.\n */\nfunction publicFunction() {}\n",
        ),
    ] {
        let path = root.path().join(filename);
        fs::write(
            &path,
            format!(
                "{source}{}",
                "// filler to force an outline rather than full content\n".repeat(1000)
            ),
        )
        .unwrap();
        for grok in [false, true] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_tilth"));
            if grok {
                command
                    .arg("grok")
                    .arg(format!("{}:{declaration_line}", path.display()))
                    .arg("--scope")
                    .arg(root.path());
            } else {
                command.arg(&path);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let output = String::from_utf8(output.stdout).unwrap();
            assert!(
                output.contains("First sentence. Second sentence."),
                "{filename} grok={grok}: {output}"
            );
            assert!(
                !output.contains("// //") && !output.contains("Second sentence. */"),
                "{output}"
            );
            if !grok {
                assert!(output.contains("[outline]"), "{output}");
            }
        }
    }
}

#[test]
fn joined_file_outline_budget_compacts_before_dropping_symbols() {
    let roots = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let fields = (0..30)
        .map(|i| format!("field{i}: string"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = format!("class Container {{\n first(argument: {{{fields}}}) {{}}\n second(argument: {{{fields}}}) {{}}\n}}\n{}", "// padding for file view\n".repeat(1000));
    for root in &roots {
        fs::write(root.path().join("short.ts"), &source).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .arg("short.ts")
        .arg("--scope")
        .arg(roots[0].path())
        .arg("--scope")
        .arg(roots[1].path())
        .args(["--budget", "160"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    for label in ["class Container", "fn first", "fn second"] {
        assert_eq!(output.matches(label).count(), 2, "{output}");
    }
    assert!(
        output.contains("0 entries omitted; signatures omitted"),
        "{output}"
    );
    assert!(output.trim_end().len().div_ceil(4) <= 160);
}

#[test]
fn file_outline_budget_preserves_navigation_and_method_docs() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("service.ts");
    let mut source = String::from("class Service {\n");
    for (name, comment) in [
        ("first", "// section name"),
        ("second", "/** Actual method documentation */"),
        ("third", "// another section"),
    ] {
        writeln!(source, "  {comment}\n  {name}(argumentWithAVeryLongName: string, anotherLongArgument: number): string {{").unwrap();
        for _ in 0..250 {
            writeln!(source, "    console.log(argumentWithAVeryLongName);").unwrap();
        }
        writeln!(source, "    return argumentWithAVeryLongName;\n  }}").unwrap();
    }
    source.push_str("}\n");
    fs::write(&path, source).unwrap();
    let run = |budget: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .arg(&path)
            .args(["--budget", budget])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let roomy = run("1000");
    assert!(roomy.contains("Actual method documentation"), "{roomy}");
    assert!(
        !roomy.contains("section name") && !roomy.contains("another section"),
        "{roomy}"
    );
    let compact = run("100");
    assert!(
        compact.contains("[outline]") && compact.contains("class Service"),
        "{compact}"
    );
    for name in ["first", "second", "third"] {
        assert!(compact.contains(&format!("fn {name}")), "{compact}");
    }
    assert!(!compact.contains("argumentWithAVeryLongName"), "{compact}");
    assert!(
        compact.contains("0 entries omitted; signatures omitted"),
        "{compact}"
    );
    assert!(compact.trim_end().len().div_ceil(4) <= 100);
}

#[test]
fn spec_maps_show_test_titles_and_keep_real_code_symbols() {
    let root = tempfile::tempdir().unwrap();
    for filename in ["service.spec.ts", "service.test.tsx", "service.test.js"] {
        fs::write(
            root.path().join(filename),
            "describe('fn and class member', () => {\n it('method check', () => {});\n});\n",
        )
        .unwrap();
    }
    fs::write(
        root.path().join("code.ts"),
        "export function parse() {}\nexport class Reader {}\n",
    )
    .unwrap();
    fs::write(
        root.path().join("reader.rs"),
        "struct Reader;\nimpl Reader {\n fn parse() {}\n}\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .arg("--map")
        .arg("--scope")
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let map = String::from_utf8(output.stdout).unwrap();
    for filename in ["service.spec.ts", "service.test.tsx", "service.test.js"] {
        assert!(
            map.contains(&format!(
                "{filename}: describe(\"fn and class member\"), it(\"method check\")"
            )),
            "{map}"
        );
    }
    assert!(map.contains("code.ts: parse, Reader"), "{map}");
    assert!(map.contains("reader.rs: Reader, Reader, parse"), "{map}");
}

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

#[test]
fn caller_initializers_use_enclosing_functions_and_preserve_callbacks() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("calls.ts"),
        r#"function target() {}
const globalResult = target();
function outer() {
  const result = target();
  let other = target();
  var legacy = target();
  const nested = () => { const result = target(); };
  it('example', () => { const result = target(); });
}
class Service {
  run() { const result = target(); }
}
"#,
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .args(["target", "--callers", "--scope"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    for (line, label) in [
        (2, "globalResult"),
        (4, "outer"),
        (5, "outer"),
        (6, "outer"),
        (7, "nested"),
        (8, "it('example') callback"),
        (11, "Service.run"),
    ] {
        assert!(
            output.contains(&format!("calls.ts:{line} [caller: {label}]")),
            "{output}"
        );
    }
    assert!(
        !output.contains("[caller: result]") && !output.contains("[caller: Service.result]"),
        "{output}"
    );
}

#[test]
fn grok_root_relative_path_with_nested_scope_keeps_caller_scope() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("src/nested")).unwrap();
    fs::write(
        root.path().join("src/nested/target.rs"),
        "fn target() {}\nfn inside() { target(); }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("outside.rs"),
        "fn outside() { target(); }\n",
    )
    .unwrap();
    for target in ["src/nested/target.rs:1", "target.rs:1"] {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .current_dir(root.path())
            .args(["grok", target, "--scope", "src/nested"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.starts_with("# grok: target ["), "{output}");
        assert!(
            output.contains("inside") && !output.contains("outside"),
            "{output}"
        );
    }
}

#[test]
fn caller_local_constants_and_generators_keep_callable_identity() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("calls.rs"), "fn rust_target() {}\nfn rust_outer() { const LOCAL: () = rust_target(); static OTHER: () = rust_target(); }\n").unwrap();
    fs::write(root.path().join("calls.ts"), "function target() {}\nfunction* generate() { const value = target(); }\nconst nested = function* () { const value = target(); };\n").unwrap();
    for (target, expected) in [
        ("rust_target", vec!["rust_outer", "rust_outer"]),
        ("target", vec!["generate", "nested"]),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .args([target, "--callers", "--scope"])
            .arg(root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("— 2 call sites"), "{output}");
        for label in expected {
            assert!(output.contains(&format!("[caller: {label}]")), "{output}");
        }
        assert!(
            !output.contains("[caller: LOCAL]")
                && !output.contains("[caller: OTHER]")
                && !output.contains("[caller: value]"),
            "{output}"
        );
    }
}

#[test]
fn caller_shell_command_substitutions_use_the_enclosing_function() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("calls.sh"),
        "target() { :; }\nouter() { value=$(target); }\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .args(["target", "--callers", "--scope"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(output.contains("calls.sh:2 [caller: outer]"), "{output}");
}

#[test]
fn caller_go_local_constant_uses_the_enclosing_function() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("calls.go"),
        "package demo\nfunc outer() { const width = len(\"hello\") }\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .args(["len", "--callers", "--scope"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(output.contains("calls.go:2 [caller: outer]"), "{output}");
}
