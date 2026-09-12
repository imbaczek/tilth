//! Rust language spec. Diverges on: stdlib prefix rule, lifetime ticks.

use crate::lang::spec::{LangSpec, StdlibRule, StripFamily, DEFAULT_DEFS, DEFAULT_DEF_KINDS};

const CALLEE_QUERY: &str = concat!(
    "(call_expression function: (identifier) @callee)\n",
    "(call_expression function: (field_expression field: (field_identifier) @callee))\n",
    "(call_expression function: (scoped_identifier name: (identifier) @callee))\n",
    "(macro_invocation macro: (identifier) @callee)\n",
    // A macro's arguments are a `token_tree` of raw tokens, so a call written
    // inside `write!(..)` has no `call_expression` node. Requiring the
    // following tree to open with `(` keeps `x[i]` and `Foo { .. }` out; the
    // rest is `callee_filter`'s job.
    "(token_tree (identifier) @callee . (token_tree . \"(\"))\n",
);

const SIBLING_QUERY: &str = concat!(
    "(field_expression value: (self) field: (field_identifier) @ref)\n",
    "(call_expression function: (field_expression value: (self) field: (field_identifier) @ref))\n",
);

/// Drop token-tree captures that the loose half of the query picked up. The
/// `.` between the identifier and the tree skips anonymous nodes, so the
/// pattern also matches the `x` in `assert_eq!(x, (a, b))`; requiring the tree
/// to be the *immediate* next sibling rejects it. Captures from the other four
/// patterns hang off `call_expression` / `field_expression` /
/// `scoped_identifier` / `macro_invocation`, never a token tree, so they pass.
///
/// Raw tokens are all this can go on, so it neither over- nor under-matches
/// perfectly: `matches!(x, Foo(_))` and `#[cfg(not(unix))]` still look like
/// calls, and a turbofish (`m!(parse::<u32>(s))`) still looks like none.
fn callee_is_call(node: &tree_sitter::Node) -> bool {
    if node.parent().is_some_and(|p| p.kind() == "token_tree") {
        node.next_sibling()
            .is_some_and(|n| n.kind() == "token_tree")
    } else {
        true
    }
}

pub(crate) const SPEC: LangSpec = LangSpec {
    display: "Rust",
    extensions: &["rs"],
    filenames: &[],
    grammar: Some(tree_sitter_rust::LANGUAGE),
    callee_query: Some(CALLEE_QUERY),
    sibling_query: Some(SIBLING_QUERY),
    stdlib: StdlibRule::Prefixes(&["std::", "core::", "alloc::"]),
    manifests: &["Cargo.toml"],
    definition_kinds: DEFAULT_DEF_KINDS,
    has_lifetimes: true,
    strip_family: Some(StripFamily::Rust),
    extract_receiver: None,
    callee_filter: Some(callee_is_call),
    definitions: DEFAULT_DEFS,
};
