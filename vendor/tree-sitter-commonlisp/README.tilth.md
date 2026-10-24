# Vendored Common Lisp grammar

Source: https://github.com/tree-sitter-grammars/tree-sitter-commonlisp
Base commit: 32323509b3d9fe96607d151c2da2c9009eb13a2f (version 0.4.1).
Patch commit: a0d90ea6af430eb8ac2c28a83c1c9be50ecd5859.
Upstream draft PR: https://github.com/tree-sitter-grammars/tree-sitter-commonlisp/pull/56
License: MIT; see LICENSE.md.

Local patch: parse lambda lists as data, including nested lists whose first
symbol is `lambda` or `loop`. The stock grammar can misparse these legal
parameter names and lose subsequent definitions in a file. Allow generic list parsing to remain available when a form beginning with
`lambda` or `loop` is data rather than valid special-form syntax. AST node
names and lambda-list field names remain compatible. Regression corpus is included.

Generated with tree-sitter CLI 0.26.3. To regenerate, apply grammar.js to an
upstream checkout, install its tree-sitter-clojure grammar dependency, and run
`tree-sitter generate` followed by `tree-sitter test`. Copy the generated src/
files and Rust bindings back here. Building tilth needs only a C compiler,
not Node or the tree-sitter CLI. Tilth compiles parser.c directly from its root
build.rs, so cargo packaging includes the patch rather than resolving an
unpatched registry grammar.

The upstream Cargo manifest is retained as Cargo.toml.upstream, because a
nested Cargo.toml would exclude this directory from tilth's published crate.
