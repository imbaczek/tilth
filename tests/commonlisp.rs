use std::fs;
use std::process::Command;

fn run(args: &[&str], scope: &std::path::Path) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tilth"));
    command.args(args).current_dir(scope);
    if args.first() != Some(&"read") {
        command.arg("--scope").arg(scope);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn commonlisp_cli_reader_symbols_are_definitions_with_usages() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("forms.lisp"), "'(quoted-data)\n(DEFUN HELLO-WORLD (x)\n  x)\n(defvar *state* nil)\n(defconstant +size+ 5)\n(defun pkg:hello (x)\n  x)\n(defun use-them ()\n  (HELLO-world *STATE*)\n  (pkg:HELLO +SIZE+))\n").unwrap();
    for query in [
        "hello-world",
        "HELLO-WORLD",
        "*state*",
        "+size+",
        "PKG:HELLO",
    ] {
        let output = run(&[query], root.path());
        assert!(output.contains("[definition]"), "{query}: {output}");
        assert!(output.contains("[usage"), "{query}: {output}");
        let grok = run(&["grok", query], root.path());
        assert!(grok.contains("## signature"), "{query}: {grok}");
        assert!(
            !grok.contains("missing from the outline"),
            "{query}: {grok}"
        );
    }
}

#[test]
fn commonlisp_symbol_probe_preserves_files_globs_and_other_languages() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("forms.lisp"), "(defvar *state* nil)\n").unwrap();
    fs::write(root.path().join("notes-state.txt"), "notes\n").unwrap();
    let symbol = run(&["*state*"], root.path());
    assert!(symbol.contains("[definition]"), "{symbol}");
    fs::write(
        root.path().join("*state*"),
        "literal filename takes priority\n",
    )
    .unwrap();
    let file = run(&["*state*"], root.path());
    assert!(
        file.contains("literal filename takes priority") && !file.contains("[definition]"),
        "{file}"
    );
    fs::write(root.path().join("notes-state.txt"), "notes\n").unwrap();
    let glob = run(&["*state.txt"], root.path());
    assert!(
        glob.contains("notes-state.txt") && !glob.contains("[definition]"),
        "{glob}"
    );
    fs::write(root.path().join("lib.rs"), "fn HELLO_WORLD() {}\n").unwrap();
    let rust = run(&["hello_world"], root.path());
    assert!(!rust.contains("[definition]"), "{rust}");
}

#[test]
fn commonlisp_signatures_and_docstrings_reach_cli_and_grok() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sample.lisp");
    fs::write(&path, "(defmacro with-value\n    ((name value) &body body)\n  \"Bind NAME around BODY.\"\n  `(let ((,name ,value)) ,@body))\n").unwrap();
    let signature = run(
        &["read", path.to_str().unwrap(), "--mode", "signature"],
        root.path(),
    );
    assert!(signature.contains("&body body"), "{signature}");
    assert!(
        !signature.contains("Bind NAME") && !signature.contains("`(let"),
        "{signature}"
    );
    let grok = run(&["grok", "with-value"], root.path());
    assert!(grok.contains("Bind NAME around BODY."), "{grok}");
}

#[test]
fn commonlisp_does_not_override_qualified_rust_methods() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("forms.lisp"),
        "(defun Service::hello () t)\n",
    )
    .unwrap();
    fs::write(
        root.path().join("lib.rs"),
        "struct Service;\nimpl Service { fn hello() {} }\n",
    )
    .unwrap();
    let grok = run(&["grok", "Service::hello"], root.path());
    assert!(grok.contains("lib.rs:"), "{grok}");
}
