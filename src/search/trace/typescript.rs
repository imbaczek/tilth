//! Bounded JS/TS receiver lookup over files already admitted to the trace graph.
//! No name-only fallback is allowed for a member expression.
use super::extract::Call;
use crate::{cache::ParsedFile, lang::declarations::Declaration, types::Lang};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tree_sitter::Node;

type Sources = HashMap<PathBuf, Arc<ParsedFile>>;
pub(super) struct Index {
    work: std::cell::Cell<usize>,
    uncertain_export: std::cell::Cell<bool>,
    members: HashMap<(PathBuf, usize, usize), Vec<usize>>,
    writes: HashMap<PathBuf, Vec<(usize, String)>>,
}
impl Index {
    pub(super) fn new(sources: &Sources, nodes: &[Declaration]) -> Self {
        let mut members = HashMap::<_, Vec<_>>::new();
        for (id, node) in nodes.iter().enumerate() {
            if supported(node.parsed.lang) {
                if let Some((start, end)) = node.span {
                    members
                        .entry((node.path.clone(), start, end))
                        .or_default()
                        .push(id);
                }
            }
        }
        let mut writes = HashMap::new();
        for (path, parsed) in sources {
            let mut sites = Vec::new();
            for node in crate::lang::walk::named_descendants(parsed.tree.root_node()) {
                if matches!(
                    node.kind(),
                    "assignment_expression"
                        | "augmented_assignment_expression"
                        | "update_expression"
                ) {
                    if let Some(left) = node
                        .child_by_field_name("left")
                        .or_else(|| node.child_by_field_name("argument"))
                    {
                        sites.push((node.start_byte(), text(left, parsed).to_owned()));
                        if matches!(left.kind(), "object_pattern" | "array_pattern") {
                            for binding in crate::lang::walk::named_descendants(left) {
                                if matches!(
                                    binding.kind(),
                                    "identifier" | "shorthand_property_identifier_pattern"
                                ) {
                                    sites.push((
                                        node.start_byte(),
                                        text(binding, parsed).to_owned(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            writes.insert(path.clone(), sites);
        }
        Self {
            members,
            writes,
            work: std::cell::Cell::new(0),
            uncertain_export: std::cell::Cell::new(false),
        }
    }
}
#[derive(Clone, Copy)]
struct Class<'a> {
    path: &'a Path,
    node: Node<'a>,
    parsed: &'a ParsedFile,
}

pub(super) fn supported(lang: Lang) -> bool {
    matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript)
}
fn text<'a>(node: Node, parsed: &'a ParsedFile) -> &'a str {
    node.utf8_text(parsed.content.as_bytes()).unwrap_or("")
}
fn is_class(node: Node) -> bool {
    matches!(
        node.kind(),
        "class_declaration" | "abstract_class_declaration" | "class"
    )
}
fn is_static(node: Node) -> bool {
    let mut c = node.walk();
    let found = node.children(&mut c).any(|n| n.kind() == "static");
    found
}
fn class_context<'a>(
    mut node: Node<'a>,
    path: &'a Path,
    parsed: &'a ParsedFile,
) -> Option<(Class<'a>, bool)> {
    let mut static_context = false;
    loop {
        match node.kind() {
            // Arrows retain lexical this. Ordinary nested functions do not.
            "function_expression"
            | "function_declaration"
            | "generator_function"
            | "generator_function_declaration" => return None,
            "method_definition" | "public_field_definition" | "field_definition" => {
                if node
                    .parent()
                    .is_none_or(|parent| parent.kind() != "class_body")
                {
                    return None;
                }
                static_context = is_static(node);
            }
            "class_static_block" => static_context = true,
            _ => {}
        }
        if is_class(node) {
            return Some((Class { path, node, parsed }, static_context));
        }
        node = node.parent()?;
    }
}
fn base(class: Class<'_>) -> Option<(Node<'_>, &str)> {
    let mut cursor = class.node.walk();
    let heritage = class
        .node
        .named_children(&mut cursor)
        .find(|n| n.kind() == "class_heritage")?;
    let mut cursor = heritage.walk();
    let clause = heritage
        .named_children(&mut cursor)
        .find(|n| n.kind() == "extends_clause")?;
    let value = clause
        .child_by_field_name("value")
        .or_else(|| clause.named_child(0))?;
    let value = if value.kind() == "generic_type" {
        value.child_by_field_name("name")?
    } else {
        value
    };
    Some((value, text(value, class.parsed)))
}
fn local_class<'a>(
    sources: &'a Sources,
    index: &Index,
    path: &'a Path,
    at: Node<'a>,
    name: &str,
    depth: usize,
) -> Option<Class<'a>> {
    if depth >= 16 || index.work.get() == 0 {
        return None;
    }
    index.work.set(index.work.get() - 1);
    let parsed = sources.get(path)?;
    // A lexical value binding blocks any same-named import/class fallback.
    if let Some(binding) =
        crate::search::grok::ts_local_binding(at, name, parsed.content.as_bytes())
    {
        return is_class(binding).then_some(Class {
            path,
            node: binding,
            parsed,
        });
    }
    let (head, member) = name.split_once('.').unwrap_or((name, ""));
    let mut cursor = parsed.tree.root_node().walk();
    for statement in parsed.tree.root_node().named_children(&mut cursor) {
        if statement.kind() != "import_statement" {
            continue;
        }
        let source = statement.child_by_field_name("source")?;
        let source = text(source, parsed).trim_matches(['\'', '"']);
        for (local, imported) in crate::read::imports::js_import_bindings(text(statement, parsed)) {
            if local != head {
                continue;
            }
            let imported_path =
                crate::read::imports::resolve_import_source(path, source, parsed.lang)?;
            let (stored_path, _) = sources.get_key_value(&imported_path)?;
            let exported = if imported == "*" {
                member
            } else if member.is_empty() {
                &imported
            } else {
                return None;
            };
            return exported_class(sources, index, stored_path, exported, depth + 1);
        }
    }
    None
}
fn exported_class<'a>(
    sources: &'a Sources,
    index: &Index,
    path: &'a Path,
    name: &str,
    depth: usize,
) -> Option<Class<'a>> {
    if depth >= 16 || index.work.get() == 0 {
        return None;
    }
    index.work.set(index.work.get() - 1);
    let parsed = sources.get(path)?;
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if statement.kind() != "export_statement" {
            continue;
        }
        let mut children = statement.walk();
        for child in statement.named_children(&mut children) {
            if is_class(child)
                && (child
                    .child_by_field_name("name")
                    .is_some_and(|n| text(n, parsed) == name)
                    || (name == "default"
                        && text(statement, parsed)
                            .trim_start()
                            .starts_with("export default")))
            {
                return Some(Class {
                    path,
                    node: child,
                    parsed,
                });
            }
            if child
                .child_by_field_name("name")
                .is_some_and(|n| text(n, parsed) == name)
                || (name == "default"
                    && text(statement, parsed)
                        .trim_start()
                        .starts_with("export default"))
            {
                index.uncertain_export.set(true);
                return None;
            }
            if matches!(child.kind(), "lexical_declaration" | "variable_declaration")
                && crate::lang::walk::named_descendants(child).any(|n| {
                    n.kind() == "variable_declarator"
                        && n.child_by_field_name("name")
                            .is_some_and(|n| text(n, parsed) == name)
                })
            {
                index.uncertain_export.set(true);
                return None;
            }
        }
        for (local, exported) in crate::read::imports::js_export_bindings(text(statement, parsed)) {
            if exported != name {
                continue;
            }
            if let Some(source) = statement.child_by_field_name("source") {
                let imported_path = crate::read::imports::resolve_import_source(
                    path,
                    text(source, parsed).trim_matches(['\'', '"']),
                    parsed.lang,
                )?;
                let (stored_path, _) = sources.get_key_value(&imported_path)?;
                return exported_class(sources, index, stored_path, &local, depth + 1);
            }
            let result = local_class(sources, index, path, statement, &local, depth + 1);
            if result.is_none() {
                index.uncertain_export.set(true);
            }
            return result;
        }
    }
    if name == "default" {
        return None;
    }
    let mut found: Option<Class<'a>> = None;
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if statement.kind() != "export_statement" {
            continue;
        }
        let mut tokens = statement.walk();
        if !statement.children(&mut tokens).any(|n| n.kind() == "*") {
            continue;
        }
        let mut children = statement.walk();
        if statement
            .named_children(&mut children)
            .any(|n| n.kind() == "namespace_export")
        {
            continue;
        }
        let Some(source) = statement.child_by_field_name("source") else {
            continue;
        };
        let Some(imported_path) = crate::read::imports::resolve_import_source(
            path,
            text(source, parsed).trim_matches(['\'', '"']),
            parsed.lang,
        ) else {
            index.uncertain_export.set(true);
            continue;
        };
        let Some((stored_path, _)) = sources.get_key_value(&imported_path) else {
            index.uncertain_export.set(true);
            continue;
        };
        if let Some(candidate) = exported_class(sources, index, stored_path, name, depth + 1) {
            if found.is_some_and(|old| {
                old.path != candidate.path || old.node.id() != candidate.node.id()
            }) {
                index.uncertain_export.set(true);
                return None;
            }
            found = Some(candidate);
        }
    }
    found
}
fn lookup(
    sources: &Sources,
    index: &Index,
    class: Class<'_>,
    method: &str,
    static_call: bool,
    depth: usize,
) -> Result<Vec<usize>, &'static str> {
    if depth >= 16 {
        return Err("receiver inheritance depth limit reached");
    }
    let Some(body) = class.node.child_by_field_name("body") else {
        return Err("receiver class body is unavailable");
    };
    let mut cursor = body.walk();
    for member in body.named_children(&mut cursor) {
        if member
            .child_by_field_name("name")
            .is_none_or(|n| text(n, class.parsed) != method)
            || is_static(member) != static_call
        {
            continue;
        }
        // A field or accessor shadows an inherited method but is not a direct method call.
        let mut tokens = member.walk();
        let accessor = member
            .children(&mut tokens)
            .any(|n| matches!(n.kind(), "get" | "set"));
        if accessor
            || member.kind() != "method_definition"
            || member.child_by_field_name("body").is_none()
        {
            continue;
        }
        let ids = index
            .members
            .get(&(
                class.path.to_owned(),
                member.start_byte(),
                member.end_byte(),
            ))
            .cloned()
            .unwrap_or_default();
        return if ids.is_empty() {
            Err("receiver member has no indexed body")
        } else {
            Ok(ids)
        };
    }
    // Do not walk through a same-name field, abstract declaration, or accessor.
    let mut cursor = body.walk();
    if body.named_children(&mut cursor).any(|n| {
        is_static(n) == static_call
            && n.child_by_field_name("name")
                .is_some_and(|n| text(n, class.parsed) == method)
    }) {
        return Err("receiver member is not a direct indexed method");
    }
    if let Some((at, name)) = base(class) {
        let base = local_class(sources, index, class.path, at, name, 0)
            .ok_or("receiver base class is outside scope or unresolved")?;
        return lookup(sources, index, base, method, static_call, depth + 1);
    }
    Err("receiver method is outside scope or unresolved")
}
fn resolve_member_inner(
    sources: &Sources,
    index: &Index,
    owner: &Declaration,
    call: &Call,
) -> Result<Vec<usize>, &'static str> {
    let parsed = &owner.parsed;
    let at = parsed
        .tree
        .root_node()
        .descendant_for_byte_range(call.byte, call.byte + 1)
        .ok_or("receiver syntax is unavailable")?;
    let receiver = call
        .receiver
        .as_deref()
        .ok_or("receiver syntax is unavailable")?;
    let (class, static_call) = if receiver == "this" || receiver == "super" {
        let (class, static_context) = class_context(at, &owner.path, parsed)
            .ok_or("receiver this binding is dynamic or unavailable")?;
        if receiver == "super" {
            let (at, name) = base(class).ok_or("receiver base class is unavailable")?;
            (
                local_class(sources, index, class.path, at, name, 0)
                    .ok_or("receiver base class is outside scope or unresolved")?,
                static_context,
            )
        } else {
            (class, static_context)
        }
    } else {
        let object = at
            .parent()
            .and_then(|n| n.child_by_field_name("object"))
            .ok_or("receiver expression is unsupported")?;
        if let Some((ty, site)) =
            crate::search::callers::js_receiver_type(object, at, parsed.content.as_bytes())
        {
            if ty.is_empty() {
                return Err("receiver binding has no known class");
            }
            if index.writes.get(&owner.path).is_some_and(|writes| {
                writes.iter().any(|(byte, name)| {
                    site < *byte
                        && *byte < call.byte
                        && (name == receiver || receiver.starts_with(&format!("{name}.")))
                })
            }) {
                return Err("receiver binding is reassigned; target unresolved");
            }
            let binding = parsed
                .tree
                .root_node()
                .descendant_for_byte_range(site, site + 1)
                .ok_or("receiver binding is unavailable")?;
            let mut declaration = Some(binding);
            while let Some(node) = declaration {
                if matches!(
                    node.kind(),
                    "variable_declarator"
                        | "required_parameter"
                        | "optional_parameter"
                        | "public_field_definition"
                        | "field_definition"
                ) {
                    let mut cursor = node.walk();
                    if node.named_children(&mut cursor).any(|n| {
                        n.kind() == "type_annotation"
                            && n.named_child(0).is_some_and(|ty| {
                                !matches!(
                                    ty.kind(),
                                    "type_identifier" | "nested_type_identifier" | "generic_type"
                                )
                            })
                    }) {
                        return Err("receiver type is compound or dynamic; target unresolved");
                    }
                    break;
                }
                if matches!(node.kind(), "statement_block" | "program") {
                    break;
                }
                declaration = node.parent();
            }
            (
                local_class(sources, index, &owner.path, binding, &ty, 0)
                    .ok_or("receiver type is outside scope or unresolved")?,
                false,
            )
        } else {
            if index.writes.get(&owner.path).is_some_and(|writes| {
                writes
                    .iter()
                    .any(|(byte, name)| *byte < call.byte && name == receiver)
            }) {
                return Err("receiver class binding is reassigned; target unresolved");
            }
            (
                local_class(sources, index, &owner.path, at, receiver, 0)
                    .ok_or("receiver class is outside scope or unresolved")?,
                true,
            )
        }
    };
    lookup(sources, index, class, &call.name, static_call, 0)
}

pub(super) fn resolve_member(
    sources: &Sources,
    index: &Index,
    owner: &Declaration,
    call: &Call,
) -> Result<Vec<usize>, &'static str> {
    index.work.set(256);
    index.uncertain_export.set(false);
    let result = resolve_member_inner(sources, index, owner, call);
    if index.work.get() == 0 {
        Err("receiver resolution work limit reached")
    } else if index.uncertain_export.get() {
        Err("receiver export is ambiguous or unresolved")
    } else {
        result
    }
}

/// Trace identities for deferred callable bodies absent from the public outline.
/// Exact expression spans keep registration/initialization separate from execution.
pub(super) fn declarations(
    path: &Path,
    parsed: &Arc<ParsedFile>,
    limit: usize,
) -> Vec<Declaration> {
    let mut declarations = crate::lang::declarations::collect(path, parsed);
    let original_len = declarations.len();
    let mut spans: std::collections::HashSet<_> = declarations
        .iter()
        .filter(|d| d.entry.kind == crate::types::OutlineKind::Function)
        .filter_map(|d| d.span)
        .collect();
    let mut owners: HashMap<_, _> = declarations
        .iter()
        .filter(|d| {
            matches!(
                d.entry.kind,
                crate::types::OutlineKind::Function | crate::types::OutlineKind::Class
            )
        })
        .filter_map(|d| d.span.map(|s| (s, d.qualified.clone())))
        .collect();
    let mut count = spans.len();
    let lines: Vec<_> = parsed.content.lines().collect();
    for node in crate::lang::walk::named_descendants(parsed.tree.root_node()) {
        if !matches!(
            node.kind(),
            "arrow_function"
                | "function_expression"
                | "generator_function"
                | "function_declaration"
                | "generator_function_declaration"
        ) {
            continue;
        }
        let span = (node.start_byte(), node.end_byte());
        if !spans.insert(span) {
            continue;
        }
        if count >= limit {
            break;
        }
        count += 1;
        let line = node.start_position().row as u32 + 1;
        let column = node.start_position().column + 1;
        let bound_name = node
            .child_by_field_name("name")
            .map(|n| text(n, parsed).to_owned())
            .or_else(|| {
                node.parent()
                    .filter(|n| n.kind() == "variable_declarator")
                    .and_then(|n| n.child_by_field_name("name"))
                    .filter(|n| n.kind() == "identifier")
                    .map(|n| text(n, parsed).to_owned())
            });
        let name = bound_name.unwrap_or_else(|| {
            let label =
                crate::search::scope::walk_to_enclosing_definition(node, &lines, parsed.lang)
                    .map_or_else(|| "<anonymous>".into(), |(_, name, _)| name);
            format!("{label} [{line}:{column}]")
        });
        let mut ancestor = node.parent();
        let mut parent = None;
        while let Some(n) = ancestor {
            if let Some(owner) = owners.get(&(n.start_byte(), n.end_byte())) {
                parent = Some(owner);
                break;
            }
            ancestor = n.parent();
        }
        let qualified = parent.map_or_else(|| name.clone(), |p| format!("{p}::{name}"));
        owners.insert(span, qualified.clone());
        declarations.push(Declaration {
            path: path.to_owned(),
            qualified,
            entry: crate::types::OutlineEntry {
                kind: crate::types::OutlineKind::Function,
                name,
                start_line: line,
                end_line: node.end_position().row as u32 + 1,
                signature: None,
                signature_end: None,
                children: Vec::new(),
                doc: None,
            },
            parsed: parsed.clone(),
            span: Some(span),
        });
    }
    let mut counts = HashMap::<String, usize>::new();
    for d in &declarations {
        *counts.entry(d.qualified.clone()).or_default() += 1;
    }
    for d in declarations.iter_mut().skip(original_len) {
        if counts.get(&d.qualified).copied().unwrap_or(0) > 1 {
            if let Some((start, end)) = d.span {
                if let Some(node) = parsed
                    .tree
                    .root_node()
                    .descendant_for_byte_range(start, end)
                {
                    d.qualified = format!(
                        "{} [{}:{}]",
                        d.qualified,
                        node.start_position().row + 1,
                        node.start_position().column + 1
                    );
                }
            }
        }
    }
    declarations
}

/// Deferred/local bodies must never become global bare-name candidates.
pub(super) fn bare_candidates(
    nodes: &[Declaration],
    index: &Index,
    owner: &Declaration,
    call: &Call,
    candidates: &[usize],
) -> Result<Vec<usize>, &'static str> {
    let parsed = &owner.parsed;
    let at = parsed
        .tree
        .root_node()
        .descendant_for_byte_range(call.byte, call.byte + 1)
        .ok_or("callable syntax is unavailable")?;
    let mut ancestor = at.parent();
    while let Some(node) = ancestor {
        if matches!(node.kind(), "function_expression" | "generator_function")
            && node
                .child_by_field_name("name")
                .is_some_and(|n| text(n, parsed) == call.name)
        {
            return Ok(index
                .members
                .get(&(owner.path.clone(), node.start_byte(), node.end_byte()))
                .cloned()
                .unwrap_or_default());
        }
        ancestor = node.parent();
    }
    if let Some(binding) =
        crate::search::grok::ts_local_binding(at, &call.name, parsed.content.as_bytes())
    {
        if index.writes.get(&owner.path).is_some_and(|writes| {
            writes.iter().any(|(byte, name)| {
                binding.start_byte() < *byte && *byte < call.byte && name == &call.name
            })
        }) {
            return Err("lexically bound callable is reassigned; target unresolved");
        }
        if binding.kind() == "variable_declarator" && binding.start_byte() > call.byte {
            return Err("lexically bound callable is declared later; invocation unresolved");
        }
        let callable = if matches!(
            binding.kind(),
            "function_declaration" | "generator_function_declaration"
        ) {
            Some(binding)
        } else {
            binding.child_by_field_name("value").filter(|n| {
                matches!(
                    n.kind(),
                    "arrow_function" | "function_expression" | "generator_function"
                )
            })
        };
        return callable
            .map(|n| {
                index
                    .members
                    .get(&(owner.path.clone(), n.start_byte(), n.end_byte()))
                    .cloned()
                    .unwrap_or_default()
            })
            .ok_or("lexically bound callable is dynamic or unindexed");
    }
    Ok(candidates
        .iter()
        .copied()
        .filter(|id| {
            let declaration = &nodes[*id];
            let Some((start, end)) = declaration.span else {
                return false;
            };
            let Some(node) = declaration
                .parsed
                .tree
                .root_node()
                .descendant_for_byte_range(start, end)
            else {
                return false;
            };
            if matches!(
                node.kind(),
                "arrow_function" | "function_expression" | "generator_function"
            ) {
                return false;
            }
            let mut parent = node.parent();
            while let Some(p) = parent {
                if matches!(
                    p.kind(),
                    "function_declaration"
                        | "function_expression"
                        | "generator_function"
                        | "method_definition"
                        | "arrow_function"
                ) || is_class(p)
                {
                    return false;
                }
                parent = p.parent();
            }
            true
        })
        .collect())
}

#[cfg(test)]
mod callback_tests {
    use super::*;
    #[test]
    fn callback_collection_observes_remaining_node_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("callbacks.ts");
        std::fs::write(&path, "it('same', () => {});\n".repeat(1000)).unwrap();
        let parsed = crate::cache::OutlineCache::new()
            .get_or_parse(&path)
            .unwrap();
        let declarations = declarations(&path, &parsed, 8);
        assert_eq!(
            declarations
                .iter()
                .filter(|d| d.entry.kind == crate::types::OutlineKind::Function)
                .count(),
            8
        );
    }
}
