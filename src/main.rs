use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process;

use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use tilth::FileScopeHint;

/// tilth — Tree-sitter indexed lookups, smart code reading for AI agents.
/// One tool replaces `read_file`, grep, glob, `ast_grep`, and find.
#[derive(Parser)]
#[command(name = "tilth", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// File path, symbol name, glob pattern, or text to search.
    /// Search multiple terms: tilth "Foo,Bar" --scope src
    /// Zero-hit symbol searches can show substring counts and ranked identifier suggestions.
    query: Option<String>,

    #[command(flatten)]
    search: SearchOptions,

    /// Run as MCP server (JSON-RPC on stdio).
    #[arg(long)]
    mcp: bool,

    /// Enable edit mode: hashline output + tilth_write tool.
    #[arg(long, conflicts_with_all = ["callers", "deps"])]
    edit: bool,

    /// Disable project fingerprint in MCP init.
    #[arg(long)]
    no_overview: bool,

    /// Print shell completions for the given shell.
    #[arg(long, value_name = "SHELL")]
    completions: Option<Shell>,
}

#[derive(clap::Args)]
struct SearchOptions {
    /// Literal query; repeat for a batch (commas are not split). Max 20.
    #[arg(long = "query", short = 'q')]
    queries: Vec<String>,
    /// Compact result shape, applied before paging.
    #[arg(long, value_parser = ["files", "definitions", "signatures", "count"])]
    select: Option<String>,
    /// Explicit search kind for literal batches or projections.
    #[arg(long, value_parser = ["symbol", "content", "regex"])]
    kind: Option<String>,
    /// Directory, file, or quoted glob to search within. Repeat to combine scopes.
    ///
    /// Scope globs select files and directories, honor ignore rules, and are limited
    /// to 100,000 visited entries and 1,000 matching paths across the request.
    /// Missing scopes suggest up to five filenames with normalized Levenshtein
    /// similarity >=0.8. If none qualify, lower-confidence suggestions require
    /// similarity >=0.4, the same extension, and a shared whole stem component
    /// of at least two characters. Otherwise nearby directory hints are offered.
    /// Hints fit within --budget and do not replace the scope. Valid scopes can return results alongside failed
    /// scopes. Callers page each resolved
    /// scope independently. Map, deps, and MCP startup require directory scopes.
    #[arg(long)]
    scope: Vec<PathBuf>,

    /// Respect .gitignore, .ignore, and Git exclude files (default; overrides the environment).
    #[arg(long, conflicts_with = "no_respect_gitignore")]
    respect_gitignore: bool,

    /// Bypass .gitignore, .ignore, and Git exclude files. .tilthignore remains active.
    #[arg(long, conflicts_with = "respect_gitignore")]
    no_respect_gitignore: bool,

    /// Line range (45-89), heading title (Architecture or ## Architecture), or TOC address (toc:1.2). Bypasses smart view.
    #[arg(long)]
    section: Option<String>,

    /// Max tokens in response. Reduces detail to fit.
    #[arg(long)]
    budget: Option<u64>,

    /// Force full output (effect depends on query type — see --help).
    ///
    /// File path: return the whole file instead of an outline (bypass smart view).
    ///
    /// Symbol / text / regex / callers: show up to 100 matches and inline
    /// source for up to 50. Explicit `--expand=N` wins. Output stays bounded
    /// by `--budget`.
    ///
    /// Glob: no effect (glob queries already return a flat file list).
    ///
    /// Deps: search all symbols/call sites and show all dependencies, within --budget.
    #[arg(long)]
    full: bool,

    /// Machine-readable JSON output.
    #[arg(long)]
    json: bool,

    /// Inline source for top N search matches (default 2 when flag bare).
    ///
    /// Applies to symbol / text / regex queries. Without the flag the
    /// result is just the outline summary. `--full` upgrades this to
    /// expand up to 50 matches (subject to `--budget`); explicit `--expand=N`
    /// wins over `--full`. No effect on file-path or glob queries.
    #[arg(long, num_args = 0..=1, default_missing_value = "2", require_equals = true)]
    expand: Option<usize>,

    /// File pattern filter (e.g. "*.rs", "!*.test.ts", "*.{go,rs}").
    ///
    /// For symbol/text/regex/callers results, narrows each resolved scope
    /// relative to its directory or a selected file's parent. Intersects with
    /// file/glob scopes and honors ignore rules.
    #[arg(long)]
    glob: Option<String>,

    /// Minimum normalized similarity for zero-hit suggestions (default 0.80).
    #[arg(long)]
    similarity_threshold: Option<f64>,

    /// Maximum number of files eligible for content similarity (default 500, inclusive).
    #[arg(long)]
    similarity_max_files: Option<std::num::NonZeroUsize>,

    /// Maximum total file bytes eligible for content similarity (default 20000000, inclusive).
    #[arg(long)]
    similarity_max_bytes: Option<std::num::NonZeroU64>,

    /// Skip N entries in the selected view (zero-based): search matches, files, declarations, or lines.
    #[arg(long)]
    offset: Option<usize>,

    /// Entries per page (default 50; search defaults to 10/100). Unit depends on the view.
    /// Output remains subject to --budget.
    #[arg(long)]
    limit: Option<std::num::NonZeroUsize>,

    /// Find callers of a symbol; use --offset and --limit to paginate.
    #[arg(long, conflicts_with_all = ["deps", "map"])]
    callers: bool,

    /// Analyze blast-radius dependencies of a file.
    #[arg(long, conflicts_with_all = ["callers", "map"])]
    deps: bool,

    /// Generate a structural codebase map.
    #[arg(long, conflicts_with_all = ["callers", "deps", "expand", "section", "full"])]
    map: bool,
}

impl SearchOptions {
    fn merge(&mut self, options: Self) {
        self.queries.extend(options.queries);
        self.select = options.select.or(self.select.take());
        self.kind = options.kind.or(self.kind.take());
        self.scope.extend(options.scope);
        self.respect_gitignore |= options.respect_gitignore;
        self.no_respect_gitignore |= options.no_respect_gitignore;
        self.section = options.section.or(self.section.take());
        self.budget = options.budget.or(self.budget);
        self.full |= options.full;
        self.json |= options.json;
        self.expand = options.expand.or(self.expand);
        self.glob = options.glob.or(self.glob.take());
        self.similarity_threshold = options.similarity_threshold.or(self.similarity_threshold);
        self.similarity_max_files = options.similarity_max_files.or(self.similarity_max_files);
        self.similarity_max_bytes = options.similarity_max_bytes.or(self.similarity_max_bytes);
        self.offset = options.offset.or(self.offset);
        self.limit = options.limit.or(self.limit);
        self.callers |= options.callers;
        self.deps |= options.deps;
        self.map |= options.map;
    }

    fn similarity_args(&self) -> serde_json::Value {
        let mut args = serde_json::json!({});
        if let Some(value) = self.similarity_threshold {
            args["similarity_threshold"] = value.into();
        }
        if let Some(value) = self.similarity_max_files {
            args["similarity_max_files"] = value.get().into();
        }
        if let Some(value) = self.similarity_max_bytes {
            args["similarity_max_bytes"] = value.get().into();
        }
        args
    }

    fn validate(&self, edit: bool) -> Result<(), clap::Error> {
        if self
            .similarity_threshold
            .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
        {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::ValueValidation,
                "--similarity-threshold must be between 0 and 1",
            ));
        }
        let compact = !self.queries.is_empty() || self.select.is_some() || self.kind.is_some();
        let conflict = if compact
            && (self.deps || self.map || self.callers || self.section.is_some() || edit)
        {
            Some("--query, --select, and --kind conflict with --deps, --map, --callers, --section, and --edit")
        } else if self.respect_gitignore && self.no_respect_gitignore {
            Some("--respect-gitignore conflicts with --no-respect-gitignore")
        } else if self.callers && (self.deps || self.map || edit) {
            Some("--callers conflicts with --deps, --map, and --edit")
        } else if self.deps && (self.map || edit) {
            Some("--deps conflicts with --map and --edit")
        } else if self.map && (self.expand.is_some() || self.section.is_some() || self.full) {
            Some("--map conflicts with --expand, --section, and --full")
        } else {
            None
        };
        match conflict {
            Some(message) => Err(clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                message,
            )),
            None => Ok(()),
        }
    }
}

#[derive(clap::Subcommand)]
enum Command {
    /// Read files directly without query classification.
    Read {
        /// File paths, at most 20.
        #[arg(required_unless_present = "slices", num_args = 1..=20, conflicts_with = "slices")]
        paths: Vec<PathBuf>,
        /// Per-file range path:start-end; repeat to combine files and merge overlapping ranges.
        #[arg(long = "slice")]
        slices: Vec<String>,
        /// Exact symbol name; repeat to fetch multiple declarations. Use name@line to disambiguate.
        #[arg(long = "symbol")]
        symbols: Vec<String>,
        /// Directory anchoring relative paths; defaults to the current directory.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Read view.
        #[arg(long, default_value = "auto", value_parser = ["auto", "full", "signature", "stripped"])]
        mode: String,
        /// Force full content; signature and stripped modes take precedence.
        #[arg(long)]
        full: bool,
        /// Line range, heading title (optional # prefix), or TOC address (toc:1.2); repeat for disjoint slices.
        #[arg(long)]
        section: Vec<String>,
        /// Maximum output tokens; also accepted before read.
        #[arg(long)]
        budget: Option<u64>,
        /// Skip N declarations in auto/code or signature mode, headings in auto/Markdown,
        /// source lines in full mode, or retained lines in stripped mode (zero-based, per path).
        #[arg(long)]
        offset: Option<usize>,
        /// Entries per page per path (default 50); selected sections page their source lines.
        #[arg(long)]
        limit: Option<std::num::NonZeroUsize>,
        /// Emit hash anchors; also accepted before read.
        #[arg(long)]
        edit: bool,
        /// View-tag emission policy (opt-in). Auto is experimental, >=1000 estimated tokens.
        #[arg(long, value_parser=["auto","always","never"])]
        tag: Option<String>,
        /// Return View unchanged when this quoted five-word view tag matches.
        #[arg(long)]
        if_tag: Option<String>,
        /// Machine-readable read status, tag and output.
        #[arg(long)]
        json: bool,
    },
    /// Search for a file, symbol, glob pattern, or text.
    Search {
        /// File path, symbol name, glob pattern, or text search.
        #[arg(required_unless_present = "queries")]
        query: Option<String>,
        #[command(flatten)]
        options: SearchOptions,
    },
    /// Install tilth into an MCP host's config.
    /// Supported hosts: claude-code, cursor, windsurf, vscode, claude-desktop, opencode, gemini, codex, amp, droid, antigravity, antigravity-cli, zed, copilot-cli, augment, kiro, kilo-code, cline, roo-code, trae, qwen-code, crush, pi
    Install {
        /// MCP host to configure.
        host: String,

        /// Enable edit mode (hashline output + tilth_write tool).
        #[arg(long)]
        edit: bool,
    },
    /// Show structural diff with function-level change summaries.
    Diff {
        /// Diff source: uncommitted (default), staged, or a git ref (e.g. HEAD~1, main..feat).
        #[arg(default_value = "uncommitted")]
        source: String,

        /// Restrict diff to a specific file or directory.
        #[arg(long)]
        scope: Option<String>,

        /// First file for file-to-file diff (requires --b).
        #[arg(long)]
        a: Option<PathBuf>,

        /// Second file for file-to-file diff (requires --a).
        #[arg(long)]
        b: Option<PathBuf>,

        /// Path to a .patch file to parse.
        #[arg(long)]
        patch: Option<PathBuf>,

        /// Git log range for per-commit summaries (e.g. HEAD~5..HEAD).
        #[arg(long)]
        log: Option<String>,

        /// Filter output to symbols or files matching this substring.
        #[arg(long)]
        search: Option<String>,

        /// Show blast-radius warnings for signature-changed symbols.
        #[arg(long)]
        blast: bool,

        /// Expand top N changed symbols with full source context.
        #[arg(long, default_value_t = 0)]
        expand: usize,

        /// Max tokens in response.
        #[arg(long, default_value_t = 10000)]
        budget: u64,

        /// Skip N files (overview), symbols/hunks (file detail), or commits (log), zero-based.
        #[arg(long)]
        offset: Option<usize>,
        /// Entries per diff page (default 50). Keep the source and working tree unchanged between pages.
        #[arg(long)]
        limit: Option<std::num::NonZeroUsize>,
    },
    /// Show a project fingerprint; use -r to include deeply nested sources.
    Overview {
        /// Directories to summarize (defaults to the current directory).
        #[arg(conflicts_with = "scope")]
        directories: Vec<PathBuf>,
        /// Directories to summarize; alternative to positional directories.
        #[arg(long, conflicts_with = "directories")]
        scope: Vec<PathBuf>,
        /// Walk all source directories instead of stopping at depth two.
        #[arg(short, long)]
        recursive: bool,
        /// Skip N source directories, ordered by depth then path, per project.
        #[arg(long)]
        offset: Option<usize>,
        /// Source directories per page (default 10), per project.
        #[arg(long)]
        limit: Option<std::num::NonZeroUsize>,
    },
    /// Trace bounded static calls, with explicit heuristic and completeness limits.
    Trace {
        from: String,
        #[arg(long)]
        to: Option<String>,
        #[arg(long, default_value = "callees", value_parser = ["callers", "callees"])]
        direction: String,
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long)]
        scope: Option<PathBuf>,
        #[arg(long)]
        budget: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Grok a symbol — one call returns def + doc + callees + callers + siblings + tests.
    ///
    /// Target accepts: bare symbol (`parse_unified_diff`), path:line
    /// (`src/diff/parse.rs:7`), or qualified name (`Type::method`).
    Grok {
        /// Symbol name or path:line.
        target: String,

        /// Restrict search to a subdirectory.
        #[arg(long, default_value = ".")]
        scope: PathBuf,

        /// Max tokens in response (also accepted before grok).
        #[arg(long)]
        budget: Option<u64>,

        /// Widen overview caps; explicit section pages always collect the complete section.
        #[arg(long)]
        full: bool,

        /// Return only this relationship section; source bodies are not expanded.
        #[arg(long,value_parser=["callers","callees","siblings","tests"])]
        section: Option<String>,
        /// Skip N individual records in the selected section (zero-based).
        #[arg(long)]
        offset: Option<usize>,
        /// Records per section page (default 50). Requires --section.
        #[arg(long)]
        limit: Option<std::num::NonZeroUsize>,
    },
}

fn main() {
    let mut cli = Cli::parse();
    if let Some(Command::Search { query, options }) = cli
        .command
        .take_if(|cmd| matches!(cmd, Command::Search { .. }))
    {
        cli.query = query;
        cli.search.merge(options);
    }
    if let Err(error) = cli.search.validate(cli.edit) {
        error.exit();
    }

    if cli.search.respect_gitignore {
        std::env::set_var("TILTH_RESPECT_GITIGNORE", "1");
    } else if cli.search.no_respect_gitignore {
        std::env::set_var("TILTH_RESPECT_GITIGNORE", "0");
    }
    configure_thread_pools();

    // Shell completions
    if let Some(shell) = cli.completions {
        // clap_complete panics on writer errors; generate in memory first.
        let mut output = Vec::new();
        clap_complete::generate(shell, &mut Cli::command(), "tilth", &mut output);
        emit_output(
            &String::from_utf8(output).expect("completions are UTF-8"),
            false,
        );
        return;
    }

    // Subcommands
    if let Some(cmd) = cli.command {
        match cmd {
            Command::Read {
                paths,
                slices,
                symbols,
                root,
                mode,
                full,
                mut section,
                budget,
                offset,
                limit,
                edit,
                tag,
                if_tag,
                json,
            } => {
                if let Some(parent_section) = cli.search.section {
                    section.insert(0, parent_section);
                }
                if paths.len() > 1 && !section.is_empty() {
                    clap::Error::raw(
                        clap::error::ErrorKind::ArgumentConflict,
                        "--section requires exactly one file",
                    )
                    .exit();
                }
                let root =
                    root.unwrap_or_else(|| std::env::current_dir().expect("current directory"));
                let root = root.canonicalize().unwrap_or(root);
                let mut args = serde_json::json!({
                    "root": root, "mode": mode, "full": full || cli.search.full,
                    "budget": budget.or(cli.search.budget),
                });
                if let Some(offset) = offset.or(cli.search.offset) {
                    args["offset"] = serde_json::json!(offset);
                }
                if let Some(limit) = limit.or(cli.search.limit) {
                    args["limit"] = serde_json::json!(limit.get());
                }
                if !slices.is_empty() {
                    let specs: Vec<_> = slices
                        .iter()
                        .map(|slice| {
                            let (path, section) = slice
                                .rsplit_once(':')
                                .unwrap_or_else(|| usage_exit("--slice requires path:start-end"));
                            if path.is_empty() {
                                usage_exit("--slice path must not be empty");
                            }
                            serde_json::json!({"path":path,"sections":[section]})
                        })
                        .collect();
                    args["slices"] = serde_json::json!(specs);
                } else if paths.len() == 1 {
                    args["path"] = serde_json::json!(paths[0]);
                } else {
                    args["paths"] = serde_json::json!(paths);
                }
                if !symbols.is_empty() {
                    args["symbols"] = serde_json::json!(symbols);
                }
                if section.len() == 1 {
                    args["section"] = serde_json::json!(section[0]);
                } else if !section.is_empty() {
                    args["sections"] = serde_json::json!(section);
                }
                if let Some(tag) = tag {
                    args["tag"] = tag.into();
                }
                if let Some(tag) = if_tag {
                    args["if_tag"] = tag.into();
                }
                match tilth::mcp::read_response_for_cli(&args, edit || cli.edit) {
                    Ok(response) => {
                        if json || cli.search.json {
                            println!("{}", response.json());
                        } else {
                            emit_output(&response.text(), io::stdout().is_terminal());
                        }
                    }
                    Err(error) => {
                        tilth::diagnostic!("read error: {error}");
                        process::exit(1);
                    }
                }
            }
            Command::Search { .. } => unreachable!("search is handled as a query"),
            Command::Install { ref host, edit } => {
                if let Err(e) = tilth::install::run(host, edit) {
                    tilth::diagnostic!("install error: {e}");
                    process::exit(1);
                }
            }
            Command::Overview {
                directories,
                scope,
                recursive,
                offset,
                limit,
            } => {
                if !directories.is_empty() && !cli.search.scope.is_empty() {
                    tilth::diagnostic!(
                        "positional overview directories cannot be mixed with --scope"
                    );
                    process::exit(2);
                }
                let mut scope = scope;
                scope.extend(cli.search.scope);
                let roots = if directories.is_empty() {
                    scope
                } else {
                    directories
                };
                let roots = if roots.is_empty() {
                    vec![PathBuf::from(".")]
                } else {
                    roots
                };
                let offset = offset.or(cli.search.offset).unwrap_or(0);
                let limit = limit
                    .or(cli.search.limit)
                    .map_or(10, std::num::NonZeroUsize::get);
                let mut outputs = Vec::new();
                for root in &roots {
                    if !root.is_dir() {
                        tilth::diagnostic!("overview scope is not a directory: {}", root.display());
                        process::exit(1);
                    }
                    let summary = tilth::overview::fingerprint_page(root, recursive, offset, limit);
                    outputs.push(if roots.len() > 1 {
                        format!("## {}\n{summary}", root.display())
                    } else {
                        summary
                    });
                }
                let output = outputs.join("\n\n");
                if output.is_empty() {
                    tilth::diagnostic!("No project fingerprint could be generated.");
                    process::exit(1);
                }
                emit_output(&output, false);
            }
            Command::Trace {
                from,
                to,
                direction,
                depth,
                scope,
                budget,
                json,
            } => {
                if cli.search.scope.len() > 1 {
                    usage_exit("trace accepts one directory scope");
                }
                let root = std::env::current_dir().expect("current directory");
                let scope = scope
                    .or_else(|| cli.search.scope.first().cloned())
                    .unwrap_or_else(|| PathBuf::from("."));
                let mut args = serde_json::json!({"root":root,"scope":scope,"from":from,"direction":direction,"depth":depth});
                if let Some(to) = to {
                    args["to"] = to.into();
                }
                if let Some(budget) = budget.or(cli.search.budget) {
                    args["budget"] = budget.into();
                }
                match tilth::mcp::trace_for_cli(&args) {
                    Ok(output) => emit_result(
                        Ok(output),
                        &from,
                        json || cli.search.json,
                        io::stdout().is_terminal(),
                        budget.or(cli.search.budget),
                    ),
                    Err(error) => {
                        tilth::diagnostic!("trace error: {error}");
                        process::exit(1);
                    }
                }
            }
            Command::Grok {
                target,
                scope,
                full,
                budget,
                section,
                offset,
                limit,
            } => {
                let scope =
                    cli_scope(scope, FileScopeHint::Directory).unwrap_or_else(|e| usage_exit(&e));
                let section = section.or(cli.search.section);
                let offset = offset.or(cli.search.offset);
                let limit = limit.or(cli.search.limit);
                let page = (offset.is_some() || limit.is_some())
                    .then_some((offset.unwrap_or(0), limit.map(std::num::NonZeroUsize::get)));
                match tilth::run_grok_paginated(
                    &target,
                    &scope,
                    full || cli.search.full,
                    section.as_deref(),
                    page,
                    budget.or(cli.search.budget),
                ) {
                    Ok(output) => emit_output(&output, io::stdout().is_terminal()),
                    Err(e) => {
                        tilth::diagnostic!("grok error: {e}");
                        process::exit(e.exit_code());
                    }
                }
            }
            Command::Diff {
                source,
                scope,
                a,
                b,
                patch,
                log,
                search,
                blast,
                expand,
                budget,
                offset,
                limit,
            } => {
                let a_str = a.as_ref().map(|p| p.to_string_lossy().into_owned());
                let b_str = b.as_ref().map(|p| p.to_string_lossy().into_owned());
                let patch_str = patch.as_ref().map(|p| p.to_string_lossy().into_owned());
                let diff_source = match tilth::diff::resolve_source(
                    Some(&source),
                    a_str.as_deref(),
                    b_str.as_deref(),
                    patch_str.as_deref(),
                    log.as_deref(),
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        tilth::diagnostic!("diff error: {e}");
                        process::exit(1);
                    }
                };
                let budget_opt = if budget == 0 { None } else { Some(budget) };
                match tilth::diff::diff_paginated(
                    &diff_source,
                    None, // repo: the CLI already runs in the caller's checkout
                    scope.as_deref(),
                    search.as_deref(),
                    blast,
                    expand,
                    budget_opt,
                    if offset.or(cli.search.offset).is_some()
                        || limit.or(cli.search.limit).is_some()
                    {
                        Some((
                            offset.or(cli.search.offset).unwrap_or(0),
                            limit.or(cli.search.limit).map(std::num::NonZeroUsize::get),
                        ))
                    } else {
                        None
                    },
                    false,
                ) {
                    Ok(output) => emit_output(&output, io::stdout().is_terminal()),
                    Err(e) => {
                        tilth::diagnostic!("diff error: {e}");
                        process::exit(1);
                    }
                }
            }
        }
        return;
    }

    // MCP mode: JSON-RPC server
    if cli.mcp {
        if cli.no_overview {
            std::env::set_var("TILTH_NO_OVERVIEW", "1");
        }
        // Pass --scope to MCP if it's not the default "."
        let mcp_scope = if scopes_are_default(&cli.search.scope) {
            None
        } else if cli.search.scope.len() == 1 {
            Some(
                cli_scope(cli.search.scope[0].clone(), FileScopeHint::Directory)
                    .unwrap_or_else(|e| usage_exit(&e)),
            )
        } else {
            tilth::diagnostic!("mcp mode accepts only one --scope");
            process::exit(2);
        };
        if let Err(e) = tilth::mcp::run(cli.edit, mcp_scope.as_deref()) {
            tilth::diagnostic!("mcp error: {e}");
            process::exit(1);
        }
        return;
    }

    let is_tty = io::stdout().is_terminal();

    // Map mode
    if cli.search.map {
        let cache = tilth::cache::OutlineCache::new();
        let scopes = resolve_scopes(&cli.search.scope);
        for scope in &scopes {
            tilth::refuse_file_scope(scope, scope, FileScopeHint::Directory)
                .unwrap_or_else(|e| usage_exit(&e));
        }
        let output = run_for_scopes(&scopes, |scope| {
            std::fs::metadata(scope)
                .map_err(|source| tilth::error::scope_io_error(scope, source))?;
            if cli.search.offset.is_some() || cli.search.limit.is_some() {
                tilth::map::generate_paginated(
                    scope,
                    3,
                    cli.search.budget,
                    &cache,
                    (
                        cli.search.offset.unwrap_or(0),
                        cli.search.limit.map(std::num::NonZeroUsize::get),
                    ),
                )
                .map_err(|reason| tilth::error::TilthError::InvalidQuery {
                    query: scope.display().to_string(),
                    reason,
                })
            } else {
                Ok(tilth::map::generate(scope, 3, cli.search.budget, &cache))
            }
        });
        let output = if cli.search.offset.is_some() || cli.search.limit.is_some() {
            tilth::finish_paged_output(&output, cli.search.budget)
        } else {
            output
        };
        let output = apply_optional_budget(output, cli.search.budget);
        emit_output(&output, is_tty);
        return;
    }

    if !cli.search.queries.is_empty() || cli.search.select.is_some() || cli.search.kind.is_some() {
        if cli.query.is_some() && !cli.search.queries.is_empty() {
            usage_exit("positional query and --query are mutually exclusive");
        }
        if cli.search.expand.is_some_and(|n| n > 0) {
            usage_exit("literal batches and projections do not expand source; use --expand=0");
        }
        let root = std::env::current_dir().expect("current directory");
        let mut args = serde_json::json!({"root":root,"kind":cli.search.kind.as_deref().unwrap_or("symbol"),"full":cli.search.full});
        for (key, value) in cli.search.similarity_args().as_object().unwrap() {
            args[key] = value.clone();
        }
        if cli.search.queries.is_empty() {
            let query = cli
                .query
                .as_deref()
                .unwrap_or_else(|| usage_exit("missing query"));
            if cli.search.select.is_some() {
                args["query"] = query.into();
            } else {
                args["queries"] = serde_json::json!([query]);
            }
        } else {
            args["queries"] = serde_json::json!(cli.search.queries);
        }
        if !cli.search.scope.is_empty() {
            args["scopes"] = serde_json::json!(cli.search.scope);
        }
        if let Some(value) = cli.search.select {
            args["select"] = value.into();
        }
        if let Some(value) = cli.search.glob {
            args["glob"] = value.into();
        }
        if let Some(value) = cli.search.offset {
            args["offset"] = value.into();
        }
        if let Some(value) = cli.search.limit {
            args["limit"] = value.get().into();
        }
        if let Some(value) = cli.search.budget {
            args["budget"] = value.into();
        }
        match tilth::mcp::search_for_cli(&args) {
            Ok(output) => emit_result(
                Ok(output),
                &args
                    .get("query")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        args["queries"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ")
                    }),
                cli.search.json,
                is_tty,
                cli.search.budget,
            ),
            Err(error) => {
                tilth::diagnostic!(
                    "{}",
                    apply_optional_budget(format!("search error: {error}"), cli.search.budget)
                );
                process::exit(1);
            }
        }
        return;
    }

    // CLI mode: single query
    let query = if let Some(q) = cli.query {
        q
    } else {
        tilth::diagnostic!(
            "usage: tilth <query> [--scope PATH_OR_GLOB] [--section N-M] [--budget N]"
        );
        process::exit(3);
    };

    let cache = tilth::cache::OutlineCache::new();
    let scopes = resolve_scopes(&cli.search.scope);
    if cli.search.deps {
        for scope in &scopes {
            tilth::refuse_file_scope(scope, scope, FileScopeHint::Directory)
                .unwrap_or_else(|e| usage_exit(&e));
        }
    }
    let scopes = if cli.search.deps {
        scopes
    } else {
        match tilth::search_scopes::expand(&scopes) {
            Ok(scopes) => scopes,
            Err(error) => {
                emit_result(
                    Err(error),
                    &query,
                    cli.search.json,
                    is_tty,
                    cli.search.budget,
                );
                return;
            }
        }
    };

    // Output destination only controls paging. File views and search expansion
    // are selected by explicit flags, so captured agent calls keep smart views.
    let expand = compute_expand(cli.search.expand, cli.search.full);

    // Callers mode
    if cli.search.callers {
        let result = run_query_for_scopes(&scopes, &query, None, |scope| {
            tilth::run_callers_paginated(
                &query,
                scope,
                expand,
                None,
                cli.search.glob.as_deref(),
                cli.search.full,
                (
                    cli.search.offset.unwrap_or(0),
                    cli.search.limit.map(|limit| limit.get()),
                ),
            )
        });
        let result = result.map(|output| match cli.search.budget {
            Some(budget) => tilth::apply_callers_output_budget(&output, budget),
            None => output,
        });
        emit_result(result, &query, cli.search.json, is_tty, cli.search.budget);
        return;
    }

    // Deps mode
    if cli.search.deps {
        let result = run_query_for_scopes(
            &scopes,
            &query,
            if cli.search.offset.is_some() || cli.search.limit.is_some() {
                None
            } else {
                cli.search.budget
            },
            |scope| {
                std::fs::metadata(scope)
                    .map_err(|source| tilth::error::scope_io_error(scope, source))?;
                let path = resolve_query_path(&query, scope);
                let page = (cli.search.offset.is_some() || cli.search.limit.is_some()).then_some((
                    cli.search.offset.unwrap_or(0),
                    cli.search.limit.map(std::num::NonZeroUsize::get),
                ));
                tilth::run_deps_paginated(&path, scope, cli.search.budget, cli.search.full, page)
            },
        );
        let result = result.map(|output| {
            if cli.search.offset.is_some() || cli.search.limit.is_some() {
                tilth::finish_paged_output(&output, cli.search.budget)
            } else {
                output
            }
        });
        emit_result(result, &query, cli.search.json, is_tty, cli.search.budget);
        return;
    }

    let result = tilth::run_expanded_scopes_with_listing(
        &query,
        &scopes,
        cli.search.section.as_deref(),
        cli.search.budget,
        cli.search.full,
        expand,
        cli.search.glob.as_deref(),
        &cache,
        cli.search.full,
        (cli.search.offset.is_some() || cli.search.limit.is_some()).then_some((
            cli.search.offset.unwrap_or(0),
            cli.search.limit.map(std::num::NonZeroUsize::get),
        )),
    );

    let mut similarity_args = cli.search.similarity_args();
    similarity_args["_similarity_page_requested"] =
        serde_json::json!(cli.search.offset.is_some() || cli.search.limit.is_some());
    let result = tilth::append_cli_similarity(
        result,
        &query,
        &scopes,
        cli.search.glob.as_deref(),
        &similarity_args,
        cli.search.offset.unwrap_or(0),
        cli.search
            .limit
            .map_or(if cli.search.full { 100 } else { 10 }, |v| v.get()),
        cli.search.budget,
    );
    emit_result(result, &query, cli.search.json, is_tty, cli.search.budget);
}

fn scopes_are_default(scopes: &[PathBuf]) -> bool {
    scopes.is_empty() || (scopes.len() == 1 && scopes[0].as_os_str() == ".")
}

fn resolve_scopes(scopes: &[PathBuf]) -> Vec<PathBuf> {
    if scopes.is_empty() {
        return vec![PathBuf::from(".")
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."))];
    }

    scopes
        .iter()
        .map(|scope| scope.canonicalize().unwrap_or_else(|_| scope.clone()))
        .collect()
}

fn resolve_query_path(query: &str, scope: &Path) -> PathBuf {
    if Path::new(query).is_absolute() {
        return PathBuf::from(query);
    }

    let scoped = scope.join(query);
    if scoped.exists() {
        return scoped;
    }

    let cwd_path = std::env::current_dir().unwrap_or_default().join(query);
    if cwd_path.exists() {
        return cwd_path;
    }

    scoped
}

fn run_for_scopes<F>(scopes: &[PathBuf], mut run: F) -> String
where
    F: FnMut(&Path) -> Result<String, tilth::error::TilthError>,
{
    if scopes.len() == 1 {
        return run(&scopes[0]).unwrap_or_else(|e| e.to_string());
    }

    scopes
        .iter()
        .map(|scope| {
            let output = run(scope).unwrap_or_else(|e| e.to_string());
            format!("# Scope: {}\n\n{output}", scope.display())
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n")
}

fn run_query_for_scopes<F>(
    scopes: &[PathBuf],
    query: &str,
    budget: Option<u64>,
    mut run: F,
) -> Result<String, tilth::error::TilthError>
where
    F: FnMut(&Path) -> Result<String, tilth::error::TilthError>,
{
    if scopes.len() == 1 {
        return run(&scopes[0]);
    }

    let mut outputs = Vec::new();
    let mut failures = Vec::new();
    for scope in scopes {
        match run(scope) {
            Ok(output) => outputs.push(format!("# Scope: {}\n\n{output}", scope.display())),
            Err(error) => failures.push(tilth::error::ScopeError {
                scope: scope.clone(),
                error,
            }),
        }
    }

    if outputs.is_empty() {
        return match failures.len() {
            0 => Err(tilth::error::TilthError::NotFound {
                path: PathBuf::from(query),
                suggestion: None,
            }),
            1 => Err(failures.pop().expect("one scope failure").error),
            _ => Err(tilth::error::TilthError::ScopedFailures { failures }),
        };
    }

    let results = outputs.join("\n\n---\n");
    if failures.is_empty() {
        return Ok(apply_optional_budget(results, budget));
    }
    let mut prelude = format!("# Search: \"{query}\"\n\n## Scope errors\n");
    for failure in failures {
        use std::fmt::Write as _;
        let _ = writeln!(prelude, "- {}: {}", failure.scope.display(), failure.error);
    }
    Ok(apply_optional_budget(
        format!("{}\n\n{results}", prelude.trim_end()),
        budget,
    ))
}

fn apply_optional_budget(output: String, budget: Option<u64>) -> String {
    match budget {
        Some(budget) => tilth::apply_output_budget(&output, budget),
        None => output,
    }
}

/// Canonicalize `--scope`, refusing a file the same way the MCP tools do.
/// A path that does not exist passes through unchanged, as before.
fn cli_scope(raw: PathBuf, hint: FileScopeHint) -> Result<PathBuf, String> {
    let scope = raw.canonicalize().unwrap_or_else(|_| raw.clone());
    tilth::refuse_file_scope(&raw, &scope, hint)?;
    Ok(scope)
}

/// Report a usage error and exit with the invalid-input code.
fn usage_exit(msg: &str) -> ! {
    eprintln!("{msg}");
    process::exit(3);
}

fn emit_result(
    result: Result<String, tilth::error::TilthError>,
    query: &str,
    json: bool,
    is_tty: bool,
    budget: Option<u64>,
) {
    match result {
        Ok(output) => {
            if json {
                let json = serde_json::json!({
                    "query": query,
                    "output": output,
                });
                emit_output(
                    &serde_json::to_string_pretty(&json)
                        .expect("serde_json::Value is always serializable"),
                    false,
                );
            } else {
                emit_output(&output, is_tty);
            }
        }
        Err(e) => {
            tilth::diagnostic!("{}", apply_optional_budget(e.to_string(), budget));
            process::exit(e.exit_code());
        }
    }
}

/// Write output to stdout. When TTY and output is long, pipe through $PAGER.
fn emit_output(output: &str, is_tty: bool) {
    let line_count = output.lines().count();
    let term_height = terminal_height();

    if is_tty && line_count > term_height {
        let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
        if let Ok(mut child) = process::Command::new(&pager)
            .arg("-R")
            .stdin(process::Stdio::piped())
            .spawn()
        {
            let result = child
                .stdin
                .take()
                .map_or(Ok(()), |mut stdin| stdin.write_all(output.as_bytes()));
            let _ = child.wait();
            // A pager may close early when the user quits. Other sink failures
            // still mean the response was not delivered.
            if let Err(error) = result {
                if error.kind() != io::ErrorKind::BrokenPipe {
                    tilth::diagnostic!("output error: {error}");
                    process::exit(1);
                }
            }
            return;
        }
    }

    // Conventional CLI output ends with a newline so the next shell prompt
    // starts on its own line. Most internal formatters terminate with `\n`,
    // but the search-result footer (e.g. `(~507 tokens)`) and a few other
    // paths do not — guard at the sink rather than auditing every formatter.
    if let Err(error) = tilth::output::write_output(&mut io::stdout().lock(), output) {
        tilth::diagnostic!("output error: {error}");
        process::exit(1);
    }
}

fn terminal_height() -> usize {
    // ioctl(TIOCGWINSZ) on stdout — the real terminal height. Bash maintains
    // $LINES as an unexported shell variable, so most subprocesses (us included)
    // never receive it; relying on it alone made tilth assume 24 rows in any
    // typical interactive shell and page nearly every result. Fall back to
    // $LINES then to 24 only if the ioctl is unavailable (tests, exotic TTYs).
    if let Some((_, terminal_size::Height(h))) = terminal_size::terminal_size() {
        return h as usize;
    }
    if let Ok(lines) = std::env::var("LINES") {
        if let Ok(h) = lines.parse::<usize>() {
            return h;
        }
    }
    24
}

/// Configure rayon global thread pool to limit CPU usage.
///
/// Defaults to min(cores / 2, 6). Override with `TILTH_THREADS` env var.
/// This matters for long-lived MCP sessions where back-to-back searches
/// can sustain high CPU (see #27).
fn configure_thread_pools() {
    let num_threads = std::env::var("TILTH_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism().map_or(4, |n| (n.get() / 2).clamp(2, 6))
        });

    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .build_global()
        .ok();
}

/// Compute the effective `expand` value for a search query from the raw
/// CLI flags. Lifted out of `main` so the `--full` / `--expand` precedence
/// is unit-testable.
///
/// Precedence:
/// - Explicit `--expand=N` always wins (`cli_expand = Some(n)`), even alongside `--full`.
/// - Bare `--full` with no `--expand` → `FULL_EXPAND_CAP` (50).
/// - Neither flag → 0 (no expansion).
///
/// Subprocess and pipeline callers keep the same concise default as terminal
/// callers. Expanding all matches requires an explicit `--full`.
fn compute_expand(cli_expand: Option<usize>, cli_full: bool) -> usize {
    /// `--budget` already bounds output, but `expand=usize::MAX` makes tilth
    /// compute the expanded source for every match before truncating —
    /// wasted parsing + rendering on pathological queries. 50 is well above
    /// any practical "show me everything that matters" case (MAX_MATCHES is
    /// 10 for symbol search anyway).
    const FULL_EXPAND_CAP: usize = 50;
    match (cli_expand, cli_full) {
        (Some(n), _) => n,
        (None, true) => FULL_EXPAND_CAP,
        (None, false) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin `--expand=N` precedence — explicit value always wins, including
    /// when combined with `--full`.
    #[test]
    fn explicit_expand_wins_over_full() {
        assert_eq!(compute_expand(Some(2), false), 2);
        assert_eq!(compute_expand(Some(2), true), 2);
        assert_eq!(compute_expand(Some(0), false), 0);
        assert_eq!(compute_expand(Some(0), true), 0);
        assert_eq!(compute_expand(Some(99), true), 99);
    }

    /// Pin `--full` → expand=50 when no explicit `--expand`.
    #[test]
    fn bare_full_promotes_to_full_expand_cap() {
        assert_eq!(compute_expand(None, true), 50);
    }

    /// Pin the default — neither flag means no expansion.
    #[test]
    fn neither_flag_means_zero_expand() {
        assert_eq!(compute_expand(None, false), 0);
    }

    /// Captured symbol searches must not opt into expansion without `--full`.
    #[test]
    fn piped_invocation_does_not_auto_expand() {
        // Simulating: user ran `tilth foo` (no --full) but stdout is piped.
        // Output destination does not alter cli.search.full or the expansion count.
        let cli_full = false; // user did NOT pass --full
        assert_eq!(compute_expand(None, cli_full), 0);
    }

    #[test]
    fn search_subcommand_accepts_query_options() {
        let cli = Cli::try_parse_from([
            "tilth",
            "search",
            "target",
            "--scope",
            "src",
            "--scope",
            "tests",
            "--full",
            "--expand=0",
            "--json",
            "--glob",
            "*.rs",
            "--budget",
            "100",
            "--respect-gitignore",
        ])
        .expect("parse search subcommand");
        let Some(Command::Search { query, options }) = cli.command else {
            panic!("expected search subcommand");
        };
        assert_eq!(query.as_deref(), Some("target"));
        assert_eq!(
            options.scope,
            vec![PathBuf::from("src"), PathBuf::from("tests")]
        );
        assert!(options.full && options.json && options.respect_gitignore);
        assert_eq!(options.expand, Some(0));
        assert_eq!(options.glob.as_deref(), Some("*.rs"));
        assert_eq!(options.budget, Some(100));
    }

    #[test]
    fn search_preserves_options_before_verb() {
        let mut cli = Cli::try_parse_from([
            "tilth",
            "--json",
            "--scope",
            "src",
            "--full",
            "--expand=1",
            "--glob",
            "*.rs",
            "--budget",
            "100",
            "--respect-gitignore",
            "--section",
            "1-8",
            "search",
            "main.rs",
            "--scope",
            "tests",
            "--budget",
            "200",
        ])
        .unwrap();
        let Some(Command::Search { options, .. }) = cli.command.take() else {
            panic!("expected search subcommand");
        };
        cli.search.merge(options);
        assert!(cli.search.json && cli.search.full && cli.search.respect_gitignore);
        assert_eq!(
            cli.search.scope,
            vec![PathBuf::from("src"), PathBuf::from("tests")]
        );
        assert_eq!(cli.search.budget, Some(200));
        assert_eq!(cli.search.expand, Some(1));
        assert_eq!(cli.search.glob.as_deref(), Some("*.rs"));
        assert_eq!(cli.search.section.as_deref(), Some("1-8"));
        assert!(cli.search.validate(false).is_ok());
    }

    #[test]
    fn search_rejects_conflicts_across_verb() {
        for flags in [
            ["--callers", "--deps"],
            ["--map", "--full"],
            ["--deps", "--map"],
        ] {
            let mut cli =
                Cli::try_parse_from(["tilth", flags[0], "search", "target", flags[1]]).unwrap();
            let Some(Command::Search { options, .. }) = cli.command.take() else {
                panic!("expected search subcommand");
            };
            cli.search.merge(options);
            assert!(cli.search.validate(false).is_err());
        }
    }

    #[test]
    fn search_subcommand_requires_query() {
        assert!(Cli::try_parse_from(["tilth", "search"]).is_err());
    }

    #[test]
    fn repeated_scope_flags_accumulate() {
        let cli = Cli::try_parse_from(["tilth", "foo", "--scope", "src", "--scope", "tests"])
            .expect("parse repeated scopes");
        assert_eq!(
            cli.search.scope,
            vec![PathBuf::from("src"), PathBuf::from("tests")]
        );
    }

    #[test]
    fn relative_query_path_resolves_per_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("foo.rs"), "fn first() {}\n").unwrap();
        std::fs::write(second.join("foo.rs"), "fn second() {}\n").unwrap();

        assert_eq!(resolve_query_path("foo.rs", &first), first.join("foo.rs"));
        assert_eq!(resolve_query_path("foo.rs", &second), second.join("foo.rs"));
    }

    #[test]
    fn multi_scope_query_reapplies_budget_after_joining() {
        let scopes = vec![PathBuf::from("one"), PathBuf::from("two")];
        let output = run_query_for_scopes(&scopes, "target", Some(80), |_| {
            Ok(format!("# Search\n\n{}", "result line\n".repeat(200)))
        })
        .unwrap();

        assert!(
            output.contains("... truncated"),
            "joined output was not capped: {output}"
        );
    }

    #[test]
    fn multi_scope_query_reports_partial_failure() {
        let scopes = vec![PathBuf::from("valid"), PathBuf::from("missing")];
        let output = run_query_for_scopes(&scopes, "target", None, |scope| {
            if scope == Path::new("valid") {
                Ok("found target".to_string())
            } else {
                Err(tilth::error::TilthError::NotFound {
                    path: scope.to_path_buf(),
                    suggestion: None,
                })
            }
        })
        .unwrap();
        assert!(output.contains("found target"), "{output}");
        assert!(output.contains("## Scope errors\n- missing:"), "{output}");
        assert!(output.contains("missing"), "{output}");
    }
    #[test]
    fn partial_failure_survives_small_budget() {
        let scopes = vec![PathBuf::from("valid"), PathBuf::from("missing")];
        let output = run_query_for_scopes(&scopes, "target", Some(90), |scope| {
            if scope == Path::new("valid") {
                Ok("found target\n".repeat(400))
            } else {
                Err(tilth::error::TilthError::NotFound {
                    path: scope.to_path_buf(),
                    suggestion: None,
                })
            }
        })
        .unwrap();
        assert!(
            output.contains("## Scope errors\n- missing: not found: missing"),
            "{output}"
        );
        assert!(output.len() <= 90 * 4, "budget exceeded: {}", output.len());
    }

    #[test]
    fn all_failed_scopes_return_each_error() {
        let scopes = vec![PathBuf::from("first"), PathBuf::from("second")];
        let error = run_query_for_scopes(&scopes, "target", None, |scope| {
            Err(tilth::error::TilthError::NotFound {
                path: scope.to_path_buf(),
                suggestion: None,
            })
        })
        .unwrap_err();
        match error {
            tilth::error::TilthError::ScopedFailures { failures } => {
                assert_eq!(failures.len(), 2);
                assert_eq!(failures[0].scope, scopes[0]);
                assert_eq!(failures[1].scope, scopes[1]);
            }
            other => panic!("expected scoped failures, got {other}"),
        }
    }

    /// #195: `--scope <file>` gets the same refusal as the MCP tools. It used
    /// to walk the lone file and print match headers with no file name.
    #[test]
    fn cli_scope_refuses_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.py");
        std::fs::write(&file, "").unwrap();
        let err = cli_scope(file, FileScopeHint::Glob).unwrap_err();
        assert!(
            err.contains("is a file") && err.contains("glob to its name (\"x.py\")"),
            "unexpected refusal: {err}"
        );
        let ok = cli_scope(dir.path().to_path_buf(), FileScopeHint::Glob).unwrap();
        assert_eq!(ok, dir.path().canonicalize().unwrap());
    }
}
