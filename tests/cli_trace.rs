use std::process::Command;
fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tilth"))
        .current_dir(root)
        .arg("trace")
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
fn direct_chain_reverse_chain_cycles_and_depth_have_precise_claims() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn start() { middle(); }\nfn middle() { end(); }\nfn end() { start(); }\n",
    )
    .unwrap();
    let out = text(run(dir.path(), &["start", "--to", "end", "--depth", "2"]));
    assert!(
        out.contains("Path found")
            && out.contains("call site a.rs:1")
            && out.contains("call site a.rs:2"),
        "{out}"
    );
    assert!(!out.contains("Results may be incomplete"), "{out}");
    let out = text(run(
        dir.path(),
        &[
            "end",
            "--to",
            "start",
            "--direction",
            "callers",
            "--depth",
            "2",
        ],
    ));
    assert!(out.contains("Path found"), "{out}");
    let out = text(run(dir.path(), &["start", "--to", "end", "--depth", "1"]));
    assert!(
        out.contains("No path found within depth 1") && !out.contains("Results may be incomplete"),
        "{out}"
    );
    let out = text(run(dir.path(), &["start", "--depth", "16"]));
    assert_eq!(out.matches("call site").count(), 3, "{out}");
}
#[test]
fn duplicate_names_do_not_fabricate_paths_and_missing_scopes_do_not_widen() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn start() { duplicate(); }\nfn duplicate() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("b.rs"), "fn duplicate() {}\n").unwrap();
    let out = text(run(dir.path(), &["start"]));
    assert!(
        out.contains("ambiguous call targets")
            && out.contains("duplicate")
            && !out.contains("call site"),
        "{out}"
    );
    let out = text(run(dir.path(), &["duplicate"]));
    assert!(out.contains("ambiguous (2 declarations)"), "{out}");
    assert!(!run(dir.path(), &["start", "--scope", "missing"])
        .status
        .success());
}
#[test]
fn lisp_direct_calls_quotes_local_bindings_and_dynamic_calls() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.lisp"),"(defun start () (MIDDLE))\n(defun middle () (|end,name|))\n(defun |end,name| () 42)\n(defun quoted () '(middle) #'middle (quote (middle)))\n(defun local () (flet ((middle () 1)) (middle)))\n(defun dynamic (fn) (funcall fn))\n").unwrap();
    let out = text(run(
        dir.path(),
        &["START", "--to", "|end,name|", "--depth", "2"],
    ));
    assert!(out.contains("Path found"), "{out}");
    let out = text(run(dir.path(), &["quoted"]));
    assert!(!out.contains("call site"), "{out}");
    let out = text(run(dir.path(), &["local"]));
    assert!(
        out.contains("lexically bound") && !out.contains("call site"),
        "{out}"
    );
    let out = text(run(dir.path(), &["dynamic"]));
    assert!(out.contains("dynamic Lisp invocation"), "{out}");
}
#[test]
fn generated_and_generic_lisp_calls_report_their_limits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.lisp"),"(defmacro make-generated () `(defun generated () 1))\n(defun start () (generated))\n(defgeneric dispatch (x))\n(defmethod dispatch ((x number)) x)\n(defun caller (x) (dispatch x))\n").unwrap();
    let out = text(run(dir.path(), &["start"]));
    assert!(
        out.contains("unresolved calls")
            && out.contains("generated")
            && out.contains("Results may be incomplete"),
        "{out}"
    );
    let out = text(run(dir.path(), &["caller"]));
    assert!(
        out.contains("ambiguous call targets") && out.contains("dispatch"),
        "{out}"
    );
}
#[test]
fn output_budget_json_and_validation() {
    let dir = tempfile::tempdir().unwrap();
    let source = format!(
        "fn start() {{ {} }}\n{}",
        (0..20)
            .map(|i| format!("target{i}();"))
            .collect::<Vec<_>>()
            .join(" "),
        (0..20)
            .map(|i| format!("fn target{i}() {{}}\n"))
            .collect::<String>()
    );
    std::fs::write(dir.path().join("a.rs"), source).unwrap();
    let out = text(run(dir.path(), &["start", "--budget", "100"]));
    assert!(
        out.contains("Output incomplete") && out.len() < 500,
        "{out}"
    );
    let out = run(dir.path(), &["start", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value["output"]
        .as_str()
        .unwrap()
        .contains("Static syntactic calls"));
    for args in [
        vec!["start", "--depth", "0"],
        vec!["start", "--depth", "17"],
        vec!["start", "--direction", "bad"],
    ] {
        assert!(!run(dir.path(), &args).status.success(), "{args:?}");
    }
}

#[test]
fn rust_namespaces_callbacks_and_macro_names_do_not_fabricate_function_paths() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"),"mod a { fn target() {} } mod b { fn start() { target(); } }\nfn callback() {}\nfn via_callback(callback: fn()) { callback(); }\nmacro_rules! same { () => {} }\nfn same() {}\nfn via_macro() { same!(); }\n").unwrap();
    for (from, to, reason) in [
        ("b::start", "a::target", "namespace"),
        ("via_callback", "callback", "lexically bound Rust"),
        ("via_macro", "same", "macro invocation"),
    ] {
        let out = text(run(dir.path(), &[from, "--to", to]));
        assert!(
            out.contains("No path found") && out.contains(reason) && !out.contains("call site"),
            "{out}"
        );
    }
}
#[test]
fn lisp_binding_lists_are_not_function_calls() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.lisp"),"(defun item () 1)\n(defun consume (x) x)\n(defun start (items) (dolist (item items) (consume item)))\n(defun values-test () (multiple-value-bind (item) (values 1) (consume item)))\n").unwrap();
    for from in ["start", "values-test"] {
        let out = text(run(dir.path(), &[from, "--to", "item"]));
        assert!(out.contains("No path found"), "{out}");
        assert!(!out.contains(" -> item "), "{out}");
    }
}

#[test]
fn rust_qualified_paths_and_pattern_shadowing_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"),"mod a { fn start() { self::end(); } fn end() {} }\nfn outer() { crate::a::end(); }\nfn callback() {}\nfn pattern() { if let Some(callback)=None::<fn()> { callback(); } }\n").unwrap();
    for from in ["a::start", "outer"] {
        let out = text(run(dir.path(), &[from, "--to", "a::end"]));
        assert!(out.contains("Path found"), "{out}");
    }
    let out = text(run(dir.path(), &["pattern", "--to", "callback"]));
    assert!(
        out.contains("No path found") && out.contains("lexically bound Rust"),
        "{out}"
    );
}
#[test]
fn lisp_package_names_and_reader_conditionals_are_not_silent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.lisp"),"(defun pkg:quote () 1)\n(defun start () (pkg:quote))\n(defun target () 2)\n(defun conditional () #+sbcl (target))\n").unwrap();
    let out = text(run(dir.path(), &["start", "--to", "pkg:quote"]));
    assert!(out.contains("Path found"), "{out}");
    let out = text(run(dir.path(), &["conditional", "--to", "target"]));
    assert!(out.contains("reader-conditional"), "{out}");
}

#[test]
fn rust_generic_calls_and_unsupported_dynamic_syntax_are_accounted_for() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn target<T>() {}\nfn start() { target::<u32>(); }\nfn dynamic(f: fn()) { (f)(); }\n",
    )
    .unwrap();
    let out = text(run(dir.path(), &["start", "--to", "target"]));
    assert!(out.contains("Path found"), "{out}");
    let out = text(run(dir.path(), &["dynamic"]));
    assert!(
        out.contains("unsupported call expression") && out.contains("Results may be incomplete"),
        "{out}"
    );
}

#[test]
fn c_functions_and_pointer_declarators_are_named_and_traced() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.c"),"static int leaf(int x) { return x; }\nstatic int (*pick(void))(int) { return leaf; }\nstatic int start(void) { return leaf(1); }\n").unwrap();
    let out = text(run(dir.path(), &["start", "--to", "leaf"]));
    assert!(out.contains("Path found"), "{out}");
    let out = text(run(dir.path(), &["pick"]));
    assert!(out.contains("Start: pick"), "{out}");
}
#[test]
fn lisp_prog_bindings_case_keys_and_phase_limits_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.lisp"),"(defun item () 1)\n(defun start () (prog ((item 1)) (return item)))\n(defun choice (x) (case x ((item) 1)))\n(defun typed (x) (typecase x ((item) 1)))\n(defun loaded () (load-time-value (item)))\n").unwrap();
    for from in ["start", "choice", "typed"] {
        let out = text(run(dir.path(), &[from, "--to", "item"]));
        assert!(out.contains("No path found"), "{out}");
    }
    let out = text(run(dir.path(), &["loaded"]));
    assert!(out.contains("evaluation phase"), "{out}");
}
#[test]
fn rust_later_local_items_and_deferred_bodies_do_not_invent_runtime_calls() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"),"fn target() {}\nfn local() { target(); fn target() {} }\nfn deferred() { let future=async { target(); }; }\n#[doc = \"cfg\"]\nfn clean() { target(); }\n").unwrap();
    for from in ["local", "deferred"] {
        let out = text(run(dir.path(), &[from, "--to", "target"]));
        assert!(
            out.contains("No path found") && out.contains("Results may be incomplete"),
            "{out}"
        );
    }
    let out = text(run(dir.path(), &["clean", "--to", "target"]));
    assert!(!out.contains("conditional compilation"), "{out}");
}

#[test]
fn a_proven_path_does_not_warn_about_unrelated_branches_or_languages() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "fn start() { end(); unknown(); }\nfn end() {}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("large.rs"), "x".repeat(500001)).unwrap();
    let out = text(run(dir.path(), &["start", "--to", "end"]));
    assert!(
        out.contains("Path found") && !out.contains("Results may be incomplete"),
        "{out}"
    );
    let out = text(run(dir.path(), &["start"]));
    assert!(out.contains("unresolved calls"), "{out}");
}

#[test]
fn rust_binding_index_handles_large_blocks_and_receiver_names_separately() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = String::from("fn target() {}\nfn stress() { target();\n");
    for i in 0..3000 {
        source.push_str(&format!("let target=||{{}}; let value{i}=0; target();\n"));
    }
    source.push_str("}\nstruct A; impl A { fn target(&self) {} fn method(&self,target:fn()) { self.target(); } }\n");
    std::fs::write(dir.path().join("a.rs"), source).unwrap();
    let out = text(run(dir.path(), &["stress"]));
    assert_eq!(out.matches("call site").count(), 1, "{out}");
    let out = text(run(dir.path(), &["A::method"]));
    assert!(!out.contains("lexically bound Rust"), "{out}");
}

#[test]
#[ignore = "requires sibling postgres, sbcl and filebrowser source trees"]
fn neighboring_source_corpora_smoke() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let cases = [
        (
            "../postgres/src/backend/tcop",
            "PostgresMain",
            "exec_simple_query",
            "Path found",
        ),
        (
            "../postgres/src/lisp",
            "parse-statements",
            "parse-statement",
            "Path found",
        ),
        (
            "../sbcl/src/pcl",
            "update-dfun",
            "compute-discriminating-function",
            "ambiguous",
        ),
        (
            "../filebrowser/http",
            "NewHandler",
            "getStaticHandlers",
            "Path found",
        ),
        (
            "../filebrowser/frontend/src/api",
            "files.ts:68",
            "fetchURL",
            "Path found",
        ),
    ];
    for (scope, from, to, expected) in cases {
        let out = text(run(
            repo,
            &[from, "--to", to, "--scope", scope, "--budget", "1500"],
        ));
        assert!(out.contains(expected), "{scope}: {out}");
    }
}

#[test]
fn selected_cfg_paths_and_import_shadowing_cannot_be_clean_witnesses() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"),"#[cfg(feature=\"x\")] fn end() {}\nfn start() { end(); }\nfn target() {}\nfn imported() { use other::target; target(); }\n").unwrap();
    let out = text(run(dir.path(), &["start", "--to", "end"]));
    assert!(out.contains("conditional compilation"), "{out}");
    let out = text(run(dir.path(), &["imported", "--to", "target"]));
    assert!(
        out.contains("No path found") && out.contains("lexically bound Rust"),
        "{out}"
    );
}

#[test]
fn uncaptured_c_function_pointer_syntax_is_not_silent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.c"),
        "int start(int (*callback)(void)) { return (*callback)(); }\n",
    )
    .unwrap();
    let out = text(run(dir.path(), &["start"]));
    assert!(
        out.contains("call-like syntax") && out.contains("Results may be incomplete"),
        "{out}"
    );
}

#[test]
fn rust_self_methods_keep_receiver_uncertainty_but_find_the_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "struct Foo; impl Foo { fn start(&self) { self.end(); } fn end(&self) {} }\n",
    )
    .unwrap();
    let out = text(run(dir.path(), &["Foo::start", "--to", "Foo::end"]));
    assert!(
        out.contains("Path found") && out.contains("receiver dispatch"),
        "{out}"
    );
}
