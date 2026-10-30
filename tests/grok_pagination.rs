use serde_json::{json, Value};
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let mut source = String::from("pub fn needle() {\n");
    for i in 0..75 {
        writeln!(source, "local{i:03}();").unwrap();
    }
    for i in 0..40 {
        writeln!(source, "foreign{i:03}();").unwrap();
    }
    source.push_str("needle();\n}\n");
    for i in 0..75 {
        writeln!(source, "fn local{i:03}() {{}}").unwrap();
    }
    for i in 0..65 {
        writeln!(source, "fn sibling{i:03}() {{}}").unwrap();
    }
    fs::write(root.path().join("target.rs"), source).unwrap();
    let mut callers = String::new();
    for i in 0..130 {
        writeln!(callers, "fn caller{i:03}() {{ needle(); }}").unwrap();
    }
    fs::write(root.path().join("callers.rs"), callers).unwrap();
    fs::create_dir(root.path().join("tests")).unwrap();
    let mut tests = String::new();
    for i in 0..70 {
        writeln!(tests, "#[test]\nfn test{i:03}() {{ needle(); }}").unwrap();
    }
    fs::write(root.path().join("tests/cases.rs"), tests).unwrap();
    root
}

fn output(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}
fn run(root: &Path, args: &[&str]) -> String {
    let output = output(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn total(section: &str) -> usize {
    match section {
        "callers" => 130,
        "tests" => 70,
        "callees" => 115,
        "siblings" => 140,
        _ => unreachable!(),
    }
}

#[test]
fn cli_pages_cover_each_section_beyond_old_caps_without_overlap() {
    let root = fixture();
    for section in ["callers", "tests", "callees", "siblings"] {
        let mut pages = Vec::new();
        for offset in (0..total(section)).step_by(40) {
            let offset = offset.to_string();
            let args = [
                "grok",
                "target.rs:1",
                "--section",
                section,
                "--offset",
                &offset,
                "--limit",
                "40",
                "--budget",
                "20000",
            ];
            let page = run(root.path(), &args);
            assert!(page.contains(&format!("of {}", total(section))), "{page}");
            assert_eq!(page, run(root.path(), &args));
            assert!(
                !page.contains("## body") && !page.contains("## delegate body"),
                "{page}"
            );
            pages.push(page);
        }
        let labels: Vec<_> = match section {
            "callers" => (0..130).map(|i| format!("caller{i:03}")).collect(),
            "tests" => (0..70).map(|i| format!("test{i:03}")).collect(),
            "callees" => (0..75)
                .map(|i| format!("local{i:03}"))
                .chain((0..40).map(|i| format!("foreign{i:03}")))
                .collect(),
            "siblings" => (0..75)
                .map(|i| format!("local{i:03}"))
                .chain((0..65).map(|i| format!("sibling{i:03}")))
                .collect(),
            _ => unreachable!(),
        };
        for label in labels {
            assert_eq!(
                pages.iter().filter(|page| page.contains(&label)).count(),
                1,
                "{section}: {label}"
            );
        }
        assert!(!pages.last().unwrap().contains("Next page:"));
        let default = run(
            root.path(),
            &["grok", "target.rs:1", "--section", section, "--full"],
        );
        assert!(
            default.contains(&format!("Showing {section} 1-50 of {}", total(section))),
            "{default}"
        );
        let explicit = run(
            root.path(),
            &[
                "grok",
                "target.rs:1",
                "--section",
                section,
                "--full",
                "--limit",
                "65",
            ],
        );
        assert!(
            explicit.contains(&format!("Showing {section} 1-65 of {}", total(section))),
            "{explicit}"
        );
        let beyond = run(
            root.path(),
            &[
                "grok",
                "target.rs:1",
                "--section",
                section,
                "--offset",
                &usize::MAX.to_string(),
                "--limit",
                "1",
            ],
        );
        assert!(
            beyond.contains(&format!("Showing {section} 0 of {}", total(section)))
                && !beyond.contains("Next page:"),
            "{beyond}"
        );
        let tight = run(
            root.path(),
            &[
                "grok",
                "target.rs:1",
                "--section",
                section,
                "--limit",
                "100",
                "--budget",
                "100",
            ],
        );
        assert!(
            !tight.contains("Next page:") && !tight.contains("End of listing."),
            "{tight}"
        );
        assert!(
            tight.contains("retry the same offset with a smaller limit"),
            "{tight}"
        );
    }
}

#[test]
fn cli_merges_section_flags_and_rejects_paging_without_a_section() {
    let root = fixture();
    let before = run(
        root.path(),
        &[
            "--section",
            "callers",
            "--offset",
            "40",
            "--limit",
            "40",
            "grok",
            "target.rs:1",
        ],
    );
    assert!(before.contains("Showing callers 41-80 of 130"), "{before}");
    let override_after = run(
        root.path(),
        &[
            "--section",
            "callers",
            "--offset",
            "40",
            "--limit",
            "40",
            "grok",
            "target.rs:1",
            "--section",
            "tests",
            "--offset",
            "0",
            "--limit",
            "2",
        ],
    );
    assert!(
        override_after.contains("Showing tests 1-2 of 70"),
        "{override_after}"
    );
    for args in [
        vec!["grok", "target.rs:1", "--offset", "0"],
        vec!["--limit", "1", "grok", "target.rs:1"],
        vec!["grok", "target.rs:1", "--section", "body"],
        vec![
            "grok",
            "target.rs:1",
            "--section",
            "callers",
            "--limit",
            "0",
        ],
    ] {
        assert!(!output(root.path(), &args).status.success(), "{args:?}");
    }
    let overview = run(root.path(), &["grok", "target.rs:1"]);
    assert!(
        overview.contains("## body")
            && overview.contains("## callees")
            && overview.contains("## callers")
            && overview.contains("## siblings")
            && overview.contains("## tests"),
        "{overview}"
    );
    assert!(!overview.contains("Showing callers"), "{overview}");
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
    fn tool(&mut self, args: Value) -> String {
        let result = self.rpc("tools/call", json!({"name":"tilth_grok","arguments":args}));
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
fn continuation(output: &str) -> Value {
    let line = output
        .lines()
        .find_map(|line| line.strip_prefix("> Next page: "))
        .unwrap();
    serde_json::from_str(line.split_once(" (keep").unwrap().0).unwrap()
}

#[test]
fn mcp_section_continuations_and_runtime_validation_work() {
    let root = fixture();
    let mut client = Mcp::new(root.path());
    let tools = client.rpc("tools/list", json!({}));
    let tool = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "tilth_grok")
        .unwrap();
    assert_eq!(
        tool["inputSchema"]["properties"]["section"]["enum"],
        json!(["callers", "callees", "siblings", "tests"])
    );
    for section in ["callers", "callees", "siblings", "tests"] {
        let args = json!({"root":root.path(),"target":"target.rs:1","section":section,"limit":3});
        let first = client.tool(args.clone());
        assert!(
            first.contains(&format!("Showing {section} 1-3 of {}", total(section))),
            "{first}"
        );
        let next = continuation(&first);
        assert_eq!(next["section"], section);
        assert_eq!(next["target"], "target.rs:1");
        assert_eq!(next["offset"], 3);
        let mut args = args;
        args.as_object_mut()
            .unwrap()
            .extend(next.as_object().unwrap().clone());
        let second = client.tool(args);
        assert!(
            second.contains(&format!("Showing {section} 4-6 of {}", total(section))),
            "{second}"
        );
        assert!(!first.contains("## body") && !second.contains("## body"));
    }
    // Section calls above must not consume the initial source-body expansion.
    let first = client.tool(json!({"root":root.path(),"target":"target.rs:1"}));
    assert!(
        first.contains("## body") && !first.contains("shown earlier"),
        "{first}"
    );
    let repeat = client.tool(json!({"root":root.path(),"target":"target.rs:1"}));
    assert!(repeat.contains("shown earlier"), "{repeat}");
    for (key, value) in [
        ("section", Value::Null),
        ("section", json!(7)),
        ("section", json!("body")),
        ("offset", json!(0.5)),
        ("offset", Value::Null),
        ("limit", json!(0)),
        ("limit", json!("2")),
        ("limit", Value::Null),
        ("budget", json!(-1)),
        ("budget", Value::Null),
    ] {
        let mut args = json!({"root":root.path(),"target":"target.rs:1","section":"callers"});
        args[key] = value;
        let result = client.rpc("tools/call", json!({"name":"tilth_grok","arguments":args}));
        assert_eq!(result["isError"], true, "{result}");
    }
    for field in ["offset", "limit"] {
        let mut args = json!({"root":root.path(),"target":"target.rs:1"});
        args[field] = json!(1);
        let result = client.rpc("tools/call", json!({"name":"tilth_grok","arguments":args}));
        assert_eq!(result["isError"], true, "{result}");
    }
}

#[test]
fn qualified_same_line_methods_keep_their_owner_across_sections_and_continuations() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("same.ts"),"export class First { foo() { first_local(); } first_peer() {} } export class Second { foo() { second_local_a(); second_local_b(); } second_peer_a() {} second_peer_b() {} }\nfunction first_local() {}\nfunction second_local_a() {}\nfunction second_local_b() {}\n").unwrap();
    fs::write(root.path().join("uses.ts"),"import { First, Second } from './same';\nfunction second_a(value: Second) { value.foo(); }\nfunction second_b(value: Second) { value.foo(); }\nfunction first(value: First) { value.foo(); }\n").unwrap();
    let mut client = Mcp::new(root.path());
    for (section, labels) in [
        ("callers", ["second_a", "second_b"]),
        ("callees", ["second_local_a", "second_local_b"]),
        ("siblings", ["second_peer_a", "second_peer_b"]),
    ] {
        let mut args =
            json!({"root":root.path(),"target":"Second.foo","section":section,"limit":1});
        let first = client.tool(args.clone());
        assert!(
            first.contains("of 2")
                && !first.contains("first_local")
                && !first.contains("first_peer"),
            "{first}"
        );
        let next = continuation(&first);
        assert_eq!(next["target"], "Second.foo");
        args.as_object_mut()
            .unwrap()
            .extend(next.as_object().unwrap().clone());
        let second = client.tool(args);
        assert!(
            first.contains(labels[0]) && second.contains(labels[1]),
            "{first}\n{second}"
        );
        assert!(!second.contains("Next page:"));
    }
}

#[test]
fn empty_sections_and_large_explicit_budgets_have_valid_terminal_pages() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("target.rs"), "fn needle() {}\n").unwrap();
    let mut client = Mcp::new(root.path());
    for section in ["callers", "callees", "siblings", "tests"] {
        let page=client.tool(json!({"root":root.path(),"target":"target.rs:1","section":section,"offset":usize::MAX,"limit":1}));
        assert!(
            page.contains(&format!("Showing {section} 0 of 0")) && !page.contains("Next page:"),
            "{page}"
        );
    }
    let mut source = String::new();
    for i in 0..2000 {
        writeln!(
            source,
            "fn a_long_distinct_caller_function_name_{i:04}() {{ needle(); }}"
        )
        .unwrap();
    }
    fs::write(root.path().join("callers.rs"), source).unwrap();
    let page=client.tool(json!({"root":root.path(),"target":"target.rs:1","section":"callers","limit":2000,"budget":50000}));
    assert!(
        page.len() > 24_000 * 4,
        "large budget regression needs a response beyond the default budget: {}",
        page.len()
    );
    assert!(
        page.contains("a_long_distinct_caller_function_name_1999") && !page.contains("Next page:"),
        "{page}"
    );
    assert!(!page.contains("Page or navigation omitted"));
}

#[test]
fn sibling_pages_preserve_same_line_source_order_and_full_nested_owner() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("nested.ts"),"class Left { make() { class Inner { foo() { left(); } left_peer() {} } } } class Right { make() { class Inner { foo() { right_z(); right_a(); } zebra() {} alpha() {} } } }\nfunction left() {}\nfunction right_z() {}\nfunction right_a() {}\n").unwrap();
    let mut client = Mcp::new(root.path());
    for (section, labels) in [
        ("siblings", ["zebra", "alpha"]),
        ("callees", ["right_z", "right_a"]),
    ] {
        let mut args =
            json!({"root":root.path(),"target":"Right.Inner.foo","section":section,"limit":1});
        let first = client.tool(args.clone());
        assert!(
            first.contains("of 2") && first.contains(labels[0]) && !first.contains("left_peer"),
            "{first}"
        );
        let next = continuation(&first);
        assert_eq!(next["target"], "Right.Inner.foo");
        args.as_object_mut()
            .unwrap()
            .extend(next.as_object().unwrap().clone());
        let second = client.tool(args);
        assert!(
            second.contains(labels[1]) && !second.contains("Next page:"),
            "{second}"
        );
    }
    fs::write(
        root.path().join("plain.rs"),
        "fn target() {} fn zebra() {} fn alpha() {}\n",
    )
    .unwrap();
    let first =
        client.tool(json!({"root":root.path(),"target":"target","section":"siblings","limit":1}));
    assert!(
        first.contains("zebra") && !first.contains("alpha"),
        "{first}"
    );
}

#[test]
fn semantic_module_and_package_qualifiers_work_without_ast_module_ancestors() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("lib.rs"), "mod api;\n").unwrap();
    fs::write(root.path().join("api.rs"),"pub struct Type;\nimpl Type {\n fn method(&self) { local_a(); local_b(); }\n fn zebra(&self) {}\n fn alpha(&self) {}\n}\nfn local_a() {}\nfn local_b() {}\n").unwrap();
    let mut client = Mcp::new(root.path());
    let siblings = client.tool(
        json!({"root":root.path(),"target":"api::Type::method","section":"siblings","limit":1}),
    );
    assert!(
        siblings.contains("of 2") && siblings.contains("zebra") && !siblings.contains("alpha"),
        "{siblings}"
    );
    let callees = client.tool(
        json!({"root":root.path(),"target":"api::Type::method","section":"callees","limit":2}),
    );
    assert!(
        callees.contains("of 2") && callees.contains("local_a") && callees.contains("local_b"),
        "{callees}"
    );
    fs::write(root.path().join("types.go"),"package api\ntype Item struct{}\nfunc (item Item) Method() {}\nfunc (item Item) Zebra() {}\nfunc (item Item) Alpha() {}\n").unwrap();
    let siblings = client.tool(
        json!({"root":root.path(),"target":"api.Item.Method","section":"siblings","limit":1}),
    );
    assert!(
        siblings.contains("of 2") && siblings.contains("Zebra") && !siblings.contains("Alpha"),
        "{siblings}"
    );
    fs::write(
        root.path().join("forms.lisp"),
        "(defun pkg:hello-world () 1)\n(defun pkg:zebra () 2)\n(defun pkg:alpha () 3)\n",
    )
    .unwrap();
    let siblings = client.tool(
        json!({"root":root.path(),"target":"pkg:hello-world","section":"siblings","limit":1}),
    );
    assert!(
        siblings.contains("of 2") && siblings.to_lowercase().contains("zebra"),
        "{siblings}"
    );
}
