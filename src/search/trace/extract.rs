//! Call-site extraction. Captures retain byte positions for exact ownership.
use crate::{cache::ParsedFile, types::Lang};
use streaming_iterator::StreamingIterator;

pub(super) struct Call {
    pub name: String,
    pub receiver: Option<String>,
    pub byte: usize,
    pub line: u32,
    pub concerns: Vec<&'static str>,
}

pub(super) fn calls(parsed: &ParsedFile) -> (Vec<Call>, Vec<String>) {
    let bindings = if parsed.lang == Lang::Rust {
        RustBindings::new(parsed)
    } else {
        RustBindings::default()
    };
    if parsed.lang == Lang::CommonLisp {
        return super::lisp::calls(parsed);
    }
    let Some(query_str) = crate::search::callee_query::callee_query_str(parsed.lang) else {
        return (
            Vec::new(),
            vec!["call extraction unsupported for this language".into()],
        );
    };
    let Some(language) = crate::lang::outline::outline_language(parsed.lang) else {
        return (Vec::new(), vec!["call grammar unavailable".into()]);
    };
    crate::search::callee_query::with_callee_query(&language, query_str, |query| {
        let Some(index) = query.capture_index_for_name("callee") else {
            return (Vec::new(), vec!["callee capture unavailable".into()]);
        };
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(query, parsed.tree.root_node(), parsed.content.as_bytes());
        let mut calls = Vec::new();
        let mut problems = Vec::new();
        while let Some(found) = matches.next() {
            for capture in found.captures().iter().filter(|c| c.index == index) {
                let node = capture.node;
                if crate::lang::spec::spec(parsed.lang)
                    .callee_filter
                    .is_some_and(|filter| !filter(&node))
                {
                    continue;
                }
                let mut name = node
                    .utf8_text(parsed.content.as_bytes())
                    .unwrap_or("")
                    .to_owned();
                let mut concerns = Vec::new();
                let mut receiver = None;
                if let Some(parent) = node.parent() {
                    if parsed.lang == Lang::Rust
                        && parent.kind() == "field_expression"
                        && parent
                            .child_by_field_name("value")
                            .is_some_and(|value| value.kind() == "self")
                    {
                        name = format!("Self::{name}");
                    }
                    match parent.kind() {
                        "scoped_identifier" => {
                            if let Ok(text) = parent.utf8_text(parsed.content.as_bytes()) {
                                text.clone_into(&mut name);
                            }
                        }
                        "field_expression"
                        | "member_expression"
                        | "attribute"
                        | "selector_expression"
                        | "navigation_expression" => {
                            receiver = parent
                                .child_by_field_name("object")
                                .and_then(|n| n.utf8_text(parsed.content.as_bytes()).ok())
                                .map(str::to_owned);
                            concerns.push("receiver dispatch is resolved by name only");
                        }
                        "macro_invocation" => concerns
                            .push("Rust macro invocation cannot resolve to a function declaration"),
                        "token_tree" => concerns.push("macro expansion may add or change calls"),
                        _ => {}
                    }
                }
                if parsed.lang == Lang::Rust
                    && node.kind() == "identifier"
                    && bindings.contains(&name, node.start_byte())
                {
                    concerns.push("lexically bound Rust callable is not resolved");
                }
                if parsed.lang != Lang::Rust {
                    concerns.push("language bindings are resolved heuristically by name");
                }
                calls.push(Call {
                    name,
                    receiver,
                    byte: node.start_byte(),
                    line: node.start_position().row as u32 + 1,
                    concerns,
                });
                if calls.len() >= super::MAX_EDGES {
                    problems.push("call-site work limit reached".into());
                    break;
                }
            }
            if calls.len() >= super::MAX_EDGES {
                break;
            }
        }
        drop(matches);
        if parsed.lang == Lang::Rust {
            rust_uncaptured(parsed, &mut calls, &mut problems, &bindings);
        } else if uncovered_call_syntax(parsed, &calls) {
            problems.push("call-like syntax is not covered by the language query".into());
        }
        if cursor.did_exceed_match_limit() {
            problems.push("tree-sitter query match limit reached".into());
        }
        (calls, problems)
    })
    .unwrap_or_else(|| (Vec::new(), vec!["call query compilation failed".into()]))
}

#[derive(Default)]
struct RustBindings(std::collections::HashMap<String, Vec<(usize, usize)>>);
impl RustBindings {
    fn new(parsed: &ParsedFile) -> Self {
        fn names(node: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
            crate::lang::walk::named_descendants(node)
                .filter(|node| matches!(node.kind(), "identifier" | "shorthand_field_identifier"))
                .map(|node| node.utf8_text(source.as_bytes()).unwrap_or("").to_owned())
                .collect()
        }
        let mut bindings = Self::default();
        for node in crate::lang::walk::named_descendants(parsed.tree.root_node()) {
            let mut add = |pattern: tree_sitter::Node<'_>, start, end| {
                for name in names(pattern, &parsed.content) {
                    bindings.0.entry(name).or_default().push((start, end));
                }
            };
            match node.kind() {
                "parameter" => {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        let mut owner = node.parent();
                        while let Some(parent) = owner {
                            if matches!(parent.kind(), "function_item" | "closure_expression") {
                                add(pattern, parent.start_byte(), parent.end_byte());
                                break;
                            }
                            owner = parent.parent();
                        }
                    }
                }
                "closure_parameters" => {
                    if let Some(owner) = node.parent() {
                        add(node, owner.start_byte(), owner.end_byte());
                    }
                }
                "let_declaration" => {
                    if let (Some(pattern), Some(block)) =
                        (node.child_by_field_name("pattern"), node.parent())
                    {
                        add(pattern, node.end_byte(), block.end_byte());
                    }
                }
                "function_item" => {
                    if let (Some(name), Some(block)) = (
                        node.child_by_field_name("name"),
                        node.parent().filter(|n| n.kind() == "block"),
                    ) {
                        add(name, block.start_byte(), block.end_byte());
                    }
                }
                "use_declaration" => {
                    if let Some(scope) = node.parent() {
                        add(node, scope.start_byte(), scope.end_byte());
                        if node
                            .utf8_text(parsed.content.as_bytes())
                            .is_ok_and(|text| text.contains('*'))
                        {
                            bindings
                                .0
                                .entry("*".into())
                                .or_default()
                                .push((scope.start_byte(), scope.end_byte()));
                        }
                    }
                }
                "for_expression" => {
                    if let (Some(pattern), Some(body)) = (
                        node.child_by_field_name("pattern"),
                        node.child_by_field_name("body"),
                    ) {
                        add(pattern, body.start_byte(), body.end_byte());
                    }
                }
                "match_arm" => {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        add(pattern, node.start_byte(), node.end_byte());
                    }
                }
                "let_condition" => {
                    if let Some(pattern) = node.child_by_field_name("pattern") {
                        let mut owner = node.parent();
                        while let Some(parent) = owner {
                            if matches!(parent.kind(), "if_expression" | "while_expression") {
                                let end = parent
                                    .child_by_field_name("consequence")
                                    .or_else(|| parent.child_by_field_name("body"))
                                    .map_or(parent.end_byte(), |n| n.end_byte());
                                add(pattern, node.end_byte(), end);
                                break;
                            }
                            owner = parent.parent();
                        }
                    }
                }
                _ => {}
            }
        }
        for ranges in bindings.0.values_mut() {
            ranges.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::new();
            for (start, end) in ranges.drain(..) {
                if let Some(last) = merged.last_mut().filter(|r| start <= r.1) {
                    last.1 = last.1.max(end);
                } else {
                    merged.push((start, end));
                }
            }
            *ranges = merged;
        }
        bindings
    }
    fn contains(&self, name: &str, byte: usize) -> bool {
        [name, "*"].iter().any(|name| {
            self.0.get(*name).is_some_and(|ranges| {
                let i = ranges.partition_point(|range| range.0 <= byte);
                i > 0 && byte < ranges[i - 1].1
            })
        })
    }
}

fn rust_uncaptured(
    parsed: &ParsedFile,
    calls: &mut Vec<Call>,
    problems: &mut Vec<String>,
    bindings: &RustBindings,
) {
    let captured = captured_call_positions(parsed, calls, |kind| {
        matches!(kind, "call_expression" | "macro_invocation")
    });
    for node in crate::lang::walk::named_descendants(parsed.tree.root_node()) {
        if is_cfg_attribute(node, &parsed.content) {
            problems.push("Rust conditional compilation is not evaluated".into());
        }
        if matches!(node.kind(), "call_expression" | "macro_invocation")
            && !captured.contains(&node.start_byte())
        {
            let function = node
                .child_by_field_name(if node.kind() == "macro_invocation" {
                    "macro"
                } else {
                    "function"
                })
                .unwrap_or(node);
            let function = if function.kind() == "generic_function" {
                function.child_by_field_name("function").unwrap_or(function)
            } else {
                function
            };
            let name = function
                .utf8_text(parsed.content.as_bytes())
                .unwrap_or("")
                .to_owned();
            let mut concerns = Vec::new();
            if node.kind() == "macro_invocation" {
                concerns.push("Rust macro invocation cannot resolve to a function declaration");
            } else if !matches!(function.kind(), "identifier" | "scoped_identifier") {
                concerns.push("unsupported call expression is not resolved");
            }
            if bindings.contains(&name, function.start_byte()) {
                concerns.push("lexically bound Rust callable is not resolved");
            }
            calls.push(Call {
                name,
                receiver: None,
                byte: function.start_byte(),
                line: function.start_position().row as u32 + 1,
                concerns,
            });
            if calls.len() >= super::MAX_EDGES {
                problems.push("call-site work limit reached".into());
                break;
            }
        }
    }
}

pub(super) fn has_rust_cfg(parsed: &ParsedFile) -> bool {
    if parsed.lang != Lang::Rust {
        return false;
    }
    for node in crate::lang::walk::named_descendants(parsed.tree.root_node()) {
        if is_cfg_attribute(node, &parsed.content) {
            return true;
        }
    }
    false
}

fn uncovered_call_syntax(parsed: &ParsedFile, calls: &[Call]) -> bool {
    let is_call = |kind: &str| {
        matches!(
            kind,
            "call_expression"
                | "invocation_expression"
                | "method_invocation"
                | "object_creation_expression"
                | "new_expression"
                | "function_call_expression"
                | "method_call"
                | "call"
                | "macro_invocation"
        )
    };
    let captured = captured_call_positions(parsed, calls, is_call);
    for node in crate::lang::walk::named_descendants(parsed.tree.root_node()) {
        if is_call(node.kind()) && !captured.contains(&node.start_byte()) {
            return true;
        }
    }
    false
}

fn is_cfg_attribute(node: tree_sitter::Node<'_>, source: &str) -> bool {
    matches!(node.kind(), "attribute_item" | "inner_attribute_item")
        && node.utf8_text(source.as_bytes()).is_ok_and(|text| {
            let path = text
                .trim_start_matches('#')
                .trim_start()
                .trim_start_matches('!')
                .trim_start_matches('[')
                .trim_start();
            matches!(
                path.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .next(),
                Some("cfg" | "cfg_attr")
            )
        })
}

fn captured_call_positions(
    parsed: &ParsedFile,
    calls: &[Call],
    is_call: impl Fn(&str) -> bool,
) -> std::collections::HashSet<usize> {
    let mut captured = std::collections::HashSet::new();
    for call in calls {
        let mut current = parsed
            .tree
            .root_node()
            .descendant_for_byte_range(call.byte, call.byte + 1);
        while let Some(node) = current {
            if is_call(node.kind()) {
                captured.insert(node.start_byte());
                break;
            }
            current = node.parent();
        }
    }
    captured
}

#[cfg(test)]
mod polish_tests {
    use super::*;
    #[test]
    fn binding_index_preserves_patterns_and_imports_under_source_order_walk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        let source="fn callback() {}\nfn pattern() { if let Some(callback)=None::<fn()> { callback(); } }\nfn imported() { use other::callback; callback(); }\n";
        std::fs::write(&path, source).unwrap();
        let cache = crate::cache::OutlineCache::new();
        let parsed = cache.get_or_parse(&path).unwrap();
        let mut old = Vec::new();
        let mut pending = vec![parsed.tree.root_node()];
        while let Some(node) = pending.pop() {
            old.push(node.id());
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        let new: Vec<_> = crate::lang::walk::named_descendants(parsed.tree.root_node())
            .map(|node| node.id())
            .collect();
        assert_eq!(
            old.iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            new.iter().copied().collect(),
            "AST walk changed coverage"
        );
        let bindings = RustBindings::new(&parsed);
        let byte = source.find("callback();").unwrap();
        assert!(bindings.contains("callback", byte), "{:?}", bindings.0);
        let byte = source.rfind("callback();").unwrap();
        assert!(bindings.contains("callback", byte), "{:?}", bindings.0);
    }
}
