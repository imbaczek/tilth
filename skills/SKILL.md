---
name: tilth
description: Use the `tilth` CLI for code reading, outlining, search, callers, blast-radius deps, and structural diffs. Activate when the user asks to explore a repo, find a symbol, trace callers, read a file, view a diff, or analyze impact. Prefer `tilth` over `grep`/`cat`/`find`/`ls` — one invocation returns AST-aware outlines, definitions, callees, and usages.
---

# tilth — code intelligence CLI

Tree-sitter + ripgrep + smart file reading in one binary. Replaces `grep`, `cat`, `find`, `ls` with AST-aware equivalents across 18 languages (Rust, TS/TSX, JS, Python, Go, Java, Scala, C, C++, Ruby, PHP, C#, Swift, Kotlin, Elixir, Bash, Common Lisp).

Run via Bash: `tilth <args>`. Search before reading — `tilth <symbol> --scope .` returns definitions, usages, and callee footers in one call.

DO NOT use `grep`, `rg`, `cat`, `head`, `tail`, `find`, `ls` — use `tilth` instead.
DO NOT re-read files whose content is already shown in expanded search results.

## Read

```bash
tilth read <path>                     # smart view: full if small, outline if large
tilth read <path> --section 45-89      # exact line range
tilth read <path> --section "Foo"      # exact markdown title, any heading level
tilth read <path> --section "## Foo"   # constrain the title to an ATX heading level
tilth read <path> --section toc:1.2    # select an address from the outline or hints
tilth read <path> --full               # request full content (subject to skip/size guards) (file paths)
```

Code outline format: `[<start>-<end>]  <symbol>`. Markdown outlines align `Lines`, `TOC`, and `Heading` columns: `[<start>-<end>]  1.2  ## Foo`. The TOC column omits the repeated `toc:` prefix; select its entry with `--section toc:1.2` (syntax shown once above the table). Sections and source-line pages preserve original line numbers; unpaged full reads return raw content. Edit mode uses hash anchors. Binary files print `[skipped]`; lockfiles and generated code print `[generated]`; minified bundles return a `[minified]` summary.

Markdown section titles match exactly, with an optional ATX level prefix (`#` through `######`). Ambiguous titles report parent paths, line ranges, and selectable `toc:` addresses; missing titles suggest nearby headings with addresses. `toc:1.2` selects the second child of the first root ATX section. Indices start at 1; skipped heading levels add no phantom entries, and container headings follow section containment. Addresses follow document order and may change when headings move. Valid line ranges such as `123-456` take precedence; use an ATX prefix to select a title that looks like a range or starts with `toc:`. Setext headings are unsupported.

Directory reads start with a README preview (up to eight lines / 4 KiB; README.md, README, README.txt, then other README extensions). Each immediate subdirectory on the selected page gets its own README preview and overview counts of files, source files, and subdirectories, walking at most three levels like the default overview. Counts honor ignore rules, exclude symlinks, and report capped walks as partial. Root counts cover immediate entries. Offset/limit page immediate children by name; previews and summaries do not consume page slots. Output remains subject to the shared token budget.

`tilth read` always treats its arguments as paths, including extensionless files and directories.
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

### Exact symbol reads and merged file slices

```bash
tilth read src/main.rs --symbol main --symbol SearchOptions::validate
tilth read src/main.rs src/lib.rs --symbol 'parse@42' --mode signature
tilth read --slice src/main.rs:1-20 --slice src/main.rs:15-30 --slice src/lib.rs:40-60 --budget 2000
```

Repeat `--symbol` for 1–20 exact names, qualified names, or `name@line` selectors.
Every selector searches all supplied files; files and selectors are not paired by
position. Results preserve selector order, report missing or ambiguous names, and
emit each declaration body once. Use qualified candidates to disambiguate names
or declarations sharing a line. Ordinary reads use exact declaration spans;
signature mode shares the complete header extraction used by search projections.
Symbol reads accept auto/full/signature and a shared budget, but exclude sections,
slices, paging, and stripped mode. Edit mode requires auto/full and emits complete
physical lines with hash anchors. Unsupported, unreadable, oversized (over 1 MB),
or partially parsed files report indexing limitations.

Repeat `--slice path:start-end` for up to 20 numeric ranges total. Files retain
first-seen order; repeated paths and aliases of the same physical file are grouped.
Ranges are sorted and overlapping or adjacent ranges merge within each file.
The example reads main.rs lines 1–30 once, then lib.rs lines 40–60. Repeated
`--section` instead preserves supplied range order and overlaps.
Slices support auto/full and edit hash anchors, preserve original line numbers,
and share one output budget. They exclude positional paths, symbols, sections,
paging, and signature/stripped modes. All ranges are validated before reads;
file errors appear alongside successful results.

MCP `tilth_read` uses `symbols` with either `path` or `paths`, or uses `slices`
exclusively with those other path/selection fields:

```json
{"root":"/project","paths":["src/main.rs","src/lib.rs"],"symbols":["main","parse@42"],"mode":"signature","budget":2000}
{"root":"/project","slices":[{"path":"src/main.rs","sections":["1-20","15-30"]},{"path":"src/lib.rs","sections":["40-60"]}],"budget":2000}
```

## Search

Search scopes accept directories, files, and quoted globs of files or directories. Repeat `--scope` to combine selections. Globs honor ignore rules; expansion stops with an error beyond 100,000 visited entries or 1,000 matching paths across the request. Narrow the scope when a limit is reached. Missing selections report errors and up to five similar filenames (normalized Levenshtein similarity ≥0.8). If none qualify, lower-confidence suggestions require similarity ≥0.4, the same extension, and a shared whole stem component of at least two characters (e.g. `wal-checkpoint.lisp` → `wal.lisp`). Otherwise nearby directory hints are offered. Hints fit within the response budget; suggestions do not select a replacement or widen the search. Valid scopes can still return results alongside failed scopes.

For symbol/text/regex/callers results, an additional `--glob` filters each resolved scope relative to its directory, or a selected file's parent. File scopes honor ignore rules too. MCP `tilth_search` accepts the same selections in `scope` or `scopes`, with an absolute `root` required for relative scopes. `tilth_list`, deps, grok, map, and MCP startup require directory scopes; diff accepts file or directory scopes.

A complete zero-hit symbol or literal search may show a separately warned list of approximate content and filename suggestions. Filename matching uses a bounded, ignore-aware metadata walk without opening suggested files, independently of the content limits. A case-normalized exact name or stem suppresses approximate suggestions. Do not treat these suggestions as definitions or automatically read, follow, grok, trace, or expand them. Content similarity requires at most 500 distinct files and 20,000,000 bytes, inclusive, and observes the 1 MB per-file cap. Regex searches do not use this fallback; projections keep exact counts. Similarity defaults to 0.80 normalized OSA. Set CLI `--similarity-threshold`, `--similarity-max-files`, or `--similarity-max-bytes`, or MCP `similarity_threshold`, `similarity_max_files`, or `similarity_max_bytes`, per search; file/byte limits apply to content similarity. Offset, limit, and budget apply to suggestions. Incomplete walks or work-limit exhaustion are reported.

With multiple `--scope` arguments, a relative query that resolves to a file under a directory scope reads the matching file(s); it does not search the other scopes for that query as text. If no scope contains the file, normal search fallback applies.

```bash
tilth <symbol> --scope <dir>                # definitions + usages
tilth <symbol> --scope src --scope tests    # search multiple scopes
tilth "retention,slot-wal" --scope src/storage/wal.lisp # selected file
tilth <symbol> --scope 'src/**/*.rs'         # quoted scope glob
tilth "Foo,Bar,Baz" --scope <dir>           # multi-symbol (max 5)
tilth <symbol> --expand                     # inline source for top 2 matches
tilth <symbol> --expand=5                   # inline source for top 5
tilth <symbol> --full                       # up to 100 matches, source for the top 50
tilth <symbol> --full --expand=0            # up to 100 matches, no inline source
tilth <symbol> --offset 100 --limit 25      # ranked matches beyond preview
tilth <symbol> --callers --scope <dir>      # call sites (structural, not text)
tilth "TODO: fix" --scope <dir>             # content search (literal text)
tilth "/regex/" --scope <dir>               # regex search
tilth <symbol> --glob "*.rs" --scope <dir>  # file pattern filter
```

`--full` semantics depend on query type:
- File path → return whole file (bypass smart-view outline).
- Symbol / text / regex / callers → raise the default preview/page size from 10 to 100; an explicit `--limit` overrides it. CLI `--full` also expands up to 50 matches unless `--expand=N` is supplied. CLI multi-symbol queries retain an expansion floor even with `--expand=0`; MCP honors `expand: 0`.
- Glob → no effect on the unpaged 20-file preview; explicit offset/limit pages files (default 50).
- Deps → collect/show all dependencies; explicit paging also collects all records, even without `--full`.
- Grok → widen overview caps; selected-section pages always collect the whole section.

Symbol search also surfaces **markdown headings as soft definitions** — `tilth StreamingResponse --scope docs/` finds `## StreamingResponse` headings ranked between code defs (60-80) and usages (0). Section body inlines automatically in the default preview (capped at 40 lines; pass `--expand` for the rest).

Output per match:
```
## <path>:<start>-<end> [definition|usage|impl]
<outline context>
<expanded source block>
-- calls --
  <callee>  <path>:<start>-<end>  <signature>
-- siblings --
  <related>  <path>:<start>-<end>  <signature>
```

`--callers` finds direct, by-name call sites. If it returns 0 matches but the symbol exists, the call is likely indirect (trait/interface dispatch, reflection, route registration, callback) — fall back to `tilth <symbol> --scope .` to see references.

### Compact projections and literal query batches

```bash
tilth search --query 'alpha,beta' --query TODO --kind content --select files --scope src
tilth search --query read --kind symbol --select signatures --scope src --limit 10
tilth search --query TODO --kind content --select count --scope src
```

Repeat `--query` for 1–20 nonempty literal queries; commas are preserved and
duplicate queries run once. Kind defaults to symbol; content and regex also work.
MCP `tilth_search` accepts `queries` (exclusive with `query`), `kind`, and `select`:

```json
{"root":"/project","queries":["alpha,beta","TODO"],"kind":"content","select":"files","scope":"src","budget":2000}
```

`files` returns unique sorted paths. `definitions` and `signatures` require symbol
search and retain only definitions. Signatures preserve complete multiline headers,
including parameters and return types, without bodies; unavailable or ambiguous
headers and parser recovery are reported. `count` returns total matches per query,
independent of page size, with lower bounds when work was skipped. Projection and
filtering precede offset/limit; pages are per query and the output budget is shared.
Literal batches return compact matches even without select. Positive `--expand`
is incompatible with literal batches and projections.

## Trace

```bash
tilth trace handle_request --to execute_query --depth 5 --scope src
tilth trace execute_query --direction callers --depth 3 --scope src --budget 2000
```

Returns a shortest discovered static call path, or a bounded neighborhood without
`--to`. Depth is 1–16 (default 2); direction defaults to callees. Endpoints accept
names, qualified names, `name@line`, or scope-relative `file:line`; ambiguity
reports candidates. MCP `tilth_trace` uses `from`, optional `to`, `direction`,
`depth`, directory `scope`, `root`, and `budget`.

Calls are resolved by name with explicit heuristic and completeness notices.
Missing paths do not prove unreachability. Dispatch, aliases, macros, callbacks,
parse failures, and unsupported syntax can leave gaps. Work caps are independent
of output budget: 100,000 directory entries, 2,000 source files, 64 MiB, 30,000
functions, 50,000 call sites, and 1 MB per parsed file. Narrow scope when a cap
prevents useful results. Hard caps and path-local concerns remain visible even
when a clean local path suppresses warnings about unrelated files.

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

Directory reads default to 50 immediate children, sorted by name, even with `--full`.
CLI globs and maps accept `--offset`/`--limit` per scope. MCP `tilth_list` pages matching files by relative path; directory nodes do not count. List/map trees keep directory context and report rollups for only the selected files. Maps are CLI-only.

```bash
tilth read artifacts/ --offset 20 --limit 20
tilth "*.rs" --scope src --offset 50 --limit 50
tilth --map --scope src --offset 50 --limit 50
```

## Deps (blast radius)

```bash
tilth <file> --deps               # what it imports + what depends on it
tilth <file> --deps --offset 20 --limit 20 # complete dependency records
```

Use for behavior/signature changes or removing/renaming exports. Explicit paging collects all exported symbols and callers first, then pages local dependencies, external dependencies, production dependents, and test dependents, sorted by path/name within each section. One dependent file with all its caller details is one record. MCP `tilth_deps` uses the same `offset`/`limit`.

## Diff (structural)

```bash
tilth diff                        # uncommitted changes
tilth diff HEAD~1                 # vs prior commit
tilth diff main..feat             # branch comparison
tilth diff --log HEAD~5..HEAD     # per-commit symbol summaries
tilth diff --blast                # warn on signature-changed exports
tilth diff --offset 20 --limit 20 # changed files
tilth diff --scope src/lib.rs --offset 10 --limit 10 # symbol/hunk blocks
tilth diff --log HEAD~20..HEAD --offset 5 --limit 5 # commits
```

Function-level change detection — `[+]` added, `[-]` removed, `[~]` modified, `[~:sig]` signature changed. Replaces `git diff` for symbol-level review. Filters apply before paging. Overview/directory scopes page files by path; file scopes page source-ordered symbol/unattributed-hunk blocks; `file:function` selects one symbol; logs page commits in Git order. MCP `tilth_diff` accepts `offset`/`limit`. Keep the source ref, scope, search filter, and working tree unchanged.

## Grok

```bash
tilth grok Type::method                              # overview: signature/doc/body + relationship previews
tilth grok Type::method --section callers --offset 50 --limit 25
tilth grok Type::method --section callees --limit 25
```

Select `callers`, `callees`, `siblings`, or `tests` to fetch only that list plus compact target context. Offset/limit require a section. Records are individual call sites, callees, declarations, or test references; internal and external callees share one page sequence, and siblings follow source order. Section calls neither expand target/delegate bodies nor consume body-dedup state. Use the file reader for source bodies. Preserve the original qualified selector across continuation calls, even when declarations share a source line. MCP `tilth_grok` uses `target`, `section`, `offset`, `limit`, and optional `budget`.

## Pagination and budget

Offsets are zero-based nonnegative integers; limits are positive integers. Supplying either opts into paging; directory reads, MCP `tilth_list`, and callers already return default pages. Without explicit paging, other views retain their normal smart view/preview.

| View | Page unit | Default size |
|------|-----------|--------------|
| Symbol/content/regex | Ranked match per target, after merging scopes | 10; 100 with full |
| Callers | Call site per target and resolved scope (CLI and MCP) | 10; 100 with full |
| Directory / CLI glob / MCP list / CLI map | Child / matching file / matching file / file | 50 |
| Code auto or code signature | Source-ordered declaration | 50 |
| Markdown auto | Heading, with global TOC address | 50 |
| Full / other text / selected section(s) | Original source line / line in selected ranges | 50 |
| Code stripped | Retained line with original source number | 50 |
| Deps | Dependency or dependent-file record | 50 |
| Diff | File / symbol-hunk block / selected symbol / commit, by view | 50 |
| Grok section | Record in the selected relationship list | 50 |

Explicit code/Markdown auto paging forces an uncapped declaration/heading outline even for small files. `--full` pages source lines instead. Code with no recognized declarations and other non-Markdown text fall back to line pages. Non-code signature/stripped requests use the normal read fallback; Markdown keeps headings unless full selects physical lines. Binary/generated/minified summaries stay skipped/summarized. Paging never renumbers source/hash/TOC addresses.

```bash
tilth read src/main.rs --mode signature --offset 25 --limit 25
tilth read src/main.rs --full --offset 100 --limit 25
tilth read src/main.rs --mode stripped --offset 100 --limit 25
tilth read src/main.rs --section 40-80 --section 120-160 --offset 20 --limit 20
tilth read src/main.rs src/lib.rs --offset 25 --limit 25 --budget 4000
```

MCP `tilth_read` supports the same paging fields with either `path` or batch `paths`. Batches and CLI multi-path reads apply the same page independently to each file and share the final budget. Section selection still requires one file and auto/full mode; multiple ranges keep their supplied order and overlaps, with offsets into the concatenated selected lines.

Symbol/content/regex searches combine and deduplicate scopes before slicing; comma-separated targets each get their own page. CLI and MCP callers page each resolved scope independently. CLI glob/map pages are also per scope. Keep query/target, root, scopes, filters, modes, source, and ignore settings unchanged between pages.

`--budget` (MCP: `budget`) controls output size independently of entry count. For unpaged outlines over 500 KB, an explicit budget replaces the default 100-entry cap; explicit pages already bypass that cap. An explicit limit overrides search's 10/100 default and other views' 50-entry default. If the page or its navigation is omitted, retry the same offset with a smaller limit or larger budget before advancing. Failed-scope ranked searches report lower bounds and withhold continuation. Offsets beyond the total yield an empty terminal page.

MCP next-page JSON is an argument patch to merge into the original request; retain required path/query/target and scope/root fields. CLI hints are flags to apply to the original command. Example calls:

```text
tilth_search({query: "TODO: fix", kind: "content", root: "/project", offset: 25, limit: 25})
tilth_search({query: "needle", root: "/project", scope: "src/main.rs"})
tilth_search({query: "needle", root: "/project", scope: "src/**/*.rs"})
tilth_read({root: "/project", paths: ["src/main.rs", "src/lib.rs"], mode: "signature", offset: 25, limit: 25})
tilth_list({patterns: ["*.rs"], scope: "/project/src", offset: 50, limit: 50})
tilth_deps({path: "src/lib.rs", root: "/project", offset: 20, limit: 20})
tilth_diff({source: "HEAD~1", root: "/project", scope: "src/lib.rs", offset: 10, limit: 10})
tilth_grok({target: "Type::method", root: "/project", section: "callers", offset: 50, limit: 25})
```
