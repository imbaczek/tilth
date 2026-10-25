# tilth

[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/jahala/tilth/badge)](https://scorecard.dev/viewer/?uri=github.com/jahala/tilth)

**Smart code reading for humans and AI agents.** tilth parses your code with tree-sitter and answers questions that text search can't. It shows where a symbol is defined, who calls it, what it calls, what depends on a file, and what changed at function level.

tilth is one binary. It works as a command-line tool and as an MCP server, which is how most agent hosts connect to tools. It builds no index and needs no setup.

```
$ tilth fastapi/dependencies/utils.py
# fastapi/dependencies/utils.py (1027 lines, ~9.6k tokens)

[1-]   imports: dataclasses, inspect, sys
[86-110]     fn ensure_multipart_is_installed
           def ensure_multipart_is_installed() -> None
[113-125]    fn get_parameterless_sub_dependant
           def get_parameterless_sub_dependant(*, depends: params.Depends, path: str) -> Dependant
[128-179]    fn get_flat_dependant
           def get_flat_dependant(
...
[203-215]    fn _get_signature
           def _get_signature(call: Callable[..., Any]) -> inspect.Signature
[218-232]    fn get_typed_signature
           def get_typed_signature(call: Callable[..., Any]) -> inspect.Signature
...
```

Small files come back whole. Large files come back as an outline with line ranges. Ask for the part you need with `--section`:

```bash
tilth fastapi/dependencies/utils.py --section 218-232
tilth docs/guide.md --section "Installation"
tilth docs/guide.md --section "toc:1.2"
```

Markdown outlines align line ranges, TOC addresses, and headings in columns:

```text
Lines  TOC  Heading
[3-8]  1.2  ## Installation
```

The `TOC` column omits the repeated `toc:` prefix; use `--section toc:1.2` to read that section. The selector syntax is shown once above the table.

Markdown titles match exactly with or without a `#` prefix; a prefix constrains the heading level. Ambiguous titles return parent paths, line ranges, and copyable `toc:` addresses. `toc:1.2` selects the second child of the first top-level ATX heading; indices start at 1 and skipped heading levels add no phantom entries. Addresses follow document order and may change when headings move. Line ranges such as `123-456` take precedence; use a `#` prefix for a title that looks like a line range or starts with `toc:`. Setext headings remain unsupported.

The examples in this README are real output from the [FastAPI](https://github.com/fastapi/fastapi) and [Gin](https://github.com/gin-gonic/gin) repositories. Lines marked `...` are cut for length.

Without paging, outlines for files over 500 KB show at most 100 entries by default. An explicit `--budget` replaces that entry cap with a token limit. Use `--offset` and `--limit` to page an uncapped code outline or a Markdown heading outline, including small files:

```bash
tilth docs/experiment_log.md --budget 20000
tilth read src/main.rs --offset 100 --limit 50
tilth read docs/experiment_log.md --offset 100 --limit 50
```

The page unit depends on the view: declarations for code outlines, headings for Markdown outlines, and source lines with `--full`. See [Pagination](#pagination) for all view units, defaults, batch behavior, and budget handling.

## Search finds definitions first

Use `tilth search <query>` for an explicit search command. It accepts the same
options as `tilth <query>`, including `--scope`, `--glob`, `--expand`, and `--json`.
Options can go before or after `search`. Repeated scopes combine; for options
with a single value, a value after `search` takes precedence.

```bash
tilth search get_typed_signature --scope fastapi --expand=1
```

For compact results, choose `--select files|definitions|signatures|count`.
Projection happens before pagination. Definitions and signatures require symbol
search; counts describe all observed matches, independently of the page size.
Incomplete scans report lower bounds and scope errors.

```bash
tilth parse_query --select signatures
tilth search --query parse_query --query parse_statement --select definitions
tilth search --query 'alpha,beta' --kind content --select count
```

Repeat `--query` (up to 20) to search literal values without splitting commas.
The positional comma-separated symbol shorthand remains available. A batch keeps
per-query results, deduplicates repeated queries, and shares one output budget.
These compact searches do not expand source (`--expand=0` is accepted).
MCP `tilth_search` accepts `queries: ["parse_query", "parse_statement"]` instead
of `query`, with the same `select` options. `--offset`/`--limit` page each query;
keep queries, selection, scopes and filters unchanged between pages.


````
$ tilth get_typed_signature --scope fastapi --expand=1
# Search: "get_typed_signature" in fastapi — 2 matches (1 definitions, 1 usages)

### dependencies/utils.py:218-232 [definition]
  [203-215]    fn _get_signature
             def _get_signature(call: Callable[..., Any]) -> inspect.Signature
-> [218-232]    fn get_typed_signature
             def get_typed_signature(call: Callable[..., Any]) -> inspect.Signature
  [235-241]    fn get_typed_annotation

```dependencies/utils.py:218-232
 218 | def get_typed_signature(call: Callable[..., Any]) -> inspect.Signature:
 219 |     signature = _get_signature(call)
 220 |     unwrapped = inspect.unwrap(call)
 ...
 232 |     return typed_signature
```

-- calls --
  _get_signature  dependencies/utils.py:203-215  def _get_signature(call: Callable[..., Any]) -> inspect.Signature
  get_typed_annotation  dependencies/utils.py:235-241  def get_typed_annotation(annotation: Any, globalns: dict[str, Any]) -> Any

### dependencies/utils.py:277 [usage in function get_dependant]
  [244-253]    fn get_typed_return_annotation
             def get_typed_return_annotation(call: Callable[..., Any]) -> Any
-> [256-328]    fn get_dependant
             def get_dependant(
  [331-352]    fn add_non_field_param_to_dependency

(~439 tokens)
````

tilth uses the syntax tree to tell a definition from a mention, and it lists definitions first. Each match is shown with the symbols around it, so you can see where you are without reading the file.

An expanded definition ends with a `-- calls --` footer. It lists the functions the definition calls, each with its file, line range and signature. An agent can follow a call chain from there without searching again.

Without explicit paging, a search shows a preview of up to 10 matches, or 100 with `--full`. Use `--offset` and `--limit` to retrieve ranked symbol, content, or regex matches beyond that preview; an explicit limit overrides the 10/100 default:

```bash
tilth search get_typed_signature --scope fastapi --offset 20 --limit 20
tilth "TODO: fix" --offset 0 --limit 25
tilth "/get_.*signature/" --offset 25 --limit 25
```

In MCP, `tilth_search` accepts the same paging fields for every `kind` (`symbol`, `content`, `regex`, and `callers`):

```json
{"query":"TODO: fix","kind":"content","root":"/project","offset":0,"limit":25}
```

Search scopes accept directories, individual files, and quoted globs:

```bash
tilth 'retention,slot-wal' --scope src/storage/wal.lisp
tilth needle --scope 'src/**/*.rs'
```

Globs select matching files and directories, honor ignore rules, and never widen an unmatched selection to the whole project. Expansion is limited to 100,000 visited entries and 1,000 matching paths; use a narrower scope if a limit is reached. File scopes honor ignore rules too; `--no-respect-gitignore` bypasses Git ignores. For symbol/text/regex/callers results, an additional `--glob` narrows each resolved scope (relative to its directory, or a selected file's parent). Missing scopes suggest up to five similar filenames with normalized Levenshtein similarity of at least 0.8. If none qualify, lower-confidence suggestions use a floor of 0.4 and require the same extension and a shared whole filename-stem component of at least two characters (for example, `wal-checkpoint.lisp` → `wal.lisp`). Directory hints remain the fallback when no filenames qualify. All hints fit within the response budget. Suggestions do not automatically select a replacement.

Symbol/content/regex searches combine and deduplicate all requested scopes before selecting a page. Comma-separated symbol targets each get their own page. Caller searches page each resolved scope independently in both CLI and MCP.

### Expanded search

On the command line, search returns compact results. `--expand` adds the source of the top matches:

```bash
tilth ServeHTTP --scope . --expand       # top 2
tilth ServeHTTP --scope . --expand=5     # top 5
```

In MCP mode, `expand` defaults to 2.

### Multi-symbol search

Look up several symbols in one call:

```bash
tilth "ServeHTTP, HandlersChain, Next" --scope .
```

Each symbol gets its own block of results and its own offset/limit page. The expand budget is shared across them; CLI multi-symbol queries retain an expansion floor even with `--expand=0`, while MCP honors an explicit `expand: 0`.

### Multiple scopes

Pass `--scope` more than once on the command line, or use `scopes` in MCP `tilth_search`, to search directories, files, and glob selections together. A relative query that names a file under a directory scope reads the matching file(s) and does not search the other scopes for that query as text. If no scope contains the file, tilth falls back to searching as usual.

### Callers

Find every call site of a symbol. tilth matches calls on the syntax tree, so comments and strings that mention the name are left out.

```
$ tilth isTrustedProxy --callers --scope .
# Callers of "isTrustedProxy" in ~/gin — 5 call sites

## context.go:1011 [caller: ClientIP]
-> trusted = c.engine.isTrustedProxy(remoteIP)
...
## gin.go:496 [caller: validateHeader]
-> if (i == 0) || (!engine.isTrustedProxy(ip)) {
```

Caller previews show up to 10 ranked call sites, or 100 with `--full`. On the CLI, use `--offset N` (zero-based) and `--limit N` (positive page size) to reach the remaining results:

```bash
tilth isTrustedProxy --callers --full --offset 100 --limit 100 --scope .
```

Each resolved scope is paginated independently in CLI and MCP. The output reports the range, total and next offset. Keep the query, scopes and filters unchanged between pages; edits to the source can change the ranking. `--budget` still caps the rendered output. If it truncates a page, reduce `--limit` and retry the same offset before advancing.

In MCP mode, use `kind: "callers"` on `tilth_search` with the same zero-based `offset` and positive `limit` parameters. With comma-separated targets, the page applies independently to each target. An explicit `limit` overrides the default 10-result page (100 with `full: true`). Continuation hints use JSON arguments to merge into the original request. MCP caller searches accept file, directory, and glob scopes, including `scopes` arrays. Pagination applies independently to each resolved scope and each target.

```json
{"query":"isTrustedProxy","kind":"callers","full":true,"offset":100,"limit":100}
```

### File dependencies

See what a file uses and what uses it. This is worth a look before you rename or remove an export.

```
$ tilth recovery.go --deps
# Deps: recovery.go — 5 local, 0 external, 3 dependents

## Uses (local)
context.go                     Abort, AbortWithStatus, Error, Next
debug.go                       IsDebugging
fs.go                          Open
gin.go                         New
routergroup.go                 handle

## Used by
benchmarks_test.go:22          BenchmarkRecoveryMiddleware → Recovery
benchmarks_test.go:36          BenchmarkManyHandlers → Recovery
gin.go:239                     Default              → Recovery
recovery_test.go:22            TestPanicClean       → RecoveryWithWriter
...
```

Add `--offset` and `--limit` to page dependency records:

```bash
tilth recovery.go --deps --offset 10 --limit 10
```

Records are ordered by section: local dependencies, external dependencies, production dependents, then test dependents; paths or names are sorted within each section. A dependent file and all its caller details form one record. Explicit paging collects all exported symbols and callers before selecting the page, even without `--full`.

In MCP mode, use `tilth_deps` with the same paging fields:

```json
{"path":"recovery.go","root":"/project","offset":10,"limit":10}
```

### Grok a symbol

`grok` returns everything about one symbol in a single call: signature, doc comment, body, callees, callers, siblings and tests.

```
$ tilth grok get_typed_signature --scope fastapi
# grok: get_typed_signature [dependencies/utils.py:218]

## signature
def get_typed_signature(call: Callable[..., Any]) -> inspect.Signature

## body
...

## callees (2 internal, 5 extern)
  dependencies/utils.py
    _get_signature       [203-215]   def _get_signature(call: Callable[..., Any]) -> inspect.Signature
    get_typed_annotation [235-241]   def get_typed_annotation(annotation: Any, globalns: dict[str, Any]) -> Any
...

## callers (1)
  dependencies/utils.py
    [277]   in get_dependant()
```

Grok accepts `--budget` before or after the subcommand:

```bash
tilth --budget 2000 grok get_typed_signature --scope fastapi
tilth grok get_typed_signature --scope fastapi --budget 2000
```

If supplied in both positions, the value after `grok` takes precedence.

Use `--section callers`, `callees`, `siblings`, or `tests` to retrieve just one relationship list, without expanding the target or delegate bodies:

```bash
tilth grok get_typed_signature --scope fastapi --section callers --offset 50 --limit 25
```

Section pages default to 50 individual records. Offsets are zero-based, and `--offset`/`--limit` require `--section`. Internal and external callees share one list; siblings follow source order. `--full` still widens the overview but does not cap a selected section. Keep the target selector, scope, and source unchanged between pages; use the file reader's `--section` option for source bodies.

In MCP mode, use `tilth_grok` with the same `section`, `offset`, `limit`, and optional `budget` fields:

```json
{"target":"Type::method","root":"/project","section":"callers","offset":50,"limit":25}
```

Continuation arguments preserve qualified target selectors, including methods that share a source line. Section requests do not consume the session's body-expansion state.

### Session memory

In MCP mode, a definition that was already expanded comes back as `[shown earlier]` in later searches. The agent does not receive the same body twice.

## Structural diff

```
$ tilth diff HEAD~1
# Diff: HEAD~1 — 10 files, 10 modified, 9 added (~447 tokens)
...
## binding/bson.go (3 symbols)
  [+]      Name                                     L16  (new, 3 lines)
  [+]      Bind                                     L20  (new, 7 lines)
  [+]      BindBody                                 L28  (new, 3 lines)

## context.go (3 symbols)
  [~]      <const>                                  L31  (body, 13→14 lines)
  [~]      Negotiate                                L1357  (body, 30→34 lines)
  [+]      BSON                                     L1242
...
```

The diff reports changes per function: what was added, what was removed, and whether a signature or only a body changed. Narrow it with `--scope`, or summarise a range of commits with `--log`.

Paging follows the selected view: files in an overview or directory scope, symbol/unattributed-hunk blocks in a file scope, one symbol in a `file:function` scope, or commits in log mode. Scope and search filters apply before paging:

```bash
tilth diff HEAD~1 --offset 20 --limit 20
tilth diff HEAD~1 --scope src/lib.rs --offset 10 --limit 10
tilth diff --log HEAD~20..HEAD --offset 5 --limit 5
```

MCP `tilth_diff` accepts the same `offset`/`limit` fields:

```json
{"source":"HEAD~1","scope":"src/lib.rs","root":"/project","offset":10,"limit":10}
```

Keep the diff source, scope, search filter, and working tree unchanged between pages. Warnings and conflicts retain their context and do not count as page entries.

## Why

I built tilth after watching AI agents make 6 tool calls to find one function: glob, read, "too big", grep, read again, read another file.

An agent with grep can find where a piece of text appears. To find out who calls a function, it has to read files and work out the answer, and that answer can be wrong without anyone noticing. tilth parses the code and computes the answer. The outline shows what is in a file, search shows where things are defined, and `--section` returns the lines you asked for.

## What it doesn't do

- tilth parses syntax. It does not resolve types.
- Callers and dependencies are matched by name. tilth cannot tell apart two functions with the same name in different modules, and it cannot follow dynamic dispatch.
- A language without a tree-sitter grammar in tilth gets text search and plain file reading only.

## Install

```bash
cargo install tilth
# or
npx tilth
```

Prebuilt binaries are on the [releases page](https://github.com/jahala/tilth/releases).

### MCP server

```bash
tilth install claude-code      # ~/.claude.json
tilth install cursor           # ~/.cursor/mcp.json
tilth install windsurf         # ~/.codeium/windsurf/mcp_config.json
tilth install vscode           # .vscode/mcp.json (project scope)
tilth install claude-desktop
tilth install opencode         # ~/.config/opencode/opencode.json
tilth install gemini           # ~/.gemini/settings.json
tilth install codex            # ~/.codex/config.toml
tilth install amp              # ~/.config/amp/settings.json
tilth install droid            # ~/.factory/mcp.json
tilth install antigravity      # ~/.gemini/antigravity/mcp_config.json
tilth install antigravity-cli  # ~/.gemini/config/mcp_config.json
tilth install zed              # ~/.config/zed/settings.json
tilth install copilot-cli      # ~/.copilot/mcp-config.json
tilth install augment          # ~/.augment/settings.json
tilth install kiro             # ~/.kiro/settings/mcp.json
tilth install kilo-code        # VS Code globalStorage (extension)
tilth install cline            # VS Code globalStorage (extension)
tilth install roo-code         # VS Code globalStorage (extension)
tilth install trae             # .trae/mcp.json (project scope)
tilth install qwen-code        # ~/.qwen/settings.json
tilth install crush            # ~/.config/crush/crush.json
tilth install pi               # ~/.pi/agent/mcp.json
```

Add `--edit` to turn on hash-anchored file editing (see [Edit mode](#edit-mode)):

```bash
tilth install claude-code --edit
```

For an MCP client that is not in the list, the server entry is the same everywhere. Only the config file location and the top-level key vary (`mcpServers` for most hosts, `amp.mcpServers` for Amp, TOML syntax for Codex):

```json
{
  "mcpServers": {
    "tilth": {
      "command": "tilth",
      "args": ["--mcp"]
    }
  }
}
```

For edit mode, use `"args": ["--mcp", "--edit"]`.

You can also call tilth from a shell. See [AGENTS.md](./AGENTS.md) for the MCP agent prompt, or [skills/SKILL.md](./skills/SKILL.md) for a Claude Code skill.

### Smaller models

Smaller models sometimes reach for their built-in Bash and Grep tools even when tilth is available. To make tilth the only route, turn off the tools it overlaps with:

```bash
claude --disallowedTools "Bash,Grep,Glob"
```

## How it decides what to show

| Input | Behaviour |
|-------|-----------|
| 0 bytes | `[empty]` |
| Binary | `[skipped]` with mime type |
| Generated (lockfiles, generated code) | `[generated]` |
| Minified (such as .min.js) | `[minified]` summary |
| Up to ~6,000 tokens | Full content |
| Over ~6,000 tokens | Structural outline with line ranges when meaningfully shorter; otherwise full content |

The smart-view rule counts tokens rather than lines. Binary, generated, and minified detection runs first; ordinary small modules print whole.

Piped, captured, and terminal reads use the same smart-view rules. Use `--full` to request source content explicitly. Supplying `--offset` or `--limit` selects the pageable view described below.

## Edit mode

Install with `--edit` to add `tilth_write` and switch `tilth_read` to hashline output:

```
42:a3f|  let x = compute();
43:f1b|  return x;
```

`tilth_write` takes one or more files in a single call. Each file uses one of three modes: `hash` (the default, which replaces lines at the hash anchors shown above), `overwrite` (the whole file, and create-only unless `overwrite: true`), and `append`. In hash mode the anchors must match the last read. If the file changed in the meantime, the hashes no longer match, and tilth rejects the edit and shows the current content:

```json
{
  "files": [
    {
      "path": "src/auth.ts",
      "edits": [
        { "start": "42:a3f", "content": "  let x = recompute();" },
        { "start": "44:b2c", "end": "46:e1d", "content": "" }
      ]
    }
  ]
}
```

Large files still come back as an outline first. Use `section` to get hashlined content for the part you need.

Inspired by [The Harness Problem](https://blog.can.ac/2026/02/12/the-harness-problem/).

## Usage

```bash
tilth read <path>                 # read file (outline if large)
tilth read <path> --section 45-89      # exact line range
tilth read <path> --section "Foo"      # markdown heading (optional ## prefix)
tilth read <path> --section toc:1.2    # TOC address from ambiguity hints
tilth read <path> --full               # request full content (subject to skip/size guards)
tilth <symbol> --scope <dir>      # definitions + usages
tilth <symbol> --expand=5         # inline source for top 5 matches
tilth <symbol> --full             # preview up to 100 matches instead of 10
tilth <symbol> --offset 100 --limit 25 # ranked matches beyond the preview
tilth <symbol> --callers          # find call sites (structural)
tilth grok <symbol>               # overview of one symbol
tilth grok <symbol> --section callers --limit 25 # one relationship page
tilth <path> --deps               # what a file uses, and what uses it
tilth "TODO: fix" --scope <dir>   # content search
tilth "/<regex>/" --scope <dir>   # regex search
tilth "*.test.ts" --scope <dir>   # glob files
tilth diff HEAD~1                 # structural diff (function-level)
tilth --map --scope <dir>         # codebase skeleton (CLI only)
```

`--map` is a command-line feature only. It is not offered as an MCP tool, because in testing agents called it far more often than it helped.

### Explicit file reads

`tilth read <path> [<path> ...]` reads files directly, without classifying the
argument as a search query. Relative paths use the current directory; pass
`--root <dir>` to anchor them elsewhere. Legacy `tilth <path>` remains supported.

```bash
tilth read src/main.rs --mode signature
tilth read src/main.rs --mode stripped
tilth read src/main.rs --section 1-20 --section 80-100
tilth read src/main.rs src/lib.rs --budget 2000
tilth read src/main.rs --edit
```

The CLI shares the MCP `tilth_read` implementation: smart `auto` view, `full`,
`signature`, and `stripped`, plus line ranges and Markdown headings. Use `--full`
for full content. Sections require one file and auto/full mode. Up to 20 files
or sections are accepted; batch output shares the token budget. Batch file errors
are reported alongside successful reads.

### Pagination

Offsets are zero-based nonnegative integers; limits are positive integers. Supplying either flag/field opts into paging, except directory reads, MCP `tilth_list`, and caller searches, which already return pages by default. Without explicit paging, other views retain their existing previews and smart-view behavior.

| View | One page entry | Default page size |
|------|----------------|-------------------|
| Symbol/content/regex search | Ranked match, per symbol target, after merging and deduplicating scopes | 10; 100 with `--full` / `full: true` |
| Caller search | Call site, per target and resolved scope (CLI and MCP) | 10; 100 with `--full` / `full: true` |
| Directory read | Immediate child, sorted by name | 50, including with `--full` |
| Code `auto` / code `signature` read | Declaration in source order, with nested-owner context | 50 |
| Markdown `auto` read | Heading in document order, with global TOC addresses | 50 |
| `full` or other text read | Physical source line, with original line/hash addresses | 50 |
| Code `stripped` read | Retained line, with its original source line number | 50 |
| `section` / `sections` read | Line in the selected ranges, in supplied range order | 50 |
| CLI glob / MCP `tilth_list` | Matching file, sorted by path | 50; unpaged CLI globs retain their 20-file preview |
| CLI `--map` | File, sorted by path; directory context is retained | 50 |
| Dependencies | One local/external dependency or dependent-file record | 50 |
| Diff overview / directory scope | Changed file, sorted by path | 50 |
| Diff file scope / `file:function` | Symbol or unattributed-hunk block / the selected symbol | 50 |
| Diff `--log` / MCP `log` | Commit, in Git log order | 50 |
| Grok with `section` | Caller, callee, sibling declaration, or test reference in that section | 50 |

An explicit limit overrides the default, including search's 10/100 sizes. Grok requires `section` when supplying `offset` or `limit`; its overview has independent default/`full` caps. Other paged views default to 50 entries; `--full` does not increase that page size.

#### File modes and batches

Explicit code `auto` paging requests an uncapped declaration outline even for small files. Markdown `auto` paging similarly forces a heading outline. `--full` / `mode: "full"` pages source lines instead. Code files with no recognized declarations and other non-Markdown text files fall back to source-line pages. Non-code signature/stripped requests use the normal read fallback: Markdown keeps heading pages unless full mode selects physical lines. Binary, generated, and minified files retain their skipped/summary handling.

```bash
tilth read src/main.rs --offset 100 --limit 25
tilth read src/main.rs --mode signature --offset 25 --limit 25
tilth read src/main.rs --full --offset 100 --limit 25
tilth read src/main.rs --mode stripped --offset 100 --limit 25
tilth read src/main.rs --section 40-80 --section 120-160 --offset 20 --limit 20
tilth read src/main.rs src/lib.rs --offset 25 --limit 25 --budget 4000
```

Batch `paths` and CLI multi-path reads apply the same offset/limit independently to each path; the assembled response shares the output budget. Section selection still requires one file (`path`, not batch `paths`), and cannot be combined with signature/stripped mode. Multiple ranges preserve their supplied order and overlaps; page offsets count the concatenated selected lines. Source line numbers, signature hashes, and Markdown TOC addresses are not renumbered for a page.

MCP `tilth_read` uses the same modes and units:

```json
{"root":"/project","paths":["src/main.rs","src/lib.rs"],"mode":"signature","offset":25,"limit":25,"budget":4000}
```

#### Directories, globs, and maps

```bash
tilth artifacts/ --offset 20 --limit 20
tilth read artifacts/ --offset 20 --limit 20
tilth "*.rs" --scope src --offset 50 --limit 50
tilth --map --scope src --offset 50 --limit 50
```

CLI globs and maps page each scope independently. MCP `tilth_list` pages matching files before rendering the tree; directory nodes do not count against its limit. Map and list tree counts/token rollups describe only the selected files. Keep patterns, depth, ignore settings, and scope unchanged between pages. Maps remain CLI-only; use `tilth_list` for MCP file listings:

```json
{"patterns":["*.json"],"scope":"/project/artifacts","offset":50,"limit":50}
```

#### Continuations and output budgets

`limit` counts logical entries; `budget` independently bounds the rendered output. Larger pages can still exceed the budget. If expansion is the cause, paged searches try a compact view before omitting matches. When a page or its navigation cannot fit, tilth reports the omission and withholds the continuation. Retry the same offset with a smaller limit or larger budget before advancing. Ranked searches with failed scopes report lower-bound counts and withhold continuation until the scope errors are resolved.

Use a CLI next-page hint with the original command. MCP continuation JSON is an argument patch: merge it into the original tool call rather than replacing required arguments. Keep the target/query, root, scopes, mode, filters, and source unchanged. Grok hints also preserve the original qualified target selector and section. An offset at or beyond the total returns an empty terminal page.

## Speed

Median of 15 runs of the release build (main after v0.10.1) on an Apple M5 Pro. Each time includes process startup. MCP mode pays startup once.

| Operation | Gin (130 files) | FastAPI (2,700 files) |
|-----------|-----------------|-----------------------|
| File read | 3 ms | 3 ms |
| Glob | 5 ms | 9 ms |
| Callers | 13 ms | 64 ms |
| Deps | 17 ms | 28 ms |
| Symbol search | 20 ms | 77 ms |
| Content search | 37 ms | 80 ms |
| Grok | 37 ms | 91 ms |
| Map | 52 ms | 184 ms |

Search walks the whole tree, so time grows with the size of the repository.

## What's inside

tilth is about 35,000 lines of Rust with no runtime dependencies.

- **tree-sitter** parses 18 languages: Rust, TypeScript, TSX, JavaScript, Python, Go, Java, Scala, C, C++, Ruby, PHP, C#, Swift, Kotlin, Elixir, Bash and Common Lisp. tilth uses it for definitions, callees, callers and outlines. Dockerfile and Make files are recognised but not parsed. Common Lisp supports definitions, outlines, signatures, docstrings and structural diffs for common defining forms; call graphs and macro expansion are not included. Ambiguous bare CLI queries such as `*state*` search for Lisp symbols when a matching Lisp definition exists; explicit file paths retain priority.
- **ripgrep's crates** (`grep-regex`, `grep-searcher`) run content search.
- The **ignore** crate walks directories in parallel with `.tilthignore` support and honors `.gitignore`, `.ignore` and Git exclude rules by default. It always skips common build and dependency folders such as `node_modules`, `target` and `.venv`.
- `.tilthignore` is a custom ignore file that works like `.gitignore` but is always honored, even when gitignore rules are bypassed. Place it in any directory to exclude its contents from walks.
- To bypass `.gitignore`, `.ignore` and Git excludes, use CLI `--no-respect-gitignore` or set `TILTH_RESPECT_GITIGNORE=0`. CLI `--respect-gitignore` explicitly enables them and overrides the environment. In MCP mode, pass `gitignore: false` for one call. `.tilthignore` and built-in junk-directory exclusions always remain active.
- **memmap2** reads files through memory maps.
- **DashMap** holds the outline cache, which is invalidated when a file's modified time changes.

Definitions and usages are searched in parallel through `rayon::join`. Callees are resolved when a definition is expanded. tilth extracts the callee names with tree-sitter queries and looks them up in the file's own outline and in the files it imports. The callers query runs the same patterns in reverse across the codebase, with a `memchr` pre-filter to skip files that cannot match.

tilth makes no network calls, so your code stays on your machine.

## A note on benchmarks

Earlier versions of this README carried a cost benchmark. It was measured on v0.5.0 with early-2026 models, and it no longer describes the tool or the models people use today. We have retired it and have not replaced it, so tilth makes no cost claim. The harness and its task definitions are preserved under the `benchmark-archive` tag.

## Name

**tilth** is the state of soil that has been prepared for planting. Your codebase is the soil, and tilth gives it structure so you can find where to dig.

## Support

[!["Buy Me A Coffee"](https://www.buymeacoffee.com/assets/img/custom_images/orange_img.png)](https://buymeacoffee.com/jahala)

## License

MIT
