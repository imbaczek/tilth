---
name: tilth
description: Use the `tilth` CLI for code reading, outlining, search, callers, blast-radius deps, and structural diffs. Activate when the user asks to explore a repo, find a symbol, trace callers, read a file, view a diff, or analyze impact. Prefer `tilth` over `grep`/`cat`/`find`/`ls` — one invocation returns AST-aware outlines, definitions, callees, and usages.
---

# tilth — code intelligence CLI

Tree-sitter + ripgrep + smart file reading in one binary. Replaces `grep`, `cat`, `find`, `ls` with AST-aware equivalents across 14 languages (Rust, TS/TSX, JS, Python, Go, Java, Scala, C, C++, Ruby, PHP, C#, Swift, Elixir).

Run via Bash: `tilth <args>`. Search before reading — `tilth <symbol> --scope .` returns definitions, usages, and callee footers in one call.

DO NOT use `grep`, `rg`, `cat`, `head`, `tail`, `find`, `ls` — use `tilth` instead.
DO NOT re-read files whose content is already shown in expanded search results.

## Read

```bash
tilth read <path>                 # smart view: full if small, outline if large
tilth read <path> --section 45-89      # exact line range
tilth read <path> --section "## Foo"   # markdown heading (suggests fuzzy matches on miss)
tilth read <path> --full               # force full content (file paths)
```

Outline format: `[<start>-<end>]  <symbol>`. Full/section format: `<line> │ <content>`. Binary files print `[skipped]`; lockfiles, minified bundles, generated code print `[generated]`.

`tilth read` always treats its arguments as file paths, including extensionless files.
Relative paths use the current directory, or `--root <dir>`. Legacy `tilth <path>`
reads remain supported when the query resolves to a file.

```bash
tilth read src/main.rs --mode signature # declarations with hash anchors
tilth read src/main.rs --mode stripped  # remove plain comments and debug logs
tilth read src/main.rs --section 1-20 --section 80-100
tilth read src/main.rs src/lib.rs --budget 2000 # shared batch budget
tilth read src/main.rs --edit            # hash anchors for edits
```

Views are `auto` (default), `full`, `signature`, and `stripped`. `--full` is an alias
for full content; signature/stripped take precedence. Sections require one file
and auto/full mode. Batches and disjoint sections are limited to 20 entries.

## Search

With multiple `--scope` arguments, a relative query that resolves to a file in any scope reads the matching file(s); it does not search the other scopes for that query as text. If no scope contains the file, normal search fallback applies.

```bash
tilth <symbol> --scope <dir>                # definitions + usages
tilth <symbol> --scope src --scope tests    # search multiple scopes
tilth "Foo,Bar,Baz" --scope <dir>           # multi-symbol (max 5)
tilth <symbol> --expand                     # inline source for top 2 matches
tilth <symbol> --expand=5                   # inline source for top 5
tilth <symbol> --full                       # up to 100 matches, source for the top 50
tilth <symbol> --full --expand=0            # up to 100 matches, no inline source
tilth <symbol> --callers --scope <dir>      # call sites (structural, not text)
tilth "TODO: fix" --scope <dir>             # content search (literal text)
tilth "/regex/" --scope <dir>               # regex search
tilth <symbol> --glob "*.rs" --scope <dir>  # file pattern filter
```

`--full` semantics depend on query type:
- File path → return whole file (bypass smart-view outline).
- Symbol / text / regex → raise the match cap from 10 to 100 and inline source for the top 50 matches. `--expand=N` sets the inline-source count only, so `--full --expand=0` still lists up to 100 matches. Multi-symbol (`"Foo,Bar"`) is the exception: it always expands at least one match per symbol.
- Glob → no-op.

Symbol search also surfaces **markdown headings as soft definitions** — `tilth StreamingResponse --scope docs/` finds `## StreamingResponse` headings ranked between code defs (60-80) and usages (0). Section body inlines automatically in the default preview (capped at 40 lines; pass `--expand` for the rest).

Output per match:
```
## <path>:<start>-<end> [definition|usage|impl]
<outline context>
<expanded source block>
── calls ──
  <callee>  <path>:<start>-<end>  <signature>
── siblings ──
  <related>  <path>:<start>-<end>  <signature>
```

`--callers` finds direct, by-name call sites. If it returns 0 matches but the symbol exists, the call is likely indirect (trait/interface dispatch, reflection, route registration, callback) — fall back to `tilth <symbol> --scope .` to see references.

## Files

```bash
tilth "*.test.ts" --scope <dir>   # glob files (.tilthignore honored)
tilth --no-respect-gitignore <symbol> --scope <dir>  # bypass .gitignore/.ignore/Git excludes
tilth --map --scope <dir>         # codebase skeleton with directory token rollups
```

Searches and file walks honor `.gitignore`, `.ignore`, and Git exclude rules by default.
Use `--no-respect-gitignore` or `TILTH_RESPECT_GITIGNORE=0` to disable them;
`--respect-gitignore` explicitly enables them and overrides the environment.
In MCP, pass `gitignore: false` for one call or use
`tilth_config(action: "set", respect_gitignore: false)` for the server process.
`tilth_config(action: "reset")` restores environment/default behavior.
`.tilthignore` and built-in junk-directory exclusions always remain active.

## Deps (blast radius)

```bash
tilth <file> --deps               # what it imports + what depends on it
```

Use only before renaming, removing, or changing an export's signature.

## Diff (structural)

```bash
tilth diff                        # uncommitted changes
tilth diff HEAD~1                 # vs prior commit
tilth diff main..feat             # branch comparison
tilth diff --log HEAD~5..HEAD     # per-commit symbol summaries
tilth diff --blast                # warn on signature-changed exports
tilth diff --expand 3             # inline source for top 3 changed symbols
```

Function-level change detection — `[+]` added, `[-]` removed, `[~]` modified, `[~:sig]` signature changed. Replaces `git diff` for symbol-level review.

## Budget

```bash
tilth <args> --budget 2000        # cap response at ~N tokens
```

Use when an outline or search returns more than you need.
