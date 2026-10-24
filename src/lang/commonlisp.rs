//! Common Lisp definitions are list forms, including ordinary lists when the
//! reader spelling does not use the grammar's lowercase special forms.
use tree_sitter::Node;

use crate::lang::spec::{DefinitionOps, LangSpec, StdlibRule};
use crate::types::{OutlineEntry, OutlineKind, SignatureEnd};

unsafe extern "C" {
    fn tree_sitter_commonlisp() -> *const ();
}

// Compile the patched parser into tilth itself so crates.io packaging cannot
// replace a local path dependency with the unpatched registry release.
pub(crate) const LANGUAGE: tree_sitter_language::LanguageFn =
    unsafe { tree_sitter_language::LanguageFn::from_raw(tree_sitter_commonlisp) };

pub(crate) const SPEC: LangSpec = LangSpec {
    display: "Common Lisp",
    extensions: &["lisp", "lsp", "cl", "asd"],
    filenames: &[],
    grammar: Some(LANGUAGE),
    callee_query: None,
    sibling_query: None,
    stdlib: StdlibRule::None,
    manifests: &[],
    definition_kinds: &["list_lit"],
    has_lifetimes: false,
    strip_family: None,
    extract_receiver: None,
    callee_filter: None,
    test_site: None,
    definitions: DefinitionOps {
        extract_name: |node, lines| definition(node, lines).map(|entry| entry.name),
        weight: |_, _| 100,
    },
};

/// Default reader case folding, preserving escaped characters and separating
/// package syntax from colons inside escaped symbol names.
fn symbol_key(name: &str) -> String {
    let mut result = String::new();
    let mut escaped = false;
    let mut chars = name.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    result.push(next);
                }
            }
            '|' => escaped = !escaped,
            ':' if !escaped => {
                if !result.ends_with('\0') {
                    result.push('\0');
                }
            }
            _ => result.push(if escaped { ch } else { ch.to_ascii_uppercase() }),
        }
    }
    result
}

pub(crate) fn names_equal(left: &str, right: &str) -> bool {
    symbol_key(left) == symbol_key(right)
}

/// Probe only ambiguous punctuation names; ordinary filenames and glob patterns
/// keep their existing meaning unless an actual Lisp definition is present.
pub(crate) fn needs_symbol_probe(query: &str) -> bool {
    if query.starts_with('|') && query.ends_with('|') {
        return true;
    }
    !query.contains('/')
        && !query.contains(char::is_whitespace)
        && (query.contains(':')
            || query.starts_with('%')
            || query.starts_with('|')
            || (query.starts_with('*') && query.ends_with('*') && query.len() > 2)
            || (query.starts_with('+') && query.ends_with('+') && query.len() > 2)
            || query.ends_with('?'))
}

pub(crate) fn has_definition(
    query: &str,
    scopes: &[std::path::PathBuf],
    glob: Option<&str>,
) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    let found = AtomicBool::new(false);
    for scope in scopes {
        let Ok(walker) = crate::search::walker(scope, glob) else {
            continue;
        };
        walker.run(|| {
            Box::new(|entry| {
                if found.load(Ordering::Relaxed) {
                    return ignore::WalkState::Quit;
                }
                let Ok(entry) = entry else {
                    return ignore::WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|kind| kind.is_file())
                    || crate::lang::detect_file_type(entry.path())
                        != crate::types::FileType::Code(crate::types::Lang::CommonLisp)
                    || entry.metadata().is_ok_and(|meta| meta.len() > 500_000)
                {
                    return ignore::WalkState::Continue;
                }
                if let Ok(source) = std::fs::read_to_string(entry.path()) {
                    let entries = crate::lang::outline::get_outline_entries(
                        &source,
                        crate::types::Lang::CommonLisp,
                    );
                    if entries.iter().any(|entry| names_equal(&entry.name, query)) {
                        found.store(true, Ordering::Relaxed);
                        return ignore::WalkState::Quit;
                    }
                }
                ignore::WalkState::Continue
            })
        });
        if found.load(Ordering::Relaxed) {
            return true;
        }
    }
    false
}

/// Match complete reader symbols, including punctuation and quoted references.
/// Strings/comments and the components of qualified names are not separate hits.
pub(crate) fn usage_lines(source: &str, query: &str) -> Vec<usize> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&LANGUAGE.into()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    let mut pending = vec![tree.root_node()];
    let mut rows = std::collections::BTreeSet::new();
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "sym_lit" | "package_lit" | "fancy_literal" | "kwd_lit"
        ) {
            if node
                .utf8_text(source.as_bytes())
                .is_ok_and(|name| names_equal(name, query))
            {
                rows.insert(node.start_position().row);
            }
            continue;
        }
        if matches!(
            node.kind(),
            "str_lit" | "comment" | "block_comment" | "char_lit"
        ) {
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    rows.into_iter().collect()
}

/// Named list elements without comments. Flatten the grammar's specialized
/// `defun`/header wrappers so uppercase and lowercase spellings share one path.
fn forms(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    let mut result = Vec::new();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "comment" | "block_comment" => {}
            "defun" | "defun_header" => result.extend(forms(child)),
            _ => result.push(child),
        }
    }
    result
}

fn text(node: Node<'_>, lines: &[&str]) -> String {
    let start = node.start_position();
    let end = node.end_position();
    if start.row == end.row {
        return lines
            .get(start.row)
            .and_then(|line| line.get(start.column..end.column))
            .unwrap_or("")
            .to_string();
    }
    let mut result = String::new();
    for row in start.row..=end.row {
        if row != start.row {
            result.push('\n');
        }
        let Some(line) = lines.get(row) else { break };
        let from = if row == start.row { start.column } else { 0 };
        let to = if row == end.row {
            end.column
        } else {
            line.len()
        };
        result.push_str(line.get(from..to).unwrap_or(""));
    }
    result
}

fn head(node: Node<'_>, lines: &[&str]) -> String {
    let spelling = text(node, lines).to_ascii_lowercase();
    // Cross-compilation packages (for example SB-XC) also expose defining forms.
    if spelling.starts_with(':') {
        spelling
    } else {
        spelling.rsplit(':').next().unwrap_or(&spelling).to_string()
    }
}

/// Collapse layout outside strings while preserving literal contents and
/// removing comments. Common Lisp quote/backquote are reader prefixes, not strings.
fn compact(source: &str) -> String {
    let mut out = String::new();
    let mut chars = source.chars().peekable();
    let mut string = false;
    let mut bar_symbol = false;
    let mut space = false;
    let mut block = 0usize;
    while let Some(ch) = chars.next() {
        if string {
            out.push(ch);
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if ch == '"' {
                string = false;
            }
            continue;
        }
        if block == 0 && (bar_symbol || ch == '\\') {
            if !bar_symbol && space && !out.is_empty() && !out.ends_with('(') {
                out.push(' ');
            }
            space = false;
            out.push(ch);
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if ch == '|' {
                bar_symbol = false;
            }
            continue;
        }
        if ch == '#' && chars.peek() == Some(&'|') {
            chars.next();
            block += 1;
            space = true;
            continue;
        }
        if block > 0 {
            if ch == '|' && chars.peek() == Some(&'#') {
                chars.next();
                block -= 1;
            }
            continue;
        }
        if ch == ';' {
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
            space = true;
        } else if ch.is_whitespace() {
            space = true;
        } else {
            if space && !out.is_empty() && !out.ends_with('(') && ch != ')' {
                out.push(' ');
            }
            space = false;
            out.push(ch);
            if ch == '"' {
                string = true;
            }
            if ch == '|' {
                bar_symbol = true;
            }
        }
    }
    out
}

fn docstring(node: Node<'_>, lines: &[&str]) -> Option<String> {
    if node.kind() != "str_lit" {
        return None;
    }
    let raw = text(node, lines);
    let body = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = body.chars();
    let mut unescaped = String::new();
    while let Some(ch) = chars.next() {
        unescaped.push(if ch == '\\' {
            chars.next().unwrap_or(ch)
        } else {
            ch
        });
    }
    Some(unescaped.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Recognize the common defining forms without evaluating or expanding macros.
fn definition(node: Node<'_>, lines: &[&str]) -> Option<OutlineEntry> {
    if node.kind() != "list_lit" {
        return None;
    }
    let parts = forms(node);
    let keyword = head(*parts.first()?, lines);
    let kind = match keyword.as_str() {
        "defun" | "defmacro" | "defgeneric" | "defmethod" => OutlineKind::Function,
        "defclass" | "define-condition" => OutlineKind::Class,
        "defstruct" => OutlineKind::Struct,
        "defvar" | "defparameter" => OutlineKind::Variable,
        "defconstant" => OutlineKind::Constant,
        "defpackage" | "defsystem" => OutlineKind::Module,
        _ => return None,
    };
    let name_node = *parts.get(1)?;
    let actual_name = if keyword == "defstruct" && name_node.kind() == "list_lit" {
        *forms(name_node).first()?
    } else {
        name_node
    };
    if !matches!(
        actual_name.kind(),
        "sym_lit" | "package_lit" | "fancy_literal" | "kwd_lit" | "str_lit" | "list_lit"
    ) {
        return None;
    }
    let name = if kind == OutlineKind::Module {
        text(actual_name, lines)
            .trim_matches('"')
            .trim_start_matches("#:")
            .trim_start_matches(':')
            .to_string()
    } else {
        compact(&text(actual_name, lines))
    };
    let header_index = if kind == OutlineKind::Function {
        parts
            .iter()
            .enumerate()
            .skip(2)
            .find(|(_, part)| part.kind() == "list_lit")?
            .0
    } else if kind == OutlineKind::Class {
        2
    } else {
        1
    };
    let header_end = parts.get(header_index)?.end_position();
    let start = node.start_position();
    let mut signature = String::new();
    for row in start.row..=header_end.row {
        let line = lines.get(row)?;
        if row != start.row {
            signature.push('\n');
        }
        let from = if row == start.row { start.column } else { 0 };
        let to = if row == header_end.row {
            header_end.column
        } else {
            line.len()
        };
        signature.push_str(line.get(from..to)?);
    }
    let doc_index = if matches!(keyword.as_str(), "defvar" | "defparameter" | "defconstant") {
        3
    } else {
        header_index + 1
    };
    let doc = parts
        .get(doc_index)
        .and_then(|node| docstring(*node, lines))
        .or_else(|| {
            parts.iter().skip(header_index + 1).find_map(|option| {
                if option.kind() != "list_lit" {
                    return None;
                }
                let args = forms(*option);
                if head(*args.first()?, lines) != ":documentation" {
                    return None;
                }
                docstring(*args.get(1)?, lines)
            })
        });
    Some(OutlineEntry {
        kind,
        name,
        start_line: start.row as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
        signature: Some(compact(&signature)),
        signature_end: Some(SignatureEnd {
            line: header_end.row as u32 + 1,
            column: header_end.column,
        }),
        children: Vec::new(),
        doc,
    })
}

/// Only descend into forms known to contain evaluated top-level declarations.
/// Quoted data, lambda lists, function bodies and arbitrary macro arguments are
/// deliberately excluded. Reader conditionals expose both source branches.
pub(crate) fn outline(root: Node<'_>, lines: &[&str]) -> Vec<OutlineEntry> {
    let mut pending = vec![root];
    let mut entries = Vec::new();
    while let Some(node) = pending.pop() {
        if let Some(entry) = definition(node, lines) {
            entries.push(entry);
            continue;
        }
        let children = if node.kind() == "source" {
            forms(node)
        } else if node.kind() == "include_reader_macro" {
            node.child_by_field_name("target").into_iter().collect()
        } else if node.kind() == "list_lit" {
            let args = forms(node);
            let keyword = args
                .first()
                .map(|node| head(*node, lines))
                .unwrap_or_default();
            let skip = match keyword.as_str() {
                "progn" | "locally" => 1,
                "eval-when" | "macrolet" | "symbol-macrolet" | "let" | "let*" | "flet"
                | "labels" => 2,
                _ => continue,
            };
            args.into_iter().skip(skip).collect()
        } else {
            continue;
        };
        pending.extend(children.into_iter().rev());
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::{detect_file_type, outline::get_outline_entries};
    use crate::types::{FileType, Lang};
    use std::path::Path;

    #[test]
    fn commonlisp_repeated_bar_comments_hide_definitions() {
        for comment in [
            "#|| (defun hidden () nil) ||#",
            "#|||\n(defun hidden () nil)\n|||#",
            "#||#",
        ] {
            let source = format!("{comment}\n(defun visible () t)\n");
            let entries = get_outline_entries(&source, Lang::CommonLisp);
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>(),
                ["visible"],
                "{source}"
            );
            assert!(crate::lang::commonlisp::usage_lines(&source, "hidden").is_empty());
        }
    }

    #[test]
    fn commonlisp_pending_loop_fixes_preserve_surrounding_definitions() {
        let source = "(defun named-consumer (xs)\n (loop named #:outer for x in xs when x collect x end collect 0))\n(defun quantified (xs)\n (loop for x in xs always (predicate x))\n (loop for x in xs never (predicate x))\n (loop for x in xs thereis (predicate x)))\n(defun after-loops () t)\n";
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&LANGUAGE.into()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "{}",
            tree.root_node().to_sexp()
        );
        let entries = get_outline_entries(source, Lang::CommonLisp);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["named-consumer", "quantified", "after-loops"]
        );
    }

    #[test]
    fn commonlisp_definitions_and_docstrings() {
        let source = r#"(in-package :demo)
(defun hello-world (name &optional (x "a  b")) "Greets NAME." (format nil "~a" name))
(defmacro with-thing ((var value) &body body) "Wrap BODY." `(let ((,var ,value)) ,@body))
(DEFUN UPPER (X) X)
(defmethod render :around ((x widget)) "Docs." (call-next-method))
(defgeneric frob (x) (:documentation "Generic docs."))
(defclass widget (base) ((slot :initarg :slot)) (:documentation "Widget docs."))
(defstruct (pair (:constructor make-pair)) "Pair docs." first second)
(defvar *state* nil "State docs.")
(defparameter *limit* 10 "Limit docs.")
(defconstant +size+ 5 "Size docs.")
(defpackage :demo (:use :cl) (:documentation "Package docs."))
(define-condition problem (error) () (:documentation "Condition docs."))
"#;
        let entries = get_outline_entries(source, Lang::CommonLisp);
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "hello-world",
                "with-thing",
                "UPPER",
                "render",
                "frob",
                "widget",
                "pair",
                "*state*",
                "*limit*",
                "+size+",
                "demo",
                "problem"
            ]
        );
        for (name, doc) in [
            ("hello-world", "Greets NAME."),
            ("with-thing", "Wrap BODY."),
            ("render", "Docs."),
            ("frob", "Generic docs."),
            ("widget", "Widget docs."),
            ("pair", "Pair docs."),
            ("*state*", "State docs."),
            ("demo", "Package docs."),
        ] {
            assert_eq!(
                entries
                    .iter()
                    .find(|e| e.name == name)
                    .unwrap()
                    .doc
                    .as_deref(),
                Some(doc),
                "{name}"
            );
        }
        assert!(entries[0].signature.as_ref().unwrap().contains("\"a  b\""));
        assert!(!entries[0].signature.as_ref().unwrap().contains("Greets"));
    }

    #[test]
    fn commonlisp_top_level_wrappers_exclude_quoted_and_local_definitions() {
        let source = "#+sbcl (defun platform () t)\n(progn (defun wrapped () t))\n(eval-when (:compile-toplevel) (defmacro compile-it () nil))\n'(defun quoted () nil)\n`(defun backquoted () nil)\n(list (defun data () nil))\n(defun outer () (defun nested () nil))\n";
        let entries = get_outline_entries(source, Lang::CommonLisp);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["platform", "wrapped", "compile-it", "outer"]
        );
    }

    #[test]
    fn commonlisp_extensions_and_multiline_header() {
        for ext in SPEC.extensions {
            assert_eq!(
                detect_file_type(Path::new(&format!("file.{ext}"))),
                FileType::Code(Lang::CommonLisp)
            );
        }
        let source = "(cl:defun pkg:hello\n (x ; parameter\n  &optional (y 'default))\n \"Some docs.\"\n (+ x y))\n";
        let entries = get_outline_entries(source, Lang::CommonLisp);
        assert_eq!(entries[0].name, "pkg:hello");
        assert_eq!(
            entries[0].signature.as_deref(),
            Some("(cl:defun pkg:hello (x &optional (y 'default))")
        );
        assert_eq!(entries[0].signature_end.unwrap().line, 3);
        assert_eq!(entries[0].doc.as_deref(), Some("Some docs."));
    }
    #[test]
    fn commonlisp_lambda_named_parameter_does_not_hide_later_forms() {
        let source =
            "(defun inline-closure (lambda env)\n (list lambda env))\n(defun after (&aux (loop *loop*)) loop)\n";
        let entries = get_outline_entries(source, Lang::CommonLisp);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["inline-closure", "after"]
        );
        assert_eq!(
            entries[0].signature.as_deref(),
            Some("(defun inline-closure (lambda env)")
        );
        assert_eq!((entries[1].start_line, entries[1].end_line), (3, 3));
    }

    #[test]
    fn commonlisp_reader_case_and_escapes_are_preserved() {
        assert!(names_equal("Foo", "FOO"));
        assert_eq!(compact(r"(defun \foo (x)"), r"(defun \foo (x)");
        assert!(names_equal("pkg:Foo", "PKG::FOO"));
        assert!(!names_equal("|Foo|", "foo"));
        assert!(names_equal("|FOO|", "foo"));
        assert!(!names_equal("|pkg:foo|", "|pkg|:|foo|"));
        assert_eq!(compact("(defun |foo;  bar| (x)"), "(defun |foo;  bar| (x)");
        let source = "(defpackage #:demo (:use :cl))\n(asdf:defsystem \"sample\" :components ((:file \"core\")))\n";
        let entries = get_outline_entries(source, Lang::CommonLisp);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["demo", "sample"]
        );
        let bloom = crate::index::bloom::BloomFilterCache::new();
        let source = "'(quoted)\n(defun target () t)\n(defvar *state* nil)\n";
        for name in ["target", "*state*"] {
            assert!(bloom.contains(
                Path::new("forms.lisp"),
                std::time::SystemTime::UNIX_EPOCH,
                source,
                name
            ));
        }
    }

    #[test]
    fn commonlisp_symbol_search_and_grok() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("forms.lisp"),
            "(sb-xc:defstruct thing slot)\n(defun pkg:hello-world (x) \"Hello.\" x)\n(defvar *state* nil)\n(defconstant +limit+ 10)\n").unwrap();
        for name in ["thing", "pkg:hello-world", "*state*", "+limit+"] {
            let result = crate::search::search_symbol_raw(name, root.path(), None, false).unwrap();
            assert_eq!(result.definitions, 1, "{name}: {result:?}");
        }
        let output = crate::run_grok("pkg:hello-world", root.path(), false).unwrap();
        assert!(output.contains("Hello."), "{output}");
        assert!(!output.contains("missing from the outline"), "{output}");
    }

    #[test]
    fn commonlisp_diff_distinguishes_body_and_lambda_list_changes() {
        use crate::diff::{
            matching::{build_diff_symbols, match_symbols},
            ChangeType,
        };
        let symbols = |source: &str| {
            build_diff_symbols(
                &get_outline_entries(source, Lang::CommonLisp),
                source,
                Lang::CommonLisp,
            )
        };
        let old = symbols("(defun hello (x)\n  (+ x 1))\n");
        let body = match_symbols(&old, &symbols("(defun hello (x)\n  (+ x 2))\n"));
        assert_eq!(body.len(), 1);
        assert!(matches!(body[0].change, ChangeType::BodyChanged));
        let signature = match_symbols(&old, &symbols("(defun hello (x &optional y)\n  (+ x y))\n"));
        assert!(matches!(signature[0].change, ChangeType::SignatureChanged));
    }

    // Run with TILTH_SBCL_ROOT=/path/to/sbcl cargo test
    // commonlisp_sbcl_corpus -- --ignored --nocapture.
    #[test]
    #[ignore = "requires an SBCL source checkout via TILTH_SBCL_ROOT"]
    fn commonlisp_sbcl_corpus() {
        let root = std::path::PathBuf::from(
            std::env::var_os("TILTH_SBCL_ROOT").expect("set TILTH_SBCL_ROOT"),
        );
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&LANGUAGE.into()).unwrap();
        let mut files = 0usize;
        let mut definitions = 0usize;
        let mut parser_errors = 0usize;
        let mut simple_forms = 0usize;
        let mut found_simple_forms = 0usize;
        let mut missed = Vec::new();
        let mut report = std::collections::BTreeMap::new();
        for file in ignore::WalkBuilder::new(root.join("src")).build() {
            let file = file.unwrap();
            if !file.file_type().is_some_and(|kind| kind.is_file())
                || file.path().extension().is_none_or(|ext| ext != "lisp")
            {
                continue;
            }
            let source = std::fs::read_to_string(file.path()).unwrap();
            let tree = parser.parse(&source, None).unwrap();
            parser_errors += usize::from(tree.root_node().has_error());
            let lines: Vec<_> = source.lines().collect();
            let entries = outline(tree.root_node(), &lines);
            files += 1;
            definitions += entries.len();
            report.insert(
                file.path()
                    .strip_prefix(&root)
                    .unwrap()
                    .display()
                    .to_string(),
                entries
                    .iter()
                    .map(|entry| (entry.name.clone(), entry.start_line, entry.end_line))
                    .collect::<Vec<_>>(),
            );
            for entry in &entries {
                assert!(
                    entry.start_line <= entry.end_line && entry.end_line as usize <= lines.len(),
                    "{}: {entry:?}",
                    file.path().display()
                );
            }
            // Audit column-zero DEFUN/DEFMACRO forms, independently of the AST.
            for (index, line) in lines.iter().enumerate() {
                if line.starts_with("(defun ") || line.starts_with("(defmacro ") {
                    simple_forms += 1;
                    if entries
                        .iter()
                        .any(|entry| entry.start_line as usize == index + 1)
                    {
                        found_simple_forms += 1;
                    } else if missed.len() < 12 {
                        missed.push(format!("{}:{} {line}", file.path().display(), index + 1));
                    }
                }
            }
        }
        eprintln!("SBCL: {files} Lisp files; {definitions} definitions; {parser_errors} files with grammar errors; {found_simple_forms}/{simple_forms} column-zero DEFUN/DEFMACRO forms indexed");
        for miss in missed {
            eprintln!("audit miss: {miss}");
        }
        if let Some(path) = std::env::var_os("TILTH_COMMONLISP_AUDIT_REPORT") {
            serde_json::to_writer(std::fs::File::create(path).unwrap(), &report).unwrap();
        }
        assert!(files > 100 && definitions > 1000);
        assert!(
            found_simple_forms * 100 >= simple_forms * 90,
            "unexpectedly low coverage of simple defining forms"
        );
    }
}
