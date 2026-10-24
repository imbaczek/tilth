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

Combined grammar snapshot: d440ce244f193feceec2de672bf0e2ead3982314
(local vendor/open-pr-fixes branch in ../tree-sitter-commonlisp).

## Additional pending upstream fixes

Reviewed on 2026-10-24. These are source-only ports; generated parser files
were rebuilt with tree-sitter CLI 0.26.3, rather than copied from the PRs.

- PR #49: https://github.com/tree-sitter-grammars/tree-sitter-commonlisp/pull/49
  at abc253c7ccf11569f218f5ef2d30c6154937328f. Fix repeated bars in block
  comments, including empty #||# and ACL2-style #|| ... ||#; include its five
  regression cases. Nested block comments remain an existing unsupported case.
- PR #53: https://github.com/tree-sitter-grammars/tree-sitter-commonlisp/pull/53
  at 332f58f4306ec1c1d0efbb9d0e468cf700e6e14e. Classify ALWAYS, NEVER and
  THEREIS as termination clauses and recognize conditional END. As elsewhere
  in this grammar, clause recognition is permissive rather than a validator.
- PR #55: https://github.com/tree-sitter-grammars/tree-sitter-commonlisp/pull/55
  at 32aee31b0caa95784769f8476d5309fd47b3fdf6. Recognize an initial LOOP
  NAMED clause. The local adaptation accepts uninterned (#:), qualified,
  escaped, keyword, and NIL symbol names as well as bare symbols. Only its
  grammar change and named-loop regression are included; build artifacts and
  unrelated build-system edits are excluded.

Additional regression cases cover quantified clauses, conditional END,
reader spellings of loop names, subsequent definitions, and lambda-list data.
The complete upstream basic corpus is retained with the adopted cases.

PRs #44 and #46 concern bindings/tooling not used by tilth. PRs #50 and #52
add editor highlighting/Swift changes, also outside tilth's current use.
PR #51 adds the license to Cargo packaging; tilth already packages the
vendored LICENSE.md and compiles the parser directly.

## Validation against SBCL

All 55 grammar corpus cases and 15 tag assertions pass. The SBCL audit covers
715 Lisp source files. Compared with the original vendored patch, these
changes remove exactly 10 false definitions inside repeated-bar block comments
in src/pcl/dfun.lisp and src/code/target-exception.lisp; all other definition
names and ranges are unchanged. The resulting index has 12,894 definitions.
The coarse column-zero DEFUN/DEFMACRO text count is 9,521/10,078 (94.5%);
its denominator includes comments and example code, so it is not semantic
reader coverage. Seven of the removed false definitions were counted by that
text audit.

Set TILTH_COMMONLISP_AUDIT_REPORT to a JSON output path when running the
ignored commonlisp_sbcl_corpus test to compare per-file definition inventories.
