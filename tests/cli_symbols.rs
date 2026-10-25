use std::process::Command;
fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .arg("read")
        .args(args)
        .output()
        .unwrap()
}
fn text(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
#[test]
fn repeated_symbols_preserve_order_and_fetch_only_requested_bodies() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn alpha() { let alpha_body = 1; } fn beta() { let beta_body = 2; }\nfn unrelated() {}\n",
    )
    .unwrap();
    let out = text(run(
        dir.path(),
        &[
            "a.rs", "--symbol", "beta", "--symbol", "alpha", "--symbol", "missing",
        ],
    ));
    assert!(
        out.find("- beta:").unwrap() < out.find("- alpha:").unwrap(),
        "{out}"
    );
    assert!(
        out.contains("alpha_body")
            && out.contains("beta_body")
            && out.contains("missing: not found"),
        "{out}"
    );
    assert!(!out.contains("unrelated"), "{out}");
    let out = text(run(dir.path(), &["a.rs", "--symbol", "beta"]));
    assert!(!out.contains("alpha_body"), "{out}");
}
#[test]
fn qualified_symbols_disambiguate_and_multiple_files_do_not_pair_positionally() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"),"struct A;\nimpl A {\n fn same() { let a_body=1; }\n}\nstruct B;\nimpl B {\n fn same() { let b_body=2; }\n}\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "fn extra() {}\n").unwrap();
    let out = text(run(dir.path(), &["a.rs", "--symbol", "same"]));
    assert!(
        out.contains("ambiguous (2 definitions)") && !out.contains("a_body"),
        "{out}"
    );
    let out = text(run(
        dir.path(),
        &["a.rs", "b.rs", "--symbol", "A::same", "--symbol", "extra"],
    ));
    assert!(
        out.contains("a_body") && out.contains("fn extra()") && !out.contains("b_body"),
        "{out}"
    );
    let out = text(run(
        dir.path(),
        &["a.rs", "--symbol", "same@7", "--mode", "signature"],
    ));
    assert!(
        out.contains("fn same()") && !out.contains("b_body"),
        "{out}"
    );
}
#[test]
fn lisp_reader_case_commas_and_method_ambiguity() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.lisp"),"(defun |a,b| () 42)\n(defun mixed () 17)\n(defmethod method ((x number)) x)\n(defmethod method ((x string)) x)\n").unwrap();
    let out = text(run(
        dir.path(),
        &[
            "a.lisp", "--symbol", "|a,b|", "--symbol", "MIXED", "--symbol", "method",
        ],
    ));
    assert!(
        out.contains("42") && out.contains("17") && out.contains("ambiguous (2 definitions)"),
        "{out}"
    );
}
#[test]
fn invalid_combinations_and_shared_budget() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        format!(
            "fn alpha() {{\n{}\n}}\nfn beta() {{}}\n",
            " let value=1;\n".repeat(300)
        ),
    )
    .unwrap();
    for args in [
        vec!["a.rs", "--symbol", "alpha", "--section", "1-2"],
        vec!["a.rs", "--symbol", "alpha", "--mode", "stripped"],
        vec!["a.rs", "--symbol", "alpha", "--offset", "0"],
    ] {
        assert!(!run(dir.path(), &args).status.success(), "{args:?}");
    }
    let out = text(run(
        dir.path(),
        &[
            "a.rs", "--symbol", "alpha", "--symbol", "beta", "--budget", "150",
        ],
    ));
    assert!(out.len() <= 700, "{} bytes", out.len());
    assert!(
        out.contains("omitted") || out.contains("truncated"),
        "{out}"
    );
}

#[test]
fn same_line_qualified_methods_and_go_grouped_names_keep_exact_spans() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"),"struct A; impl A { fn same() { let only_a=1; } } struct B; impl B { fn same() { let only_b=2; } }\n").unwrap();
    let out = text(run(dir.path(), &["a.rs", "--symbol", "A::same"]));
    assert!(out.contains("only_a") && !out.contains("only_b"), "{out}");
    assert!(!run(
        dir.path(),
        &[
            "a.rs",
            "--symbol",
            "A::same",
            "--mode",
            "signature",
            "--edit"
        ]
    )
    .status
    .success());
}
#[test]
fn go_grouped_declarations_resolve_each_name_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.go"), "package sample\nvar a, b int\n").unwrap();
    let out = text(run(dir.path(), &["a.go", "--symbol", "a", "--symbol", "b"]));
    assert!(
        out.contains("- a:") && out.contains("- b:") && out.contains("var a, b int"),
        "{out}"
    );
    assert!(
        !out.contains("not found") && !out.contains("unavailable"),
        "{out}"
    );
}
