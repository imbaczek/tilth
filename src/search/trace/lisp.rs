//! Conservative evaluation-aware traversal of Common Lisp forms.
use super::extract::Call;
use crate::cache::ParsedFile;
use tree_sitter::Node;

fn parts(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    let mut out = Vec::new();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "comment" | "block_comment" => {}
            "defun" | "defun_header" => out.extend(parts(child)),
            _ => out.push(child),
        }
    }
    out
}
fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}
fn head(node: Node<'_>, source: &str) -> String {
    let key = crate::lang::commonlisp::symbol_key(text(node, source));
    let unqualified = key
        .strip_prefix("CL\0")
        .or_else(|| key.strip_prefix("COMMON-LISP\0"))
        .unwrap_or(&key);
    if !unqualified.contains('\0') && unqualified == unqualified.to_ascii_uppercase() {
        unqualified.to_ascii_lowercase()
    } else {
        format!("symbol:{key}")
    }
}

pub(super) fn calls(parsed: &ParsedFile) -> (Vec<Call>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    visit(
        parsed.tree.root_node(),
        &parsed.content,
        &[],
        &mut out,
        &mut problems,
        0,
    );
    (out, problems)
}
fn visit(
    node: Node<'_>,
    source: &str,
    locals: &[String],
    out: &mut Vec<Call>,
    problems: &mut Vec<String>,
    depth: usize,
) {
    if depth > 256 || out.len() >= super::MAX_EDGES {
        if !problems
            .iter()
            .any(|p| p == "Lisp traversal work limit reached")
        {
            problems.push("Lisp traversal work limit reached".into());
        }
        return;
    }
    match node.kind() {
        "quoting_lit" | "var_quoting_lit" | "str_lit" | "comment" | "block_comment" => return,
        "read_cond_lit" | "splicing_read_cond_lit" => {
            problems.push("reader-conditional branches are not evaluated".into());
        }
        "include_reader_macro" => {
            problems.push("reader-conditional branches are not evaluated".into());
            if let Some(target) = node.child_by_field_name("target") {
                visit(target, source, locals, out, problems, depth + 1);
            }
            return;
        }
        "syn_quoting_lit" => {
            problems.push("quasiquote expansion is not traced".into());
            return;
        }
        _ => {}
    }
    if node.kind() != "list_lit" {
        for child in parts(node) {
            visit(child, source, locals, out, problems, depth + 1);
        }
        return;
    }
    let forms = parts(node);
    let Some(first) = forms.first().copied() else {
        return;
    };
    let keyword = head(first, source);
    let children = |items: &[Node<'_>], out: &mut Vec<Call>, problems: &mut Vec<String>| {
        for item in items {
            visit(*item, source, locals, out, problems, depth + 1);
        }
    };
    match keyword.as_str() {
        "quote" | "function" | "declare" | "go" | "defclass" | "defstruct" | "defpackage"
        | "define-condition" => {}
        "defun" | "defmacro" | "defmethod" | "defgeneric" => {
            if let Some(header) = forms
                .iter()
                .enumerate()
                .skip(2)
                .find(|(_, n)| n.kind() == "list_lit")
                .map(|(i, _)| i)
            {
                children(&forms[header + 1..], out, problems);
            }
        }
        "defvar"
        | "defparameter"
        | "defconstant"
        | "multiple-value-bind"
        | "destructuring-bind"
        | "multiple-value-setq"
        | "the" => {
            children(forms.get(2..).unwrap_or(&[]), out, problems);
        }

        "let" | "let*" | "prog" | "prog*" => {
            if let Some(bindings) = forms.get(1) {
                for binding in parts(*bindings) {
                    if binding.kind() == "list_lit" {
                        let binding = parts(binding);
                        children(binding.get(1..).unwrap_or(&[]), out, problems);
                    }
                }
            }
            children(forms.get(2..).unwrap_or(&[]), out, problems);
        }
        "dolist" | "dotimes" => {
            if let Some(binding) = forms.get(1) {
                let binding = parts(*binding);
                children(binding.get(1..).unwrap_or(&[]), out, problems);
            }
            children(forms.get(2..).unwrap_or(&[]), out, problems);
        }
        "do" | "do*" => {
            if let Some(bindings) = forms.get(1) {
                for binding in parts(*bindings) {
                    if binding.kind() == "list_lit" {
                        let binding = parts(binding);
                        children(binding.get(1..).unwrap_or(&[]), out, problems);
                    }
                }
            }
            if let Some(exit) = forms.get(2) {
                children(&parts(*exit), out, problems);
            }
            children(forms.get(3..).unwrap_or(&[]), out, problems);
        }

        "handler-case" | "restart-case" => {
            if let Some(protected) = forms.get(1) {
                visit(*protected, source, locals, out, problems, depth + 1);
            }
            for clause in forms.iter().skip(2) {
                let clause = parts(*clause);
                children(clause.get(2..).unwrap_or(&[]), out, problems);
            }
            problems.push("handler or restart dispatch is not resolved".into());
        }
        "handler-bind" | "restart-bind" => {
            if let Some(bindings) = forms.get(1) {
                for binding in parts(*bindings) {
                    let binding = parts(binding);
                    children(binding.get(1..).unwrap_or(&[]), out, problems);
                }
            }
            children(forms.get(2..).unwrap_or(&[]), out, problems);
            problems.push("handler or restart dispatch is not resolved".into());
        }
        "with-open-file"
        | "with-open-stream"
        | "with-input-from-string"
        | "with-output-to-string" => {
            if let Some(binding) = forms.get(1) {
                let binding = parts(*binding);
                children(binding.get(1..).unwrap_or(&[]), out, problems);
            }
            children(forms.get(2..).unwrap_or(&[]), out, problems);
            problems.push("resource macro expansion may add calls".into());
        }
        "with-slots" | "with-accessors" => {
            children(forms.get(2..).unwrap_or(&[]), out, problems);
            problems.push("slot or accessor dispatch is not resolved".into());
        }
        "loop" => {
            let mut previous = String::new();
            for form in forms.iter().skip(1) {
                if form.kind() == "list_lit"
                    && !matches!(previous.as_str(), "for" | "as" | "with" | "of-type")
                {
                    visit(*form, source, locals, out, problems, depth + 1);
                }
                previous = head(*form, source);
            }
            problems.push("LOOP clauses are analyzed heuristically".into());
        }
        "flet" | "labels" | "macrolet" => {
            let bindings = forms.get(1).map(|n| parts(*n)).unwrap_or_default();
            let mut names = locals.to_vec();
            for binding in &bindings {
                if let Some(name) = parts(*binding).first() {
                    names.push(text(*name, source).to_owned());
                }
            }
            // Local function bodies have no indexed identity. Keep them out of the global graph.
            if !bindings.is_empty() {
                problems.push("local function or macro bodies are not indexed".into());
            }
            for body in forms.get(2..).unwrap_or(&[]) {
                visit(*body, source, &names, out, problems, depth + 1);
            }
        }
        "lambda" => {
            problems.push("closure invocation is not resolved".into());
        }
        "case" | "ecase" | "ccase" | "typecase" | "etypecase" | "ctypecase" => {
            if let Some(value) = forms.get(1) {
                visit(*value, source, locals, out, problems, depth + 1);
            }
            for clause in forms.iter().skip(2) {
                let clause = parts(*clause);
                children(clause.get(1..).unwrap_or(&[]), out, problems);
            }
        }
        "eval-when" => {
            children(forms.get(2..).unwrap_or(&[]), out, problems);
            problems.push("evaluation phase is not resolved".into());
        }
        "load-time-value" => {
            children(forms.get(1..).unwrap_or(&[]), out, problems);
            problems.push("evaluation phase is not resolved".into());
        }
        "cond" => {
            for clause in forms.iter().skip(1) {
                children(&parts(*clause), out, problems);
            }
        }
        "setq" => {
            for value in forms.iter().skip(2).step_by(2) {
                visit(*value, source, locals, out, problems, depth + 1);
            }
        }
        "setf" => {
            for (i, value) in forms.iter().enumerate().skip(1) {
                if i % 2 == 0 {
                    visit(*value, source, locals, out, problems, depth + 1);
                } else if value.kind() == "list_lit" {
                    problems.push("generalized assignment dispatch is not resolved".into());
                    let place = parts(*value);
                    children(place.get(1..).unwrap_or(&[]), out, problems);
                }
            }
        }
        "progn"
        | "prog1"
        | "prog2"
        | "progv"
        | "multiple-value-prog1"
        | "if"
        | "when"
        | "unless"
        | "and"
        | "or"
        | "not"
        | "unwind-protect"
        | "block"
        | "return-from"
        | "catch"
        | "throw"
        | "tagbody"
        | "locally" => children(&forms[1..], out, problems),
        _ => {
            let name = text(first, source).to_owned();
            let mut concerns = Vec::new();
            if !matches!(first.kind(), "sym_lit" | "package_lit" | "fancy_literal") {
                problems.push("computed Lisp operator is not resolved".into());
                children(&forms, out, problems);
                return;
            }
            if locals
                .iter()
                .any(|local| crate::lang::commonlisp::names_equal(local, &name))
            {
                concerns.push("lexically bound Lisp function is not resolved");
            }
            if matches!(keyword.as_str(), "funcall" | "apply" | "eval") {
                concerns.push("dynamic Lisp invocation is not resolved");
            }
            out.push(Call {
                name,
                byte: first.start_byte(),
                line: first.start_position().row as u32 + 1,
                concerns,
            });
            children(&forms[1..], out, problems);
        }
    }
}
