use serde_json::{json, Value};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn run(root: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn files(root: &Path, count: usize) {
    for i in (0..count).rev() {
        fs::write(root.join(format!("file{i:03}.txt")), "data\n").unwrap();
    }
}

fn assert_safe_budget(output: &str) {
    assert!(
        !output.contains("> Next page:") && !output.contains("> End of listing."),
        "{output}"
    );
    assert!(
        output.contains("retry the same offset with a smaller limit"),
        "{output}"
    );
}

#[test]
fn cli_glob_and_map_cover_all_files_in_stable_pages() {
    let root = tempfile::tempdir().unwrap();
    files(root.path(), 63);
    for prefix in [vec!["*.txt"], vec!["search", "*.txt"], vec![".", "--map"]] {
        let mut pages = Vec::new();
        for offset in [0, 25, 50] {
            let offset = offset.to_string();
            let mut args = prefix.clone();
            args.extend(["--offset", &offset, "--limit", "25", "--budget", "20000"]);
            let output = run(root.path(), &args);
            assert!(output.contains("of 63"), "{output}");
            assert_eq!(output, run(root.path(), &args));
            pages.push(output);
        }
        let joined = pages.join("\n");
        for i in 0..63 {
            assert_eq!(
                joined.matches(&format!("file{i:03}.txt")).count(),
                1,
                "{joined}"
            );
        }
        let mut args = prefix.clone();
        let offset = usize::MAX.to_string();
        args.extend(["--offset", &offset, "--limit", "1"]);
        let beyond = run(root.path(), &args);
        assert!(
            beyond.contains("files 0 of 63") && !beyond.contains("Next page:"),
            "{beyond}"
        );
        let mut args = prefix;
        args.extend(["--limit", "63", "--budget", "100"]);
        assert_safe_budget(&run(root.path(), &args));
    }
    let default = run(root.path(), &["*.txt"]);
    assert_eq!(
        default
            .lines()
            .filter(|line| line.starts_with("  file"))
            .count(),
        20
    );
}

#[test]
fn cli_code_and_signature_pages_reach_declarations_after_old_caps() {
    let root = tempfile::tempdir().unwrap();
    let mut source = String::new();
    for i in 0..135 {
        writeln!(source, "fn item{i:03}() {{\n    let value={i};\n}}\n").unwrap();
    }
    fs::write(root.path().join("code.rs"), source).unwrap();
    for prefix in [
        vec!["code.rs"],
        vec!["read", "code.rs"],
        vec!["read", "code.rs", "--mode", "signature"],
    ] {
        let mut outputs = Vec::new();
        for offset in [0, 70] {
            let offset = offset.to_string();
            let mut args = prefix.clone();
            args.extend(["--offset", &offset, "--limit", "70", "--budget", "20000"]);
            let output = run(root.path(), &args);
            assert!(output.contains("of 135"), "{output}");
            outputs.push(output);
        }
        for i in 0..135 {
            let name = format!("item{i:03}");
            assert_eq!(
                outputs
                    .iter()
                    .filter(|output| output.contains(&name))
                    .count(),
                1,
                "{name}"
            );
        }
        let mut args = prefix;
        args.extend(["--limit", "135", "--budget", "100"]);
        assert_safe_budget(&run(root.path(), &args));
    }
    fs::write(root.path().join("nested.ts"),"class Box {\n  first(\n    value: string,\n  ): string { return value; }\n  second() {}\n}\n").unwrap();
    let outline = run(root.path(), &["nested.ts", "--offset", "1", "--limit", "1"]);
    assert!(
        outline.contains("Within: Box") && outline.contains("first") && !outline.contains("second"),
        "{outline}"
    );
    let signature = run(
        root.path(),
        &[
            "read",
            "nested.ts",
            "--mode",
            "signature",
            "--offset",
            "1",
            "--limit",
            "1",
        ],
    );
    assert!(
        signature.contains("Within: Box")
            && signature.contains("2:")
            && signature.contains("3:")
            && signature.contains("value: string"),
        "{signature}"
    );
}

#[test]
fn cli_source_stripped_and_section_pages_preserve_original_addresses() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("plain.txt"), "first\n\nthird\nfourth\n").unwrap();
    for prefix in [vec!["plain.txt"], vec!["read", "plain.txt", "--full"]] {
        let mut args = prefix;
        args.extend(["--offset", "1", "--limit", "2"]);
        let output = run(root.path(), &args);
        assert!(
            output.contains("source lines 2-3 of 4") && output.contains("2  \n3  third"),
            "{output}"
        );
        assert!(
            !output.contains("1  first") && !output.contains("4  fourth"),
            "{output}"
        );
    }
    fs::write(
        root.path().join("code.rs"),
        "fn needle() {\n// comment\nlet a=1;\n\nlet b=2;\n}\n",
    )
    .unwrap();
    let hash = run(
        root.path(),
        &[
            "read", "code.rs", "--full", "--edit", "--offset", "1", "--limit", "2",
        ],
    );
    assert!(
        hash.contains("2:")
            && hash.contains("|// comment")
            && hash.contains("3:")
            && !hash.contains("|let b=2;"),
        "{hash}"
    );
    let stripped = run(
        root.path(),
        &[
            "read", "code.rs", "--mode", "stripped", "--offset", "1", "--limit", "1",
        ],
    );
    assert!(
        stripped.contains("Showing retained lines 2-2") && stripped.contains("3  let a=1;"),
        "{stripped}"
    );
    let selected = run(
        root.path(),
        &[
            "read",
            "code.rs",
            "--section",
            "2-3",
            "--section",
            "5-6",
            "--offset",
            "2",
            "--limit",
            "1",
        ],
    );
    assert!(
        selected.contains("Showing selected lines 3-3 of 4") && selected.contains("5  let b=2;"),
        "{selected}"
    );
    let empty = run(
        root.path(),
        &[
            "read",
            "code.rs",
            "--full",
            "--offset",
            &usize::MAX.to_string(),
            "--limit",
            "1",
        ],
    );
    assert!(
        empty.contains("source lines 0 of 6") && !empty.contains("Next page:"),
        "{empty}"
    );
}

#[test]
fn deps_pages_collect_late_exports_and_preserve_all_record_sections() {
    let root = tempfile::tempdir().unwrap();
    let mut source = String::new();
    for i in 0..14 {
        fs::write(
            root.path().join(format!("dep{i:02}.ts")),
            "export const value=1;\n",
        )
        .unwrap();
        writeln!(source, "import {{value as v{i}}} from './dep{i:02}';").unwrap();
    }
    for i in 0..31 {
        writeln!(source, "export function f{i}() {{}}").unwrap();
    }
    fs::write(root.path().join("target.ts"), source).unwrap();
    for i in 0..20 {
        fs::write(
            root.path().join(format!("user{i:02}.ts")),
            "import {f30} from './target';\nexport function caller() { f30(); }\n",
        )
        .unwrap();
    }
    let mut pages = Vec::new();
    for offset in [0, 13, 26] {
        let offset = offset.to_string();
        let output = run(
            root.path(),
            &[
                "target.ts",
                "--deps",
                "--offset",
                &offset,
                "--limit",
                "13",
                "--budget",
                "20000",
            ],
        );
        assert!(
            output.contains("14 local") && output.contains("20 dependents"),
            "{output}"
        );
        assert!(output.contains("of 34"), "{output}");
        pages.push(output);
    }
    let joined = pages.join("\n");
    for i in 0..14 {
        assert_eq!(
            joined.matches(&format!("dep{i:02}.ts")).count(),
            1,
            "{joined}"
        );
    }
    for i in 0..20 {
        assert_eq!(
            joined.matches(&format!("user{i:02}.ts")).count(),
            1,
            "{joined}"
        );
    }
    assert_safe_budget(&run(
        root.path(),
        &["target.ts", "--deps", "--limit", "34", "--budget", "100"],
    ));
}

fn patch(root: &Path, files: usize, symbols: usize) -> String {
    let mut patch = String::new();
    for i in 0..files {
        let name = format!("change{i:02}.rs");
        let mut source = String::new();
        for j in 0..symbols {
            writeln!(source, "fn symbol{j:03}() {{}}").unwrap();
        }
        fs::write(root.join(&name), &source).unwrap();
        writeln!(patch,"diff --git a/{name} b/{name}\nnew file mode 100644\n--- /dev/null\n+++ b/{name}\n@@ -0,0 +1,{symbols} @@").unwrap();
        for line in source.lines() {
            writeln!(patch, "+{line}").unwrap();
        }
    }
    fs::write(root.join("changes.patch"), patch).unwrap();
    "changes.patch".into()
}

#[test]
fn diff_pages_use_files_then_symbol_blocks_and_filter_before_slicing() {
    let root = tempfile::tempdir().unwrap();
    let path = patch(root.path(), 7, 65);
    git(root.path(), &["init"]);
    for i in 0..7 {
        fs::write(root.path().join(format!("change{i:02}.rs")), "").unwrap();
    }
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-m", "baseline"]);
    patch(root.path(), 7, 65);
    let mut pages = Vec::new();
    for offset in [0, 3, 6] {
        let offset = offset.to_string();
        pages.push(run(
            root.path(),
            &[
                "diff", "--patch", &path, "--offset", &offset, "--limit", "3", "--budget", "20000",
            ],
        ));
    }
    for i in 0..7 {
        let name = format!("## change{i:02}.rs");
        assert_eq!(pages.iter().filter(|page| page.contains(&name)).count(), 1);
    }
    let mut pages = Vec::new();
    for offset in [0, 30, 60] {
        let offset = offset.to_string();
        let output = run(
            root.path(),
            &[
                "diff",
                "--scope",
                "change00.rs",
                "--offset",
                &offset,
                "--limit",
                "30",
                "--budget",
                "20000",
            ],
        );
        assert!(output.contains("of 65"), "{output}");
        pages.push(output);
    }
    for i in 0..65 {
        let name = format!("## [+] symbol{i:03}");
        assert_eq!(
            pages.iter().filter(|page| page.contains(&name)).count(),
            1,
            "{name}"
        );
    }
    let filtered = run(
        root.path(),
        &["diff", "--search", "symbol064", "--limit", "1"],
    );
    assert!(
        filtered.contains("files 1-1 of 7")
            && filtered.contains("symbol064")
            && !filtered.contains("symbol063"),
        "{filtered}"
    );
    let before = run(
        root.path(),
        &["--offset", "1", "--limit", "1", "diff", "--patch", &path],
    );
    assert!(
        before.contains("files 2-2 of 7") && before.contains("change01.rs"),
        "{before}"
    );
    let empty = run(
        root.path(),
        &[
            "diff",
            "--scope",
            "change00.rs:symbol000",
            "--offset",
            "1",
            "--limit",
            "1",
        ],
    );
    assert!(empty.contains("symbols 0 of 1"), "{empty}");
    assert_safe_budget(&run(
        root.path(),
        &["diff", "--patch", &path, "--limit", "7", "--budget", "50"],
    ));
    fs::write(root.path().join("untouched.rs"), "fn untouched() {}\n").unwrap();
    let empty = run(
        root.path(),
        &[
            "diff",
            "--patch",
            &path,
            "--scope",
            "untouched.rs",
            "--limit",
            "1",
        ],
    );
    assert!(
        empty.contains("files 0 of 0") && !empty.contains("Next page:"),
        "{empty}"
    );
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn diff_log_pages_cover_commits_in_log_order() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init"]);
    for i in 0..7 {
        fs::write(root.path().join("file.rs"), format!("fn item{i}() {{}}\n")).unwrap();
        git(root.path(), &["add", "file.rs"]);
        git(root.path(), &["commit", "-m", &format!("checkpoint{i}")]);
    }
    let mut pages = Vec::new();
    for offset in [0, 2, 4] {
        let offset = offset.to_string();
        let output = run(
            root.path(),
            &[
                "diff",
                "--log",
                "HEAD~6..HEAD",
                "--offset",
                &offset,
                "--limit",
                "2",
            ],
        );
        assert!(output.contains("of 6"), "{output}");
        pages.push(output);
    }
    let joined = pages.join("\n");
    for i in 1..7 {
        assert_eq!(
            joined.matches(&format!("checkpoint{i}")).count(),
            1,
            "{joined}"
        );
    }
    assert!(
        pages[0].contains("checkpoint6") && pages[2].contains("checkpoint1"),
        "{joined}"
    );
}

struct Mcp {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    id: u64,
}
impl Mcp {
    fn new(root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .current_dir(root)
            .args(["--mcp", "--no-overview"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            input,
            output,
            id: 0,
        };
        client.rpc("initialize",json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}));
        client
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let result: Value = serde_json::from_str(&line).unwrap();
        assert!(result.get("error").is_none(), "{result}");
        result["result"].clone()
    }
    fn tool(&mut self, name: &str, args: Value) -> String {
        let result = self.rpc("tools/call", json!({"name":name,"arguments":args}));
        assert_ne!(result["isError"], true, "{result}");
        result["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_read_deps_and_diff_expose_and_apply_pages() {
    let root = tempfile::tempdir().unwrap();
    let path = patch(root.path(), 3, 4);
    fs::write(
        root.path().join("target.ts"),
        "import {value} from './a';\nimport {value as b} from './b';\n",
    )
    .unwrap();
    for name in ["a.ts", "b.ts"] {
        fs::write(root.path().join(name), "export const value=1;\n").unwrap();
    }
    let mut client = Mcp::new(root.path());
    let schema = client.rpc("tools/list", json!({}));
    for name in ["tilth_read", "tilth_deps", "tilth_diff"] {
        let tool = schema["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        for (key, min) in [("offset", 0), ("limit", 1)] {
            assert_eq!(tool["inputSchema"]["properties"][key]["minimum"], min);
        }
    }
    let grok = schema["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "tilth_grok")
        .unwrap();
    assert_eq!(grok["inputSchema"]["properties"]["offset"]["minimum"], 0);
    assert_eq!(
        grok["inputSchema"]["properties"]["section"]["enum"],
        json!(["callers", "callees", "siblings", "tests"])
    );
    for mode in ["auto", "signature", "full", "stripped"] {
        let output = client.tool(
            "tilth_read",
            json!({"root":root.path(),"path":"change00.rs","mode":mode,"offset":1,"limit":1}),
        );
        assert!(
            output.contains("2-2 of 4") && output.contains(r#""offset":2"#),
            "{output}"
        );
        assert!(!output.contains("ignored"), "{output}");
    }
    let deps = client.tool(
        "tilth_deps",
        json!({"root":root.path(),"path":"target.ts","offset":1,"limit":1}),
    );
    assert!(
        deps.contains("dependency records 2-2 of 2")
            && deps.contains("b.ts")
            && !deps.contains("a.ts"),
        "{deps}"
    );
    let diff = client.tool(
        "tilth_diff",
        json!({"root":root.path(),"patch":path,"offset":1,"limit":1}),
    );
    assert!(
        diff.contains("files 2-2 of 3")
            && diff.contains("change01.rs")
            && diff.contains(r#""offset":2"#),
        "{diff}"
    );
    let batch=client.tool("tilth_read",json!({"root":root.path(),"paths":["change00.rs","change01.rs"],"offset":1,"limit":1,"budget":50}));
    assert_safe_budget(&batch);
    for name in ["tilth_read", "tilth_deps", "tilth_diff"] {
        for (key, value) in [
            ("offset", json!(-1)),
            ("offset", json!(0.5)),
            ("offset", Value::Null),
            ("limit", json!(0)),
            ("limit", json!("1")),
        ] {
            let mut args = json!({"root":root.path(),"path":"target.ts","patch":"changes.patch"});
            args[key] = value;
            let result = client.rpc("tools/call", json!({"name":name,"arguments":args}));
            assert_eq!(result["isError"], true, "{result}");
        }
    }
}

#[test]
fn diff_symbol_and_unattributed_hunk_pages_share_source_order() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init"]);
    fs::write(
        root.path().join("mixed.rs"),
        "// old header\n\nfn body() { let value=1; }\n",
    )
    .unwrap();
    git(root.path(), &["add", "mixed.rs"]);
    git(root.path(), &["commit", "-m", "initial"]);
    fs::write(
        root.path().join("mixed.rs"),
        "// new header\n\nfn body() { let value=2; }\n",
    )
    .unwrap();
    let first = run(
        root.path(),
        &[
            "diff", "--scope", "mixed.rs", "--offset", "0", "--limit", "1",
        ],
    );
    let second = run(
        root.path(),
        &[
            "diff", "--scope", "mixed.rs", "--offset", "1", "--limit", "1",
        ],
    );
    assert!(
        first.contains("of 2") && first.contains("new header") && !first.contains("## [~] body"),
        "{first}"
    );
    assert!(
        second.contains("## [~] body") && !second.contains("new header"),
        "{second}"
    );
}
