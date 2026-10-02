use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "-q"])
        .arg(root.path())
        .status()
        .unwrap();
    assert!(status.success());
    fs::write(root.path().join(".gitignore"), "git_hidden.rs\n").unwrap();
    fs::write(root.path().join(".ignore"), "ignore_hidden.rs\n").unwrap();
    fs::write(root.path().join(".git/info/exclude"), "exclude_hidden.rs\n").unwrap();
    fs::write(root.path().join(".tilthignore"), "tilth_hidden.rs\n").unwrap();
    fs::create_dir(root.path().join("node_modules")).unwrap();
    for name in [
        "visible.rs",
        "git_hidden.rs",
        "ignore_hidden.rs",
        "exclude_hidden.rs",
        "tilth_hidden.rs",
        "node_modules/junk.rs",
    ] {
        fs::write(
            root.path().join(name),
            "// ignore marker\nfn policy_symbol() { common_fn(); }\n",
        )
        .unwrap();
    }
    root
}

fn assert_visibility(output: &str, respect: bool) {
    assert!(output.contains("visible.rs"), "{output}");
    for name in ["git_hidden.rs", "ignore_hidden.rs", "exclude_hidden.rs"] {
        assert_eq!(output.contains(name), !respect, "{name}: {output}");
    }
    assert!(!output.contains("tilth_hidden.rs"), "{output}");
    assert!(!output.contains("junk.rs"), "{output}");
}

#[test]
fn cli_defaults_and_overrides_apply_to_filesystem_walks() {
    let root = fixture();
    for (query, mode) in [
        ("*.rs", None),
        ("ignore marker", None),
        ("policy_symbol", None),
        ("common_fn", Some("--callers")),
        ("", Some("--map")),
    ] {
        for (env, flag, respect) in [
            (None, None, true),
            (Some("0"), None, false),
            (Some("false"), None, false),
            (Some("1"), Some("--no-respect-gitignore"), false),
            (Some("0"), Some("--respect-gitignore"), true),
        ] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_tilth"));
            command.env_remove("TILTH_RESPECT_GITIGNORE");
            if let Some(value) = env {
                command.env("TILTH_RESPECT_GITIGNORE", value);
            }
            if let Some(flag) = flag {
                command.arg(flag);
            }
            if !query.is_empty() {
                command.arg(query);
            }
            if let Some(mode) = mode {
                command.arg(mode);
            }
            let output = command.arg("--scope").arg(root.path()).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_visibility(&String::from_utf8(output.stdout).unwrap(), respect);
        }
    }
    // Explicit search accepts the disable flag on either side of the verb.
    for args in [
        vec!["--no-respect-gitignore", "search", "*.rs"],
        vec!["search", "*.rs", "--no-respect-gitignore"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .env_remove("TILTH_RESPECT_GITIGNORE")
            .args(args)
            .arg("--scope")
            .arg(root.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_visibility(&String::from_utf8(output.stdout).unwrap(), false);
    }
    for args in [
        vec!["--respect-gitignore", "--no-respect-gitignore", "*.rs"],
        vec![
            "--respect-gitignore",
            "search",
            "*.rs",
            "--no-respect-gitignore",
        ],
        vec![
            "--no-respect-gitignore",
            "search",
            "*.rs",
            "--respect-gitignore",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tilth"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{error}");
        assert!(
            error.contains("--respect-gitignore") && error.contains("--no-respect-gitignore"),
            "{error}"
        );
    }
}

struct Mcp {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    id: u64,
}

impl Mcp {
    fn start(root: &std::path::Path, env: Option<&str>, flag: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tilth"));
        command
            .args(["--mcp", "--no-overview"])
            .current_dir(root)
            .env_remove("TILTH_RESPECT_GITIGNORE");
        if let Some(value) = env {
            command.env("TILTH_RESPECT_GITIGNORE", value);
        }
        if let Some(flag) = flag {
            command.arg(flag);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            id: 0,
        }
    }

    fn call(&mut self, name: &str, args: serde_json::Value) -> String {
        self.id += 1;
        writeln!(self.input, "{}", serde_json::json!({"jsonrpc":"2.0", "id":self.id, "method":"tools/call", "params":{"name":name,"arguments":args}})).unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        reply["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn config(&mut self, args: serde_json::Value, respect: bool, source: &str) {
        let result: serde_json::Value =
            serde_json::from_str(&self.call("tilth_config", args)).unwrap();
        assert_eq!(result["respect_gitignore"], respect);
        assert_eq!(result["source"], source);
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_default_runtime_call_and_startup_overrides() {
    let root = fixture();
    let mut mcp = Mcp::start(root.path(), None, None);
    mcp.config(serde_json::json!({}), true, "default");
    for (name, args) in [
        ("tilth_list", serde_json::json!({"patterns":["*.rs"]})),
        (
            "tilth_search",
            serde_json::json!({"query":"ignore marker", "kind":"content"}),
        ),
        ("tilth_search", serde_json::json!({"query":"policy_symbol"})),
        (
            "tilth_search",
            serde_json::json!({"query":"common_fn", "kind":"callers"}),
        ),
    ] {
        assert_visibility(&mcp.call(name, args.clone()), true);
        let mut bypass = args.clone();
        bypass["gitignore"] = false.into();
        assert_visibility(&mcp.call(name, bypass), false);
        assert_visibility(&mcp.call(name, args), true);
    }
    mcp.config(
        serde_json::json!({"action":"set","respect_gitignore":false}),
        false,
        "runtime",
    );
    assert_visibility(
        &mcp.call("tilth_list", serde_json::json!({"patterns":["*.rs"]})),
        false,
    );
    assert_visibility(
        &mcp.call(
            "tilth_list",
            serde_json::json!({"patterns":["*.rs"],"gitignore":true}),
        ),
        true,
    );
    mcp.config(serde_json::json!({}), false, "runtime");
    mcp.config(serde_json::json!({"action":"reset"}), true, "default");
    assert_visibility(
        &mcp.call("tilth_list", serde_json::json!({"patterns":["*.rs"]})),
        true,
    );
    for (env, flag, startup) in [
        ("0", None, false),
        ("false", None, false),
        ("1", Some("--no-respect-gitignore"), false),
        ("0", Some("--respect-gitignore"), true),
    ] {
        let mut mcp = Mcp::start(root.path(), Some(env), flag);
        mcp.config(serde_json::json!({}), startup, "environment");
        assert_visibility(
            &mcp.call("tilth_list", serde_json::json!({"patterns":["*.rs"]})),
            startup,
        );
        mcp.config(
            serde_json::json!({"action":"set","respect_gitignore":!startup}),
            !startup,
            "runtime",
        );
        mcp.config(
            serde_json::json!({"action":"reset"}),
            startup,
            "environment",
        );
    }
}
