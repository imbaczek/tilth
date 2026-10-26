use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
fn cli(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}
fn cli_json(root: &std::path::Path, args: &[&str]) -> Value {
    let output = cli(root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn cli_tags_round_trip_across_processes_without_changing_default_output() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
    let plain = cli(dir.path(), &["read", "a.rs"]);
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("view-tag:"));
    let first = cli_json(dir.path(), &["read", "a.rs", "--tag", "always", "--json"]);
    let tag = first["tag"].as_str().unwrap();
    assert_eq!(first["status"], "content");
    assert!(!first["output"].as_str().unwrap().contains("view-tag:"));
    let hit = cli_json(dir.path(), &["read", "a.rs", "--if-tag", tag, "--json"]);
    assert_eq!(hit["status"], "unchanged");
    assert!(hit.get("output").is_none());
    let text = cli(dir.path(), &["read", "a.rs", "--if-tag", tag]);
    assert!(text.status.success());
    assert_eq!(
        String::from_utf8(text.stdout).unwrap().trim(),
        "View unchanged."
    );
    let fresh = cli(dir.path(), &["read", "a.rs", "--tag", "always"]);
    assert!(String::from_utf8_lossy(&fresh.stdout).starts_with(&format!("view-tag: {tag}\n")));
    std::fs::write(dir.path().join("a.rs"), "fn bravo() {}\n").unwrap();
    let miss = cli_json(dir.path(), &["read", "a.rs", "--if-tag", tag, "--json"]);
    assert_eq!(miss["status"], "content");
    assert_ne!(miss["tag"], first["tag"]);
    assert!(miss["output"].as_str().unwrap().contains("bravo"));
    assert!(!cli(
        dir.path(),
        &["read", "a.rs", "--tag", "never", "--if-tag", tag]
    )
    .status
    .success());
    assert!(!cli(dir.path(), &["read", "a.rs", "--if-tag", "bad"])
        .status
        .success());
}

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    id: u64,
}
impl Server {
    fn new(root: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .args(["--mcp", "--no-overview", "--edit", "--scope"])
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut server = Self {
            child,
            input,
            output,
            id: 0,
        };
        server.request("initialize",json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}));
        server
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(self.output.read_line(&mut line).unwrap() > 0);
            let response: Value = serde_json::from_str(&line).unwrap();
            if response["id"] == self.id {
                assert!(response.get("error").is_none(), "{response}");
                return response["result"].clone();
            }
        }
    }
    fn read(&mut self, args: Value) -> Value {
        self.request("tools/call", json!({"name":"tilth_read","arguments":args}))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
fn mcp_structured_and_text_tags_validate_fresh_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.rs");
    std::fs::write(&path, "fn alpha() {}\n").unwrap();
    let mut server = Server::new(dir.path());
    let first = server.read(json!({"path":path,"tag":"always"}));
    let tag = first["structuredContent"]["tag"].as_str().unwrap();
    assert!(first["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with(&format!("view-tag: {tag}\n")));
    let hit = server.read(json!({"path":path,"if_tag":tag}));
    assert_ne!(hit["isError"], true);
    assert_eq!(hit["structuredContent"]["status"], "unchanged");
    assert_eq!(hit["content"][0]["text"], "View unchanged.");
    std::fs::write(&path, "fn bravo() {}\n").unwrap();
    let miss = server.read(json!({"path":path,"if_tag":tag}));
    assert_eq!(miss["structuredContent"]["status"], "content");
    assert_ne!(miss["structuredContent"]["tag"], tag);
    let partial = server
        .read(json!({"root":dir.path(),"paths":["a.rs","missing.rs"],"if_tag":tag,"budget":40}));
    assert!(partial.get("structuredContent").is_none());
    let failed = server.read(json!({"path":dir.path().join("missing.rs"),"if_tag":tag}));
    assert_eq!(failed["isError"], true);
    assert!(failed.get("structuredContent").is_none());
    let invalid = server.read(json!({"path":path,"tag":true}));
    assert_eq!(invalid["isError"], true);
}
