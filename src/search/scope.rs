//! Enclosing-scope annotator: given a `(file, line)`, return the nearest
//! enclosing definition, qualified with its containing type or module.
//! Used by the search formatter to annotate usages with their containing
//! function/class/module, and internally by `callers::find_enclosing_function`
//! when reporting the calling-function context of a call site.

use std::path::Path;

use crate::cache::OutlineCache;
use crate::lang::elixir::{extract_elixir_definition_name, is_elixir_definition};
use crate::lang::treesitter::{
    extract_definition_name, node_text_simple, NodeTextMode, DEFINITION_KINDS,
};

/// Type-like node kinds that can enclose a function definition.
const TYPE_KINDS: &[&str] = &[
    "class_declaration",
    "class_definition",
    "struct_item",
    "impl_item",
    "interface_declaration",
    "trait_item",
    "trait_declaration",
    "type_declaration",
    "enum_item",
    "enum_declaration",
    "module",
    "mod_item",
    "namespace_definition",
];

/// Resolved enclosing-definition context for a (file, line). Used by the
/// search formatter to annotate usages with their containing scope.
#[derive(Debug)]
pub struct EnclosingScope {
    /// Exact declaration identity, including declarations on the same line.
    pub(super) byte_range: (usize, usize),
    /// Normalized kind label (e.g. `"function"`, `"class"`, `"struct"`).
    pub kind: &'static str,
    /// Identifier of the definition. Qualified with its enclosing type or
    /// module when one wraps it (e.g. `"Class.method"`, `"Module.func"`).
    pub name: String,
}

/// Walk up the AST from `node` to the nearest definition, qualified with its
/// enclosing type/module if one wraps it. Returns the AST node so the caller
/// can read its kind, plus the rendered name and line range.
pub(super) fn walk_to_enclosing_definition<'a>(
    node: tree_sitter::Node<'a>,
    lines: &[&str],
    lang: crate::types::Lang,
) -> Option<(tree_sitter::Node<'a>, String, (u32, u32))> {
    let mut current = Some(node);
    while let Some(n) = current {
        let def_name = if matches!(n.kind(), "arrow_function" | "function_expression") {
            anonymous_function_name(n, lines)
        } else if DEFINITION_KINDS.contains(&n.kind()) {
            extract_definition_name(n, lines)
        } else if lang == crate::types::Lang::Elixir && is_elixir_definition(n, lines) {
            extract_elixir_definition_name(n, lines)
        } else {
            None
        };

        if let Some(name) = def_name {
            let range = (
                n.start_position().row as u32 + 1,
                n.end_position().row as u32 + 1,
            );

            // Walk further up to find an enclosing type/module and qualify the name.
            // `defmodule` is a `call` node, not in TYPE_KINDS, so Elixir needs a
            // separate check to produce `Module.func`.
            let mut parent = n.parent();
            while let Some(p) = parent {
                if TYPE_KINDS.contains(&p.kind()) {
                    if let Some(type_name) = extract_definition_name(p, lines) {
                        return Some((n, format!("{type_name}.{name}"), range));
                    }
                }
                if lang == crate::types::Lang::Elixir && is_elixir_definition(p, lines) {
                    if let Some(type_name) = extract_elixir_definition_name(p, lines) {
                        return Some((n, format!("{type_name}.{name}"), range));
                    }
                }
                parent = p.parent();
            }

            return Some((n, name, range));
        }
        current = n.parent();
    }
    None
}

fn anonymous_function_name(node: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let parent = node.parent()?;
    if parent.kind() == "variable_declarator" {
        return parent
            .child_by_field_name("name")
            .map(|name| node_text_simple(name, lines, NodeTextMode::Full));
    }
    if parent.kind() == "arguments" {
        let call = parent.parent()?;
        let function = call.child_by_field_name("function")?;
        let name = node_text_simple(function, lines, NodeTextMode::Full);
        let first = parent.named_child(0);
        if let Some(title) = first.filter(|n| n.kind() == "string") {
            return Some(format!(
                "{name}({}) callback",
                node_text_simple(title, lines, NodeTextMode::Full)
            ));
        }
        return Some(format!("{name} callback"));
    }
    Some("<anonymous>".to_string())
}

/// Find the nearest enclosing definition for `(path, line)` by re-parsing
/// the file with tree-sitter (cached on `OutlineCache`). AST-correct across
/// every language tilth supports — replaces parsing the rendered outline
/// string back into structured data.
///
/// Returns `None` if the file isn't a code file, the parse fails, or `line`
/// sits at the top level outside any definition.
pub fn enclosing_definition_at(
    path: &Path,
    line: u32,
    cache: &OutlineCache,
) -> Option<EnclosingScope> {
    if line == 0 {
        return None;
    }
    let parsed = cache.get_or_parse(path)?;
    let lines: Vec<&str> = parsed.content.lines().collect();
    let row = (line - 1) as usize;
    if row >= lines.len() {
        return None;
    }

    let column = lines[row].len() - lines[row].trim_start().len();
    let point = tree_sitter::Point { row, column };
    let target = parsed
        .tree
        .root_node()
        .descendant_for_point_range(point, point)?;

    let (mut def_node, mut name, _range) =
        walk_to_enclosing_definition(target, &lines, parsed.lang)?;
    // A local binding's initializer is part of its containing function's usage
    // scope. Starting at its first token must not label the function as that binding.
    if matches!(
        def_node.kind(),
        "lexical_declaration" | "variable_declaration" | "const_item" | "static_item"
    ) {
        let mut ancestor = def_node.parent();
        while let Some(node) = ancestor {
            if matches!(
                node.kind(),
                "function_declaration"
                    | "function_definition"
                    | "function_item"
                    | "method_definition"
                    | "method_declaration"
                    | "arrow_function"
                    | "function_expression"
                    | "generator_function"
                    | "generator_function_declaration"
            ) {
                let (function, function_name, _) =
                    walk_to_enclosing_definition(node, &lines, parsed.lang)?;
                def_node = function;
                name = function_name;
                break;
            }
            ancestor = node.parent();
        }
    }
    Some(EnclosingScope {
        byte_range: (def_node.start_byte(), def_node.end_byte()),
        kind: kind_label(def_node, &lines, parsed.lang),
        name,
    })
}

/// Map a tree-sitter definition node to a short user-facing label. Every kind
/// we handle is enumerated here, so adding a new language grammar is "add the
/// node kind to this match" with no string heuristics elsewhere.
fn kind_label(node: tree_sitter::Node, lines: &[&str], lang: crate::types::Lang) -> &'static str {
    match node.kind() {
        "function_declaration"
        | "function_definition"
        | "function_item"
        | "method_definition"
        | "method_declaration"
        | "decorated_definition"
        | "arrow_function"
        | "function_expression" => "function",
        "class_declaration" | "class_definition" => "class",
        "struct_item" => "struct",
        "interface_declaration" => "interface",
        "trait_declaration" | "trait_item" => "trait",
        "type_alias_declaration" | "type_item" | "type_declaration" => "type",
        "enum_item" | "enum_declaration" => "enum",
        "lexical_declaration" | "variable_declaration" => "variable",
        "const_item" | "const_declaration" => "const",
        "static_item" => "static",
        "property_declaration" => "property",
        "mod_item" | "namespace_definition" => "module",
        "object_declaration" => "object",
        "impl_item" => "impl",
        "export_statement" => "export",
        "call" if lang == crate::types::Lang::Elixir => elixir_kind_label(node, lines),
        _ => "definition",
    }
}

/// Elixir definitions are all `call` nodes; the keyword (`def`, `defmodule`,
/// …) lives in the call's `target` field. Map it to the same vocabulary
/// `kind_label` produces for other languages.
fn elixir_kind_label(node: tree_sitter::Node, lines: &[&str]) -> &'static str {
    let Some(target) = node.child_by_field_name("target") else {
        return "definition";
    };
    match node_text_simple(target, lines, NodeTextMode::Full).as_str() {
        "defmodule" => "module",
        "defprotocol" => "protocol",
        "defimpl" => "impl",
        "def" | "defp" | "defmacro" | "defmacrop" | "defguard" | "defguardp" | "defdelegate" => {
            "function"
        }
        "defstruct" | "defexception" => "struct",
        _ => "definition",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn enclosing_at_rust_top_level_function() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "a.rs", "fn foo() {\n    let x = 1;\n}\n");
        let cache = OutlineCache::new();
        let scope = enclosing_definition_at(&p, 2, &cache).unwrap();
        assert_eq!(scope.kind, "function");
        assert_eq!(scope.name, "foo");
    }

    #[test]
    fn enclosing_at_indented_inline_methods_keeps_the_method_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(
            tmp.path(),
            "a.ts",
            "class Foo {\n  first() { target(); }\n  second() { target(); }\n}\n",
        );
        let cache = OutlineCache::new();
        let first = enclosing_definition_at(&path, 2, &cache).unwrap();
        let second = enclosing_definition_at(&path, 3, &cache).unwrap();
        assert_eq!(first.name, "Foo.first");
        assert_eq!(second.name, "Foo.second");
        assert_ne!(first.byte_range, second.byte_range);
    }

    #[test]
    fn enclosing_at_rust_method_inside_mod() {
        // mod_item has a name field; impl_item does not, so the qualifier path
        // exercised here is mod-name → method-name.
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "a.rs",
            "mod outer {\n    fn helper() {\n        let x = 1;\n    }\n}\n",
        );
        let cache = OutlineCache::new();
        let scope = enclosing_definition_at(&p, 3, &cache).unwrap();
        assert_eq!(scope.kind, "function");
        assert_eq!(scope.name, "outer.helper");
    }

    #[test]
    fn enclosing_at_typescript_method_qualifies_with_class() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "a.ts",
            "class Foo {\n  bar() {\n    const x = 1;\n  }\n}\n",
        );
        let cache = OutlineCache::new();
        let scope = enclosing_definition_at(&p, 3, &cache).unwrap();
        assert_eq!(scope.kind, "function");
        assert_eq!(scope.name, "Foo.bar");
    }

    #[test]
    fn enclosing_at_python_method_qualifies_with_class() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "a.py",
            "class Foo:\n    def bar(self):\n        x = 1\n",
        );
        let cache = OutlineCache::new();
        let scope = enclosing_definition_at(&p, 3, &cache).unwrap();
        assert_eq!(scope.kind, "function");
        assert_eq!(scope.name, "Foo.bar");
    }

    #[test]
    fn enclosing_at_elixir_def_qualifies_with_module() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "a.ex",
            "defmodule Foo do\n  def bar do\n    :ok\n  end\nend\n",
        );
        let cache = OutlineCache::new();
        let scope = enclosing_definition_at(&p, 3, &cache).unwrap();
        assert_eq!(scope.kind, "function");
        assert_eq!(scope.name, "Foo.bar");
    }

    #[test]
    fn enclosing_at_elixir_defmodule_kind_is_module() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "a.ex",
            "defmodule Foo do\n  @moduledoc \"hi\"\nend\n",
        );
        let cache = OutlineCache::new();
        let scope = enclosing_definition_at(&p, 2, &cache).unwrap();
        assert_eq!(scope.kind, "module");
        assert_eq!(scope.name, "Foo");
    }

    #[test]
    fn enclosing_at_top_level_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "a.rs", "// just a comment\nfn foo() {}\n");
        let cache = OutlineCache::new();
        assert!(enclosing_definition_at(&p, 1, &cache).is_none());
    }

    #[test]
    fn enclosing_at_zero_line_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "a.rs", "fn foo() {}\n");
        let cache = OutlineCache::new();
        assert!(enclosing_definition_at(&p, 0, &cache).is_none());
    }

    #[test]
    fn enclosing_at_non_code_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "a.md", "# heading\n\nsome text\n");
        let cache = OutlineCache::new();
        assert!(enclosing_definition_at(&p, 3, &cache).is_none());
    }

    #[test]
    fn enclosing_at_caches_parse_across_calls() {
        // Two calls into the same file should reuse the cached parse —
        // observable indirectly by mutating the file between calls without
        // touching mtime: the first parse wins, the second sees stale data
        // because the mtime didn't change. (Test only asserts the cache hit
        // path returns the first-parse result.)
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "a.rs", "fn foo() { let x = 1; }\n");
        let cache = OutlineCache::new();
        let a = enclosing_definition_at(&p, 1, &cache).unwrap();
        let b = enclosing_definition_at(&p, 1, &cache).unwrap();
        assert_eq!(a.name, b.name);
        assert_eq!(a.kind, b.kind);
    }

    #[test]
    fn enclosing_at_kind_labels_for_common_definition_kinds() {
        // One case per kind_label match arm beyond `function`/`module`,
        // so a regression that miscategorizes (e.g.) a Rust `struct` as
        // `definition` would surface here.
        let cases: &[(&str, &str, u32, &str, &str)] = &[
            ("a.rs", "struct Foo { x: u32 }\n", 1, "struct", "Foo"),
            ("b.rs", "enum Color { Red, Blue }\n", 1, "enum", "Color"),
            (
                "c.rs",
                "trait Greeter { fn hi(&self); }\n",
                1,
                "trait",
                "Greeter",
            ),
            (
                "d.ts",
                "interface Shape { area(): number; }\n",
                1,
                "interface",
                "Shape",
            ),
            ("e.ts", "class Widget { x = 1; }\n", 1, "class", "Widget"),
        ];
        let cache = OutlineCache::new();
        for (filename, content, line, kind, name) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let p = write(tmp.path(), filename, content);
            let scope = enclosing_definition_at(&p, *line, &cache)
                .unwrap_or_else(|| panic!("no scope returned for {filename}"));
            assert_eq!(scope.kind, *kind, "kind mismatch for {filename}");
            assert_eq!(scope.name, *name, "name mismatch for {filename}");
        }
    }

    #[test]
    fn enclosing_at_rust_impl_block_does_not_qualify_with_type() {
        // tree-sitter-rust's `impl_item` exposes its type via a `type` field,
        // not via the `name`/`identifier`/`declarator` fields that
        // extract_definition_name probes. So methods inside `impl Foo {...}`
        // produce the bare function name, not `"Foo.bar"`. Pre-existing
        // behavior of find_enclosing_function — pinned here so a future
        // qualifier improvement is an intentional, visible change.
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "a.rs",
            "struct Foo;\nimpl Foo {\n    fn bar(&self) {\n        let x = 1;\n    }\n}\n",
        );
        let cache = OutlineCache::new();
        let scope = enclosing_definition_at(&p, 4, &cache).unwrap();
        assert_eq!(scope.kind, "function");
        assert_eq!(scope.name, "bar");
    }
}
