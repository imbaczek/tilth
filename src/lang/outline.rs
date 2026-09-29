use crate::lang::treesitter::{node_text_simple, NodeTextMode};
use crate::types::{Lang, OutlineEntry, OutlineKind, SignatureEnd};

/// Get the tree-sitter Language for a given Lang variant.
pub fn outline_language(lang: Lang) -> Option<tree_sitter::Language> {
    crate::lang::spec::spec(lang).grammar.map(Into::into)
}

/// Parse markdown content into a tree-sitter block tree.
///
/// Returns `None` if the parser fails to set the language (should not happen
/// in practice). The block grammar is what tilth's outline / definition
/// scanners need: it emits `atx_heading`, `setext_heading`, `section`, and
/// `fenced_code_block` nodes. Inline structure (emphasis, links inside the
/// heading text) is parsed by a separate inline grammar tilth doesn't use —
/// heading text is read as the raw inline node's text.
///
/// Centralised so both `read::outline::markdown` and
/// `search::symbol::find_defs_markdown_buf` configure the parser the same
/// way.
pub fn parse_markdown(content: &str) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_md::LANGUAGE.into()).ok()?;
    parser.parse(content, None)
}

/// Map an `atx_heading` or `setext_heading` node to its 1-6 level by
/// inspecting the marker child. Returns `None` for malformed nodes.
pub fn heading_level(node: tree_sitter::Node) -> Option<u8> {
    let kind = node.kind();
    if kind == "atx_heading" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "atx_h1_marker" => return Some(1),
                "atx_h2_marker" => return Some(2),
                "atx_h3_marker" => return Some(3),
                "atx_h4_marker" => return Some(4),
                "atx_h5_marker" => return Some(5),
                "atx_h6_marker" => return Some(6),
                _ => {}
            }
        }
        None
    } else if kind == "setext_heading" {
        // setext H1: `=====`; H2: `-----`. Marker is a child node.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "setext_h1_underline" => return Some(1),
                "setext_h2_underline" => return Some(2),
                _ => {}
            }
        }
        None
    } else {
        None
    }
}

/// Read the heading text of an `atx_heading` / `setext_heading` node from
/// pre-split source lines. Returns the inline content with surrounding
/// whitespace + trailing `#`s (for ATX-closed headings like `## Foo ##`)
/// trimmed, matching the previous hand-rolled scanner's output.
pub fn heading_text(node: tree_sitter::Node, lines: &[&str]) -> String {
    // Both heading kinds expose their inline content as an `inline` child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "inline" {
            let text = node_text_simple(child, lines, NodeTextMode::Full);
            return text.trim().trim_end_matches('#').trim().to_string();
        }
    }
    String::new()
}

/// Walk top-level children of the root node, extracting outline entries.
pub(crate) fn walk_top_level(
    root: tree_sitter::Node,
    lines: &[&str],
    lang: Lang,
) -> Vec<OutlineEntry> {
    let mut entries = Vec::new();
    let mut cursor = root.walk();

    for child in root.children(&mut cursor) {
        if let Some(entry) = node_to_entry(child, lines, lang, 0) {
            entries.push(entry);
        }
    }

    entries
}

/// Convert a tree-sitter node to an `OutlineEntry` based on its kind.
fn node_to_entry(
    node: tree_sitter::Node,
    lines: &[&str],
    lang: Lang,
    depth: usize,
) -> Option<OutlineEntry> {
    // Python decorators wrap the declaration in `decorated_definition`.
    // Delegate to its function or class node to keep normal metadata and child traversal.
    if node.kind() == "decorated_definition" && lang == Lang::Python {
        let mut cursor = node.walk();
        let declaration = node
            .children(&mut cursor)
            .find(|child| matches!(child.kind(), "function_definition" | "class_definition"))?;
        let mut entry = node_to_entry(declaration, lines, lang, depth)?;
        if entry.doc.is_none() {
            entry.doc = extract_doc(node, lines);
        }
        return Some(entry);
    }

    let kind_str = node.kind();
    let start_line = node.start_position().row as u32 + 1;
    let end_line = node.end_position().row as u32 + 1;

    let mut signature_end = None;
    let (kind, name, signature) = match kind_str {
        // Functions
        "function_declaration"
        | "function_definition"
        | "function_item"
        | "method_definition"
        | "method_declaration"
        | "constructor_declaration"
        | "init_declaration"
        | "deinit_declaration"
        | "protocol_function_declaration" => {
            let name = find_child_text(node, "name", lines)
                .or_else(|| find_child_text(node, "identifier", lines))
                .unwrap_or_else(|| {
                    // Swift deinit has no name field — use the node kind as name
                    if kind_str == "deinit_declaration" {
                        "deinit".into()
                    } else {
                        "<anonymous>".into()
                    }
                });
            let (sig, end) = extract_signature(node, lines, lang);
            signature_end = Some(end);
            (OutlineKind::Function, name, Some(sig))
        }

        // Classes & structs
        "class_declaration" | "class_definition" => {
            let name = find_child_text(node, "name", lines)
                .or_else(|| find_child_text(node, "identifier", lines))
                .unwrap_or_else(|| "<anonymous>".into());
            (OutlineKind::Class, name, None)
        }
        "struct_item" | "struct_declaration" => {
            let name = find_child_text(node, "name", lines).unwrap_or_else(|| "<anonymous>".into());
            (OutlineKind::Struct, name, None)
        }

        // Interfaces & traits
        "interface_declaration"
        | "type_alias_declaration"
        | "trait_item"
        | "trait_declaration"
        | "trait_definition"
        | "protocol_declaration" => {
            let name = find_child_text(node, "name", lines).unwrap_or_else(|| "<anonymous>".into());
            (OutlineKind::Interface, name, None)
        }
        "type_item" | "type_definition" | "typealias_declaration" => {
            let name = find_child_text(node, "name", lines).unwrap_or_else(|| "<anonymous>".into());
            (OutlineKind::TypeAlias, name, None)
        }

        // Enums
        "enum_item" | "enum_declaration" | "enum_definition" => {
            let name = find_child_text(node, "name", lines).unwrap_or_else(|| "<anonymous>".into());
            (OutlineKind::Enum, name, None)
        }

        // Impl blocks (Rust)
        "impl_item" => {
            let name = find_child_text(node, "type", lines).unwrap_or_else(|| "<impl>".into());
            (OutlineKind::Module, format!("impl {name}"), None)
        }

        // Objects (Scala companion objects, singletons; Kotlin object declarations)
        "object_declaration" | "object_definition" => {
            let name = find_child_text(node, "name", lines)
                .or_else(|| find_child_text(node, "identifier", lines))
                .unwrap_or_else(|| "<anonymous>".into());
            (OutlineKind::Module, name, None)
        }

        // Constants and variables
        "const_item" | "const_declaration" | "static_item" => {
            let name = find_child_text(node, "name", lines)
                .or_else(|| first_identifier_text(node, lines))
                .unwrap_or_else(|| "<const>".into());
            (OutlineKind::Constant, name, None)
        }
        "val_definition" => {
            let name = first_identifier_text(node, lines).unwrap_or_else(|| "<val>".into());
            (OutlineKind::ImmutableVariable, name, None)
        }
        "lexical_declaration" | "variable_declaration" | "var_definition" => {
            let name = first_identifier_text(node, lines).unwrap_or_else(|| "<var>".into());
            (OutlineKind::Variable, name, None)
        }

        // Properties (C#, Swift, Kotlin)
        "property_declaration" | "protocol_property_declaration" => {
            let name = find_child_text(node, "name", lines)
                .or_else(|| first_identifier_text(node, lines))
                .unwrap_or_else(|| "<property>".into());
            let (sig, end) = extract_signature(node, lines, lang);
            signature_end = Some(end);
            (OutlineKind::Property, name, Some(sig))
        }

        // Imports — collect as a group
        "import_statement"
        | "import_declaration"
        | "import"
        | "use_declaration"
        | "namespace_use_declaration"
        | "use_item"
        | "using_directive" => {
            let text = node_text(node, lines);
            (OutlineKind::Import, text, None)
        }

        // Exports — `export` is a modifier on a wrapped declaration, not a
        // peer of `function`/`class`/`const`. Recurse into the inner
        // declaration so the entry renders with its real kind. Falling back to
        // `OutlineKind::Export` only when there is no nameable declaration
        // inside (`export { … }`, `export * from …`, `export default <expr>`).
        // Without this, `export_statement`'s `name` is the full source span
        // (already starts with `export `), and the renderer prepends the
        // `Export` kind_label `"export"` again — producing the doubled-keyword
        // outline header `export export async function foo(`.
        "export_statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(mut inner) = node_to_entry(child, lines, lang, depth) {
                    // Extend the entry's range to cover the `export` keyword
                    // so the outline byte range still points at the statement.
                    inner.start_line = start_line;
                    return Some(inner);
                }
            }
            // No nameable inner declaration; strip leading `export ` so the
            // rendered name doesn't duplicate the `kind_label`.
            let raw = node_text(node, lines);
            let name = raw
                .strip_prefix("export ")
                .map(str::to_string)
                .unwrap_or(raw);
            (OutlineKind::Export, name, None)
        }

        // Module declarations
        "mod_item"
        | "module"
        | "namespace_declaration"
        | "namespace_definition"
        | "file_scoped_namespace_declaration" => {
            let name = find_child_text(node, "name", lines).unwrap_or_else(|| "<module>".into());
            (OutlineKind::Module, name, None)
        }

        // Elixir: all definitions are `call` nodes distinguished by target identifier
        "call" if lang == Lang::Elixir => {
            return elixir_call_to_entry(node, lines, lang, depth);
        }

        // Elixir: @type, @typep, @opaque are unary_operator nodes
        "unary_operator" if lang == Lang::Elixir => {
            return elixir_attr_to_entry(node, lines);
        }

        // Bash: top-level variable assignments (`MY_VAR=value`, `ARR[0]=value`)
        "variable_assignment" if lang == Lang::Bash => {
            let name = assignment_name(node, lines).unwrap_or_else(|| "<var>".into());
            (OutlineKind::Variable, name, None)
        }

        // Bash: top-level `export` / `declare` / `readonly` declarations. The name
        // is the `name` of the inner variable_assignment (`export FOO=bar`) or a
        // bare variable_name child (`export FOO`). Function-local `local`
        // declarations are nested in function bodies, so walk_top_level never
        // reaches them here. Multi-variable declarations surface their first name.
        "declaration_command" if lang == Lang::Bash => {
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find_map(|child| match child.kind() {
                    "variable_assignment" => assignment_name(child, lines),
                    "variable_name" => Some(node_text(child, lines)),
                    _ => None,
                })?;
            (OutlineKind::Variable, name, None)
        }

        _ => return None,
    };

    // Collect children for classes, impls, modules, traits/interfaces
    let is_namespace = matches!(
        kind_str,
        "namespace_declaration" | "namespace_definition" | "file_scoped_namespace_declaration"
    );
    let children = if matches!(
        kind,
        OutlineKind::Class | OutlineKind::Struct | OutlineKind::Module | OutlineKind::Interface
    ) && depth < 1
    {
        // Namespaces are transparent wrappers — don't consume a depth level,
        // so classes inside namespaces still collect their methods.
        let child_depth = if is_namespace { depth } else { depth + 1 };
        collect_children(node, lines, lang, child_depth)
    } else {
        Vec::new()
    };

    // Extract doc comment if present
    let doc = extract_doc(node, lines);

    Some(OutlineEntry {
        kind,
        name,
        start_line,
        end_line,
        signature,
        signature_end,
        children,
        doc,
    })
}

/// Collect child entries from a class/struct/impl body.
fn collect_children(
    node: tree_sitter::Node,
    lines: &[&str],
    lang: Lang,
    depth: usize,
) -> Vec<OutlineEntry> {
    let mut children = Vec::new();
    let mut cursor = node.walk();

    // Look for a body node first (C# uses `declaration_list` instead of `*_body`/`*_block`)
    let body = node.children(&mut cursor).find(|c| {
        let k = c.kind();
        k.contains("body") || k.contains("block") || k == "declaration_list"
    });

    let parent = body.unwrap_or(node);
    let mut cursor2 = parent.walk();

    for child in parent.children(&mut cursor2) {
        if let Some(entry) = node_to_entry(child, lines, lang, depth) {
            children.push(entry);
        }
    }

    children
}

/// Extract the first line as a function signature (name + params + return type).
fn extract_signature(
    node: tree_sitter::Node,
    lines: &[&str],
    lang: Lang,
) -> (String, SignatureEnd) {
    let start = node.start_position();
    let start = (start.row, start.column);
    let (display_end, source_end) = signature_positions(node, lines, lang);
    let source = source_text_between(lines, start, display_end);

    (
        compact_signature(&source, lang),
        SignatureEnd {
            line: source_end.0 as u32 + 1,
            column: source_end.1,
        },
    )
}

fn signature_positions(
    node: tree_sitter::Node,
    lines: &[&str],
    lang: Lang,
) -> ((usize, usize), (usize, usize)) {
    let start = node.start_position();
    let start = (start.row, start.column);

    if let Some(body) = declaration_body(node, lang) {
        let body_start = body.start_position();
        let body_start = (body_start.row, body_start.column);
        let intro_len = body_introducer_len(body, lines);

        let source_end = if intro_len > 0 {
            (body_start.0, body_start.1 + intro_len)
        } else if body_start.0 > start.0 {
            let row = body_start.0 - 1;
            (row, lines.get(row).map_or(0, |line| line.len()))
        } else {
            body_start
        };

        let display_end = if intro_len > 0 || body_start.0 == start.0 {
            body_start
        } else {
            source_end
        };
        return (display_end, source_end);
    }

    if lang == Lang::Elixir {
        if let Some(bounds) = elixir_keyword_body_positions(node, lines) {
            return bounds;
        }
    }

    let end = node.end_position();
    let end = (end.row, end.column);
    (end, end)
}

fn declaration_body(node: tree_sitter::Node, lang: Lang) -> Option<tree_sitter::Node> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body);
    }

    let mut cursor = node.walk();
    if lang == Lang::Elixir {
        if let Some(body) = node
            .children(&mut cursor)
            .find(|child| child.kind() == "do_block")
        {
            return Some(body);
        }
    }

    let mut cursor = node.walk();
    let body = node.children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "block"
                | "statement_block"
                | "compound_statement"
                | "function_body"
                | "function_block"
                | "body_statement"
                | "declaration_list"
                | "arrow_expression_clause"
                | "accessor_list"
                | "property_body"
        )
    });
    body
}

fn body_introducer_len(body: tree_sitter::Node, lines: &[&str]) -> usize {
    let start = body.start_position();
    let Some(line) = lines.get(start.row) else {
        return 0;
    };
    let Some(tail) = line.get(start.column..) else {
        return 0;
    };

    // This is checked only on a structurally selected body node. A brace in a
    // TypeScript return type is never considered a declaration body.
    if tail.starts_with('{') {
        1
    } else if (body.kind() == "do_block" && tail.starts_with("do"))
        || (body.kind() == "arrow_expression_clause" && tail.starts_with("=>"))
    {
        2
    } else {
        0
    }
}

fn elixir_keyword_body_positions(
    node: tree_sitter::Node,
    lines: &[&str],
) -> Option<((usize, usize), (usize, usize))> {
    let mut cursor = node.walk();
    let arguments = node
        .children(&mut cursor)
        .find(|child| child.kind() == "arguments")?;
    let mut cursor = arguments.walk();
    let keywords = arguments
        .children(&mut cursor)
        .find(|child| child.kind() == "keywords")?;
    let mut cursor = keywords.walk();
    let key = keywords
        .children(&mut cursor)
        .filter(|child| child.kind() == "pair")
        .filter_map(|pair| pair.child_by_field_name("key"))
        .find(|key| node_text(*key, lines).trim() == "do:")?;
    let start = key.start_position();
    let end = key.end_position();
    Some(((start.row, start.column), (end.row, end.column)))
}

fn source_text_between(lines: &[&str], start: (usize, usize), end: (usize, usize)) -> String {
    if start.0 > end.0 || end.0 >= lines.len() {
        return String::new();
    }

    let mut text = String::new();
    for row in start.0..=end.0 {
        let Some(line) = lines.get(row) else {
            break;
        };
        let from = if row == start.0 {
            start.1.min(line.len())
        } else {
            0
        };
        let to = if row == end.0 {
            end.1.min(line.len())
        } else {
            line.len()
        };
        if let Some(fragment) = line.get(from..to) {
            text.push_str(fragment);
        }
        if row < end.0 {
            text.push('\n');
        }
    }
    text
}

fn compact_signature(source: &str, lang: Lang) -> String {
    #[derive(Clone, Copy)]
    enum Quote {
        Ordinary(char),
        Triple(char),
        RustRaw(usize),
    }

    let mut signature = String::new();
    let mut chars = source.chars().peekable();
    let mut quote = None;
    let mut escaped = false;
    let mut pending_space = false;

    while let Some(ch) = chars.next() {
        if let Some(delimiter) = quote {
            signature.push(ch);
            match delimiter {
                Quote::RustRaw(hashes) => {
                    if ch == '"' && chars.clone().take(hashes).all(|next| next == '#') {
                        for _ in 0..hashes {
                            signature.push(chars.next().unwrap());
                        }
                        quote = None;
                    }
                }
                Quote::Ordinary(end) | Quote::Triple(end) => {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == end {
                        if matches!(delimiter, Quote::Triple(_)) {
                            if chars.clone().take(2).eq([end, end]) {
                                signature.push(chars.next().unwrap());
                                signature.push(chars.next().unwrap());
                                quote = None;
                            }
                        } else {
                            quote = None;
                        }
                    }
                }
            }
            continue;
        }

        if ch.is_whitespace() {
            pending_space = !signature.is_empty();
            continue;
        }

        if pending_space
            && !matches!(ch, ')' | ']' | '>' | '}' | ',' | ';' | '.' | ':')
            && !matches!(signature.chars().last(), Some('(' | '[' | '<' | '{'))
        {
            signature.push(' ');
        }
        pending_space = false;
        signature.push(ch);

        let rust_lifetime = ch == '\''
            && lang.has_lifetimes()
            && chars
                .peek()
                .is_some_and(|next| next.is_alphabetic() || *next == '_')
            && chars.clone().nth(1) != Some('\'');
        if rust_lifetime {
            continue;
        }

        if lang == Lang::Rust && ch == '"' {
            let prefix = &signature[..signature.len() - 1];
            let hashes = prefix
                .bytes()
                .rev()
                .take_while(|byte| *byte == b'#')
                .count();
            if prefix[..prefix.len() - hashes].ends_with('r') {
                quote = Some(Quote::RustRaw(hashes));
                continue;
            }
        }
        if lang == Lang::Python && matches!(ch, '"' | '\'') && chars.clone().take(2).eq([ch, ch]) {
            signature.push(chars.next().unwrap());
            signature.push(chars.next().unwrap());
            quote = Some(Quote::Triple(ch));
        } else if matches!(ch, '"' | '\'') || ch == char::from(96u8) {
            quote = Some(Quote::Ordinary(ch));
        }
    }

    if lang == Lang::Python {
        signature = signature
            .trim_end()
            .trim_end_matches(':')
            .trim_end()
            .to_string();
    }

    if signature.len() > 120 {
        format!("{}...", crate::types::truncate_str(&signature, 117))
    } else {
        signature
    }
}

/// Find a named child and return its text.
fn find_child_text(node: tree_sitter::Node, field: &str, lines: &[&str]) -> Option<String> {
    node.child_by_field_name(field).map(|n| node_text(n, lines))
}

/// Resolve the variable name from an assignment `name` field, unwrapping a
/// `subscript` (`ARR[0]=x`) to its base `variable_name` so the symbol
/// surfaces as `ARR`, not `ARR[0]`.
fn assignment_name(node: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    if name.kind() == "subscript" {
        let mut cursor = name.walk();
        let base = name
            .children(&mut cursor)
            .find(|c| c.kind() == "variable_name")
            .unwrap_or(name);
        Some(node_text(base, lines))
    } else {
        Some(node_text(name, lines))
    }
}

/// Get the text of a node, truncated to the first line.
fn node_text(node: tree_sitter::Node, lines: &[&str]) -> String {
    node_text_simple(node, lines, NodeTextMode::Truncated)
}

/// Find the first identifier-like child.
/// Recurses one level through declarators and `variable_declaration` nodes to find
/// the actual identifier inside wrapper nodes (e.g. Kotlin `property_declaration`
/// → `variable_declaration` → `simple_identifier`).
fn first_identifier_text(node: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind.contains("identifier") || kind.contains("name") {
            let text = node_text(child, lines);
            if !text.is_empty() {
                return Some(text);
            }
        }
        // Recurse one level through wrapper nodes (variable_declarator, variable_declaration)
        if kind.contains("declarator") || kind.contains("declaration") {
            let mut inner = child.walk();
            for grandchild in child.children(&mut inner) {
                if grandchild.kind().contains("identifier") {
                    let text = node_text(grandchild, lines);
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

/// Extract a doc comment from the previous sibling.
fn extract_doc(node: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let prev = node.prev_sibling()?;
    let kind = prev.kind();
    if kind.contains("comment") || kind.contains("doc") {
        let text = node_text(prev, lines);
        let trimmed = text
            .trim_start_matches("///")
            .trim_start_matches("//!")
            .trim_start_matches("/**")
            .trim_start_matches('#')
            .trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Elixir-specific outline helpers
// ---------------------------------------------------------------------------

/// Elixir function-like definition keywords that produce `OutlineKind::Function`.
/// This is the subset of definition keywords handled uniformly (extract function
/// name from arguments). Container keywords (`defmodule`, `defprotocol`, `defimpl`,
/// `defstruct`, `defexception`) have their own match arms in `elixir_call_to_entry`.
/// See also `ELIXIR_DEFINITION_TARGETS` in `elixir.rs` for the complete set.
const ELIXIR_DEF_KEYWORDS: &[&str] = &[
    "def",
    "defp",
    "defmacro",
    "defmacrop",
    "defguard",
    "defguardp",
    "defdelegate",
];

/// Convert an Elixir `call` node to an outline entry.
///
/// In the Elixir tree-sitter grammar, `defmodule`, `def`, `defp`, `defstruct`,
/// etc. are all `call` nodes whose `target` field is an identifier like `"def"`.
fn elixir_call_to_entry(
    node: tree_sitter::Node,
    lines: &[&str],
    lang: Lang,
    depth: usize,
) -> Option<OutlineEntry> {
    let target = node.child_by_field_name("target")?;
    let keyword = node_text(target, lines);
    let start_line = node.start_position().row as u32 + 1;
    let end_line = node.end_position().row as u32 + 1;

    let mut signature_end = None;
    let (kind, name, signature) = match keyword.as_str() {
        "defmodule" => {
            let name = elixir_first_arg_text(node, lines)?;
            (OutlineKind::Module, name, None)
        }
        kw if ELIXIR_DEF_KEYWORDS.contains(&kw) => {
            let name = elixir_func_name(node, lines)?;
            let (sig, end) = extract_signature(node, lines, lang);
            signature_end = Some(end);
            (OutlineKind::Function, name, Some(sig))
        }
        "defstruct" | "defexception" => (OutlineKind::Struct, keyword.clone(), None),
        "defprotocol" => {
            let name = elixir_first_arg_text(node, lines)?;
            (OutlineKind::Interface, name, None)
        }
        "defimpl" => {
            let name = elixir_first_arg_text(node, lines)?;
            (OutlineKind::Module, format!("impl {name}"), None)
        }
        "use" | "import" | "alias" | "require" => {
            let text = node_text(node, lines);
            (OutlineKind::Import, text, None)
        }
        _ => return None,
    };

    // Collect children for modules, protocols, impls
    let children = if matches!(kind, OutlineKind::Module | OutlineKind::Interface) && depth < 1 {
        elixir_collect_children(node, lines, lang, depth + 1)
    } else {
        Vec::new()
    };

    // Extract @doc / @moduledoc from previous sibling
    let doc = elixir_extract_doc(node, lines);

    Some(OutlineEntry {
        kind,
        name,
        start_line,
        end_line,
        signature,
        signature_end,
        children,
        doc,
    })
}

/// Convert an Elixir `unary_operator` node (`@type`, `@typep`, `@opaque`) to an outline entry.
fn elixir_attr_to_entry(node: tree_sitter::Node, lines: &[&str]) -> Option<OutlineEntry> {
    let operand = node.child_by_field_name("operand")?;
    if operand.kind() != "call" {
        return None;
    }
    let target = operand.child_by_field_name("target")?;
    let attr_name = node_text(target, lines);
    let start_line = node.start_position().row as u32 + 1;
    let end_line = node.end_position().row as u32 + 1;
    match attr_name.as_str() {
        "type" | "typep" | "opaque" => {
            let name = elixir_type_name(operand, lines)?;
            let sig = node_text(node, lines);
            Some(OutlineEntry {
                kind: OutlineKind::TypeAlias,
                name,
                start_line,
                end_line,
                signature: Some(sig),
                signature_end: None,
                children: Vec::new(),
                doc: None,
            })
        }
        "callback" | "macrocallback" => {
            let name = elixir_callback_name(operand, lines)?;
            let sig = node_text(node, lines);
            Some(OutlineEntry {
                kind: OutlineKind::Function,
                name,
                start_line,
                end_line,
                signature: Some(sig),
                signature_end: None,
                children: Vec::new(),
                doc: None,
            })
        }
        _ => None,
    }
}

/// Extract the first argument text from an Elixir call node.
/// For `defmodule Foo.Bar do ... end`, returns `"Foo.Bar"`.
fn elixir_first_arg_text(node: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let args = super::treesitter::elixir_arguments(node)?;
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if child.is_named() {
            return Some(node_text(child, lines));
        }
    }
    None
}

/// Extract function name from an Elixir `def`/`defp` call node.
///
/// For `def greet(name) do ... end`, the AST is:
///   call[target=def] → arguments → call[target=greet] → arguments → ...
/// For `def greet(name), do: ...` (keyword form), same structure.
fn elixir_func_name(node: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let args = super::treesitter::elixir_arguments(node)?;
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        return super::treesitter::elixir_extract_func_head_name(child, lines);
    }
    None
}

/// Extract type name from an Elixir `@type` call.
/// For `@type t :: %{...}`, the call operand is `type t :: %{...}`,
/// and we extract `t` from the first argument.
fn elixir_type_name(call: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let args = super::treesitter::elixir_arguments(call)?;
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        // `type t :: ...` → binary_operator with left=identifier
        if child.kind() == "binary_operator" {
            if let Some(left) = child.child_by_field_name("left") {
                // left may be a call like `t()` or an identifier `t`
                if left.kind() == "call" {
                    if let Some(target) = left.child_by_field_name("target") {
                        return Some(node_text(target, lines));
                    }
                }
                return Some(node_text(left, lines));
            }
        }
        // Bare identifier
        if child.kind() == "identifier" {
            return Some(node_text(child, lines));
        }
    }
    None
}

/// Extract callback name from an Elixir `@callback` call.
/// For `@callback handle_event(event :: term()) :: :ok`, the call operand is
/// `callback handle_event(...) :: :ok`. The arguments contain a `binary_operator`
/// with `::`, whose left side is a `call` with target = the callback name.
fn elixir_callback_name(call: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let args = super::treesitter::elixir_arguments(call)?;
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        if child.kind() == "binary_operator" {
            // `handle_event(...) :: return_type` → left is the function head
            if let Some(left) = child.child_by_field_name("left") {
                return super::treesitter::elixir_extract_func_head_name(left, lines);
            }
        }
        // Bare callback without return type spec (unlikely but handle it)
        return super::treesitter::elixir_extract_func_head_name(child, lines);
    }
    None
}

/// Collect child entries from an Elixir module/protocol/impl `do_block`.
///
/// This intentionally includes `use`/`alias`/`import`/`require` as import entries
/// inside module outlines. In Elixir these are structural — `use GenServer` injects
/// callbacks, `alias Foo.Bar` affects name resolution — so they provide useful
/// context alongside function definitions.
fn elixir_collect_children(
    node: tree_sitter::Node,
    lines: &[&str],
    lang: Lang,
    depth: usize,
) -> Vec<OutlineEntry> {
    let mut children = Vec::new();
    let mut cursor = node.walk();

    // Find the do_block child
    let Some(do_block) = node.children(&mut cursor).find(|c| c.kind() == "do_block") else {
        return children;
    };

    let mut cursor2 = do_block.walk();
    for child in do_block.children(&mut cursor2) {
        if let Some(entry) = node_to_entry(child, lines, lang, depth) {
            children.push(entry);
        }
    }

    children
}

/// Extract @doc or @moduledoc text from the previous sibling of an Elixir definition.
///
/// In Elixir, `@doc "text"` is a `unary_operator` node. We check if the
/// previous sibling is such a node and extract the string content.
fn elixir_extract_doc(node: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let prev = node.prev_sibling()?;
    if prev.kind() != "unary_operator" {
        return None;
    }
    let operand = prev.child_by_field_name("operand")?;
    if operand.kind() != "call" {
        return None;
    }
    let target = operand.child_by_field_name("target")?;
    let attr = node_text(target, lines);
    if attr != "doc" && attr != "moduledoc" {
        return None;
    }
    // Get the doc argument — use tree-sitter node types to handle all forms:
    //   `@doc "text"`           → string node
    //   `@doc """heredoc"""`    → string node (multi-line)
    //   `@doc ~S"""sigil"""`    → sigil node
    //   `@doc ~s"""sigil"""`    → sigil node
    //   `@doc false`            → boolean node (suppress docs)
    let args = super::treesitter::elixir_arguments(operand)?;
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        match child.kind() {
            // `@doc false` suppresses documentation
            "boolean" => return None,
            // Regular string (`"text"`, `"""heredoc"""`) or sigil (`~S"""..."""`, `~s"""..."""`)
            "string" | "sigil" => {
                return elixir_extract_doc_string(child, lines);
            }
            _ => {}
        }
    }
    None
}

/// Extract the first meaningful line from an Elixir doc string or sigil node.
///
/// For single-line strings (`"text"`), returns the content without quotes.
/// For heredocs/sigils (`"""..."""`, `~S"""..."""`), returns the first
/// non-empty content line. Uses tree-sitter source lines rather than
/// fragile string trimming.
fn elixir_extract_doc_string(node: tree_sitter::Node, lines: &[&str]) -> Option<String> {
    let start_row = node.start_position().row;
    let end_row = node.end_position().row;

    if start_row == end_row {
        // Single-line: `"text"` or `~s"text"` — strip delimiters and sigil prefix
        let mut text = node_text(node, lines);
        // Strip sigil prefix (~s, ~S, etc.) if present
        if text.starts_with('~') && text.len() >= 2 {
            text = text[2..].to_string();
        }
        let trimmed = text.trim_matches('"').trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed.to_string());
    }

    // Multi-line (heredoc or sigil): scan interior lines for first non-empty content
    for row in (start_row + 1)..end_row {
        if row >= lines.len() {
            break;
        }
        let line = lines[row].trim();
        if !line.is_empty() && line != "\"\"\"" {
            return Some(line.to_string());
        }
    }
    None
}

/// Extract the source module name from an import statement text.
/// Handles: `use std::fs;` → `std::fs`, `import X from "react"` → `react`,
/// `from collections import X` → `collections`
///
/// The `lang` parameter is needed to disambiguate `use` (Rust path vs Elixir module)
/// and `import` (JS/TS `from` syntax vs Elixir/Python/Go bare module name).
pub(crate) fn extract_import_source(text: &str, lang: Option<crate::types::Lang>) -> String {
    let trimmed = text.trim().trim_end_matches(';');

    // Bash: `source ./lib.sh`, `. ./lib.sh`, or tab-separated variants
    if lang == Some(crate::types::Lang::Bash) {
        let after = trimmed
            .strip_prefix("source")
            .or_else(|| trimmed.strip_prefix('.'))
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .map_or(trimmed, str::trim_start);
        // Skip variable-expanded paths (contain `$`)
        if after.contains('$') {
            return String::new();
        }
        return after.trim_matches(|c| c == '"' || c == '\'').to_string();
    }

    // Elixir: `use GenServer`, `import Kernel`, `alias Foo.Bar`, `require Logger`
    // Must be checked before the Rust `use` and JS `import` branches.
    if lang == Some(crate::types::Lang::Elixir) {
        for prefix in &["use ", "import ", "alias ", "require "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                return rest.split(',').next().unwrap_or(rest).trim().to_string();
            }
        }
        return trimmed.to_string();
    }

    // Rust: `use foo::bar` → `foo::bar`
    if let Some(rest) = trimmed.strip_prefix("use ") {
        return rest
            .split('{')
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_end_matches("::")
            .to_string();
    }

    // JS/TS: `import ... from "source"` or `import "source"`
    if trimmed.starts_with("import") {
        if let Some(from_pos) = trimmed.find("from ") {
            let source = &trimmed[from_pos + 5..];
            return source
                .trim()
                .trim_matches(|c| c == '"' || c == '\'' || c == ';')
                .to_string();
        }
        // Direct import: `import "source"`
        let after = trimmed.strip_prefix("import ").unwrap_or("");
        return after
            .trim()
            .trim_matches(|c| c == '"' || c == '\'' || c == ';')
            .to_string();
    }

    // Python: `from module import ...` or `import module`
    if let Some(rest) = trimmed.strip_prefix("from ") {
        return rest.split_whitespace().next().unwrap_or("").to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("import ") {
        return rest.split_whitespace().next().unwrap_or("").to_string();
    }

    // C/C++: #include "file.h" or #include <header>
    if let Some(rest) = trimmed.strip_prefix("#include") {
        return rest.trim().to_string(); // preserves quotes/angles for external detection
    }

    // Go: `import "source"` — already handled above via "import"
    // Fallback: first meaningful token
    trimmed
        .split_whitespace()
        .last()
        .unwrap_or(trimmed)
        .to_string()
}

/// Get structured outline entries for file content.
pub fn get_outline_entries(content: &str, lang: Lang) -> Vec<OutlineEntry> {
    let Some(ts_lang) = outline_language(lang) else {
        return Vec::new();
    };

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }

    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };

    let lines: Vec<&str> = content.lines().collect();
    walk_top_level(tree.root_node(), &lines, lang)
}

#[cfg(test)]
mod markdown_helper_tests {
    use super::{heading_level, heading_text, parse_markdown};

    /// Walk the tree and collect every `atx_heading`/`setext_heading` node.
    fn collect_headings(tree: &tree_sitter::Tree) -> Vec<tree_sitter::Node<'_>> {
        let mut out = Vec::new();
        let mut cursor = tree.walk();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if matches!(node.kind(), "atx_heading" | "setext_heading") {
                out.push(node);
            }
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        out.sort_by_key(|n| n.start_position().row);
        out
    }

    #[test]
    fn parse_returns_block_tree_with_sections() {
        let src = "# Top\n\ncontent\n\n## Sub\n\nmore\n";
        let tree = parse_markdown(src).unwrap();
        let root = tree.root_node();
        assert_eq!(root.kind(), "document");
        // The document contains at least one section node.
        let mut cursor = root.walk();
        let has_section = root.children(&mut cursor).any(|c| c.kind() == "section");
        assert!(has_section, "expected document to contain section children");
    }

    #[test]
    fn fenced_code_blocks_do_not_emit_headings() {
        // The whole point: a `# foo` inside a fenced code block must NOT be
        // parsed as an atx_heading. The hand-rolled scanners had to track
        // fence state manually; the AST does this for free.
        let src = "# Real\n\n```python\n# fake heading\nprint('x')\n```\n\n## Also Real\n";
        let tree = parse_markdown(src).unwrap();
        let headings = collect_headings(&tree);
        let lines: Vec<&str> = src.lines().collect();
        let texts: Vec<String> = headings.iter().map(|n| heading_text(*n, &lines)).collect();
        assert_eq!(texts, vec!["Real".to_string(), "Also Real".to_string()]);
    }

    #[test]
    fn tilde_fences_are_recognised() {
        let src = "# Real\n\n~~~\n# inside tilde fence\n~~~\n\n## Other\n";
        let tree = parse_markdown(src).unwrap();
        let lines: Vec<&str> = src.lines().collect();
        let headings = collect_headings(&tree);
        let texts: Vec<String> = headings.iter().map(|n| heading_text(*n, &lines)).collect();
        assert_eq!(texts, vec!["Real".to_string(), "Other".to_string()]);
    }

    #[test]
    fn level_extraction_covers_h1_through_h6() {
        let src = "# A\n\n## B\n\n### C\n\n#### D\n\n##### E\n\n###### F\n";
        let tree = parse_markdown(src).unwrap();
        let headings = collect_headings(&tree);
        let levels: Vec<u8> = headings.iter().filter_map(|n| heading_level(*n)).collect();
        assert_eq!(levels, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn trailing_atx_close_hashes_are_stripped() {
        let src = "## Foo ##\n";
        let tree = parse_markdown(src).unwrap();
        let lines: Vec<&str> = src.lines().collect();
        let headings = collect_headings(&tree);
        assert_eq!(heading_text(headings[0], &lines), "Foo");
    }
}

#[cfg(test)]
mod python_decorated_definition_tests {
    use super::get_outline_entries;
    use crate::types::{Lang, OutlineKind};

    #[test]
    fn decorated_definitions_keep_declaration_ranges_and_children() {
        let source = concat!(
            "def plain():\n",
            "    return 0\n",
            "\n",
            "# Handler documentation\n",
            "@decorator\n",
            "def decorated_handler(value):\n",
            "    return value\n",
            "\n",
            "class Service:\n",
            "    @property\n",
            "    def decorated_property(self):\n",
            "        return 1\n",
            "\n",
            "    @classmethod\n",
            "    def decorated_classmethod(cls):\n",
            "        return cls()\n",
            "\n",
            "    def normal_method(self):\n",
            "        return 2\n",
            "\n",
            "@decorator\n",
            "class DecoratedClass:\n",
            "    def nested_method(self):\n",
            "        return 3\n",
        );

        let entries = get_outline_entries(source, Lang::Python);
        let decorated_handler = entries
            .iter()
            .find(|entry| entry.name == "decorated_handler")
            .unwrap();
        assert_eq!(decorated_handler.kind, OutlineKind::Function);
        assert_eq!(
            (decorated_handler.start_line, decorated_handler.end_line),
            (6, 7)
        );
        assert_eq!(
            decorated_handler.signature.as_deref(),
            Some("def decorated_handler(value)")
        );
        assert_eq!(
            decorated_handler.doc.as_deref(),
            Some("Handler documentation")
        );

        let service = entries
            .iter()
            .find(|entry| entry.name == "Service")
            .unwrap();
        assert_eq!(service.kind, OutlineKind::Class);
        assert_eq!((service.start_line, service.end_line), (9, 19));
        assert_eq!(service.children.len(), 3);

        for (child, name, start_line, end_line) in [
            (&service.children[0], "decorated_property", 11, 12),
            (&service.children[1], "decorated_classmethod", 15, 16),
            (&service.children[2], "normal_method", 18, 19),
        ] {
            assert_eq!(child.name, name);
            assert_eq!(child.kind, OutlineKind::Function);
            assert_eq!((child.start_line, child.end_line), (start_line, end_line));
        }

        let decorated_class = entries
            .iter()
            .find(|entry| entry.name == "DecoratedClass")
            .unwrap();
        assert_eq!(decorated_class.kind, OutlineKind::Class);
        assert_eq!(
            (decorated_class.start_line, decorated_class.end_line),
            (22, 24)
        );
        assert_eq!(decorated_class.children.len(), 1);
        let nested_method = &decorated_class.children[0];
        assert_eq!(nested_method.name, "nested_method");
        assert_eq!((nested_method.start_line, nested_method.end_line), (23, 24));
    }
}

#[cfg(test)]
mod bash_outline_tests {
    use super::{extract_import_source, get_outline_entries};
    use crate::search::callees::extract_callee_names;
    use crate::types::{Lang, OutlineKind};

    // Fixture covering both function syntaxes, top-level vars, and a nested local.
    const BASH_FIXTURE: &str = r#"MY_CONST=hello
DEBUG_MODE=0

greet() { echo "hi $1"; }

function cleanup {
    rm -f /tmp/x
}

main() {
    greet world
    cleanup
    local y=1
}
"#;

    #[test]
    fn bash_outline_functions_and_vars() {
        let entries = get_outline_entries(BASH_FIXTURE, Lang::Bash);

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        // All three functions must appear
        assert!(
            names.contains(&"greet"),
            "expected greet in outline, got: {names:?}"
        );
        assert!(
            names.contains(&"cleanup"),
            "expected cleanup in outline, got: {names:?}"
        );
        assert!(
            names.contains(&"main"),
            "expected main in outline, got: {names:?}"
        );

        // Top-level variables must appear
        assert!(
            names.contains(&"MY_CONST"),
            "expected MY_CONST in outline, got: {names:?}"
        );
        assert!(
            names.contains(&"DEBUG_MODE"),
            "expected DEBUG_MODE in outline, got: {names:?}"
        );

        // Functions must have Function kind
        for fname in &["greet", "cleanup", "main"] {
            let entry = entries.iter().find(|e| e.name == *fname).unwrap();
            assert_eq!(
                entry.kind,
                OutlineKind::Function,
                "{fname} should be OutlineKind::Function"
            );
        }

        // Variables must have Variable kind
        for vname in &["MY_CONST", "DEBUG_MODE"] {
            let entry = entries.iter().find(|e| e.name == *vname).unwrap();
            assert_eq!(
                entry.kind,
                OutlineKind::Variable,
                "{vname} should be OutlineKind::Variable"
            );
        }

        // Nested `local y=1` must NOT appear at the top level
        assert!(
            !names.contains(&"y"),
            "nested local 'y' must not appear in top-level outline, got: {names:?}"
        );
    }

    #[test]
    fn bash_callee_names_for_main() {
        // Derive main's range from the outline so the test can't silently drift
        // if the fixture is edited.
        let main = get_outline_entries(BASH_FIXTURE, Lang::Bash)
            .into_iter()
            .find(|e| e.name == "main")
            .expect("main must be in the outline");
        let names = extract_callee_names(
            BASH_FIXTURE,
            Lang::Bash,
            Some((main.start_line, main.end_line)),
        );

        assert!(
            names.contains(&"greet".to_string()),
            "expected greet as callee, got: {names:?}"
        );
        assert!(
            names.contains(&"cleanup".to_string()),
            "expected cleanup as callee, got: {names:?}"
        );
        // echo is called inside greet, outside main's range, so it is absent here.
    }

    #[test]
    fn bash_outline_surfaces_declarations_and_hyphenated_names() {
        // export/declare/readonly declarations must surface (the common config
        // pattern), and hyphenated function names must be captured whole.
        let src = "export E_VAR=1\n\
                   declare -r D_VAR=2\n\
                   readonly R_VAR=3\n\
                   deploy-app() { :; }\n";
        let entries = get_outline_entries(src, Lang::Bash);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        for v in ["E_VAR", "D_VAR", "R_VAR"] {
            let e = entries
                .iter()
                .find(|e| e.name == v)
                .unwrap_or_else(|| panic!("{v} should be outlined, got: {names:?}"));
            assert_eq!(e.kind, OutlineKind::Variable, "{v} should be a Variable");
        }
        let dep = entries
            .iter()
            .find(|e| e.name == "deploy-app")
            .unwrap_or_else(|| panic!("deploy-app should be outlined whole, got: {names:?}"));
        assert_eq!(dep.kind, OutlineKind::Function);
    }

    #[test]
    fn bash_extract_import_source_source_keyword() {
        let line = "source ./lib/utils.sh";
        let result = extract_import_source(line, Some(Lang::Bash));
        assert_eq!(result, "./lib/utils.sh");
    }

    #[test]
    fn bash_extract_import_source_dot_keyword() {
        let line = ". ./config.sh";
        let result = extract_import_source(line, Some(Lang::Bash));
        assert_eq!(result, "./config.sh");
    }

    #[test]
    fn bash_extract_import_source_quoted() {
        let line = r#"source "./lib/helpers.sh""#;
        let result = extract_import_source(line, Some(Lang::Bash));
        assert_eq!(result, "./lib/helpers.sh");
    }

    #[test]
    fn bash_extract_import_source_variable_expanded_returns_empty() {
        let line = r#"source "$DIR/lib.sh""#;
        let result = extract_import_source(line, Some(Lang::Bash));
        assert!(
            result.is_empty(),
            "variable-expanded source should return empty, got: {result:?}"
        );
    }

    #[test]
    fn bash_subscript_assignment_surfaces_base_name() {
        // `ARR[0]=hello` should appear as `ARR` (Variable), not `ARR[0]`.
        let entries = get_outline_entries("ARR[0]=hello\n", Lang::Bash);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"ARR"),
            "expected ARR in outline, got: {names:?}"
        );
        assert!(
            !names.contains(&"ARR[0]"),
            "ARR[0] must not appear verbatim in outline, got: {names:?}"
        );
        let entry = entries.iter().find(|e| e.name == "ARR").unwrap();
        assert_eq!(
            entry.kind,
            OutlineKind::Variable,
            "ARR should be OutlineKind::Variable"
        );
    }

    #[test]
    fn bash_extract_import_source_tab_separated() {
        // `source\t./lib.sh` (tab separator) must be parsed correctly.
        let result = extract_import_source("source\t./lib/utils.sh", Some(Lang::Bash));
        assert_eq!(result, "./lib/utils.sh");
    }
}

#[cfg(test)]
mod signature_boundary_regression_tests {
    use super::get_outline_entries;
    use crate::types::Lang;

    #[test]
    fn quoted_python_defaults_keep_whitespace_and_punctuation() {
        for (old_default, new_default) in [
            ("a  b", "a b"),
            ("a , b", "a, b"),
            ("[ a ]", "[a]"),
            ("a : b", "a: b"),
        ] {
            let old = format!("def f(value: str = \"{old_default}\"):\n    return value\n");
            let new = format!("def f(value: str = \"{new_default}\"):\n    return value\n");
            let old_sig = get_outline_entries(&old, Lang::Python)[0]
                .signature
                .clone()
                .unwrap();
            let new_sig = get_outline_entries(&new, Lang::Python)[0]
                .signature
                .clone()
                .unwrap();
            assert!(old_sig.contains(&format!("\"{old_default}\"")), "{old_sig}");
            assert_ne!(old_sig, new_sig, "{old_default:?} vs {new_default:?}");
        }
    }

    #[test]
    fn multiline_signatures_stay_compact_without_changing_literals() {
        let source =
            "def f(\n    value: str = \"a  b\",\n    count: int = 1,\n):\n    return value\n";
        let signature = get_outline_entries(source, Lang::Python)[0]
            .signature
            .clone()
            .unwrap();
        assert_eq!(signature, "def f(value: str = \"a  b\", count: int = 1,)");
    }

    #[test]
    fn rust_lifetimes_remain_outside_quoted_literals() {
        let source = "fn borrow<'a>(value: &'a str) -> &'a str { value }\n";
        let signature = get_outline_entries(source, Lang::Rust)[0]
            .signature
            .clone()
            .unwrap();
        assert_eq!(signature, "fn borrow<'a>(value: &'a str) -> &'a str");
    }

    #[test]
    fn elixir_keyword_body_uses_structural_key_position() {
        let multiline = "def g(x),\n  do: (\n    IO.puts(x)\n    x + 123\n  )\n";
        let entry = &get_outline_entries(multiline, Lang::Elixir)[0];
        assert_eq!(entry.signature.as_deref(), Some("def g(x),"));
        let end = entry.signature_end.unwrap();
        assert_eq!((end.line, end.column), (2, 6));

        let quoted = "def f(x \\\\ \", do:\"), do: x\n";
        let entry = &get_outline_entries(quoted, Lang::Elixir)[0];
        assert_eq!(entry.signature.as_deref(), Some("def f(x \\\\ \", do:\"),"));
        let end = entry.signature_end.unwrap();
        assert_eq!(
            (end.line, end.column),
            (1, quoted.find("do: x").unwrap() + 4)
        );
    }
}

#[cfg(test)]
mod signature_change_classification_regression_tests {
    use super::get_outline_entries;
    use crate::diff::matching::{build_diff_symbols, match_symbols};
    use crate::diff::ChangeType;
    use crate::types::Lang;

    #[test]
    fn quoted_default_whitespace_is_classified_as_signature_change() {
        let old = "def f(value: str = \"a  b\"):\n    return value\n";
        let new = "def f(value: str = \"a b\"):\n    return value\n";
        let old_entries = get_outline_entries(old, Lang::Python);
        let new_entries = get_outline_entries(new, Lang::Python);
        let old_symbols = build_diff_symbols(&old_entries, old, Lang::Python);
        let new_symbols = build_diff_symbols(&new_entries, new, Lang::Python);

        let changes = match_symbols(&old_symbols, &new_symbols);
        assert_eq!(changes.len(), 1);
        assert!(
            matches!(changes[0].change, ChangeType::SignatureChanged),
            "{changes:?}"
        );
    }
}

#[cfg(test)]
mod extended_literal_signature_regression_tests {
    use super::get_outline_entries;
    use crate::types::Lang;

    #[test]
    fn python_triple_quoted_default_keeps_embedded_quote_and_spaces() {
        let source = "def f(x = \"\"\"a\"  b\"\"\"):\n    return x\n";
        let signature = get_outline_entries(source, Lang::Python)[0]
            .signature
            .clone()
            .unwrap();
        assert!(signature.contains("\"\"\"a\"  b\"\"\""), "{signature}");
    }

    #[test]
    fn rust_raw_string_default_keeps_embedded_quote_and_spaces() {
        let signature = super::compact_signature("fn f(x: &str = r#\"a\"  b\"#)", Lang::Rust);
        assert!(signature.contains("r#\"a\"  b\"#"), "{signature}");
    }
    #[test]
    fn python_raw_default_keeps_escaped_quote_and_spaces() {
        let source = r#"def f(x = r"a\"  b"):
    return x
"#;
        let signature = get_outline_entries(source, Lang::Python)[0]
            .signature
            .clone()
            .unwrap();
        assert!(signature.contains(r#"r"a\"  b""#), "{signature}");
    }

    #[test]
    fn rust_raw_string_with_multiple_hashes_keeps_quote_and_spaces() {
        let signature = super::compact_signature("fn f(x = r##\"a\"#  b\"##)", Lang::Rust);
        assert!(signature.contains("r##\"a\"#  b\"##"), "{signature}");
    }
}
