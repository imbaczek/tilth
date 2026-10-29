//! Stateless bounded traces over a static, name-resolved call graph.
mod extract;
mod lisp;
mod typescript;
use crate::{
    cache::OutlineCache,
    lang::declarations::Declaration,
    types::{FileType, Lang, OutlineKind},
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

const MAX_FILES: usize = 2000;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_NODES: usize = 30000;
pub(super) const MAX_EDGES: usize = 50000;

#[derive(Clone, Copy)]
pub(crate) struct Request<'a> {
    pub from: &'a str,
    pub to: Option<&'a str>,
    pub direction: &'a str,
    pub depth: usize,
    pub budget: Option<u64>,
}
struct Edge {
    from: usize,
    to: usize,
    line: u32,
    concerns: Vec<&'static str>,
}
#[derive(Default, Clone)]
struct Issues {
    counts: BTreeMap<String, usize>,
    examples: BTreeMap<String, BTreeSet<String>>,
}
impl Issues {
    fn add(&mut self, reason: impl Into<String>) {
        *self.counts.entry(reason.into()).or_default() += 1;
    }
    fn example(&mut self, reason: &str, example: impl Into<String>) {
        self.add(reason);
        let examples = self.examples.entry(reason.into()).or_default();
        if examples.len() < 3 {
            examples.insert(example.into());
        }
    }
    fn merge(&mut self, other: &Self) {
        for (reason, count) in &other.counts {
            *self.counts.entry(reason.clone()).or_default() += count;
        }
        for (reason, examples) in &other.examples {
            let ours = self.examples.entry(reason.clone()).or_default();
            for example in examples {
                if ours.len() < 3 {
                    ours.insert(example.clone());
                }
            }
        }
    }
    fn render(&self) -> String {
        if self.counts.is_empty() {
            return String::new();
        }
        let mut out = String::from("> Results may be incomplete or contain heuristic bindings:\n");
        let mut reasons: Vec<_> = self.counts.iter().collect();
        reasons.sort_by_key(|(reason, _)| {
            (
                !reason.contains("limit"),
                !reason.starts_with("unresolved calls"),
                reason.as_str(),
            )
        });
        for (reason, count) in reasons.into_iter().take(10) {
            let _ = write!(out, "> - {reason} ({count})");
            if let Some(examples) = self.examples.get(reason) {
                let _ = write!(
                    out,
                    "; e.g. {}",
                    examples.iter().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            out.push('\n');
        }
        if self.counts.len() > 10 {
            let _ = writeln!(
                out,
                "> - {} additional limitation categories",
                self.counts.len() - 10
            );
        }
        out
    }
}
#[derive(Default)]
struct Graph {
    nodes: Vec<Declaration>,
    ts_sources: HashMap<std::path::PathBuf, std::sync::Arc<crate::cache::ParsedFile>>,
    edges: Vec<Edge>,
    issues: Issues,
    node_issues: HashMap<usize, Issues>,
    language_issues: HashMap<Lang, Issues>,
}

fn nested_rust_context(
    parsed: &crate::cache::ParsedFile,
    byte: usize,
    owner: (usize, usize),
) -> bool {
    let mut node = parsed
        .tree
        .root_node()
        .descendant_for_byte_range(byte, byte + 1);
    while let Some(current) = node {
        if matches!(
            current.kind(),
            "closure_expression" | "async_block" | "const_item" | "static_item"
        ) {
            return true;
        }
        if current.kind() == "function_item" {
            return (current.start_byte(), current.end_byte()) != owner;
        }
        node = current.parent();
    }
    false
}
fn rust_context(node: &Declaration) -> (Vec<String>, Option<String>) {
    let (start, end) = node.span.unwrap();
    let mut current = node
        .parsed
        .tree
        .root_node()
        .descendant_for_byte_range(start, end);
    let mut modules = Vec::new();
    let mut ty = None;
    while let Some(ancestor) = current {
        if ancestor.kind() == "mod_item" {
            if let Some(name) = ancestor.child_by_field_name("name") {
                modules.push(
                    name.utf8_text(node.parsed.content.as_bytes())
                        .unwrap_or("")
                        .to_owned(),
                );
            }
        }
        if ancestor.kind() == "impl_item" {
            ty = ancestor
                .child_by_field_name("type")
                .and_then(|name| name.utf8_text(node.parsed.content.as_bytes()).ok())
                .map(str::to_owned);
        }
        current = ancestor.parent();
    }
    modules.reverse();
    (modules, ty)
}
fn rust_call_name(node: &Declaration, name: &str) -> String {
    let (mut modules, ty) = rust_context(node);
    if let Some(name) = name.strip_prefix("crate::") {
        return name.to_owned();
    }
    if let Some(name) = name.strip_prefix("Self::") {
        if let Some(ty) = ty {
            modules.push(ty);
        }
        modules.push(name.to_owned());
        return modules.join("::");
    }
    let mut tail = name;
    if let Some(name) = tail.strip_prefix("self::") {
        tail = name;
    } else {
        while let Some(name) = tail.strip_prefix("super::") {
            modules.pop();
            tail = name;
        }
    }
    modules.push(tail.to_owned());
    modules.join("::")
}

fn key(lang: Lang, name: &str) -> String {
    if lang == Lang::CommonLisp {
        crate::lang::commonlisp::symbol_key(name)
    } else {
        name.to_owned()
    }
}

fn build(scope: &Path, cache: &OutlineCache, preferred: &str) -> Result<Graph, String> {
    build_limited(scope, cache, preferred, MAX_NODES, MAX_EDGES)
}
struct SourceChunk {
    first: usize,
    last: usize,
    parsed: std::sync::Arc<crate::cache::ParsedFile>,
}
struct SourceIndex {
    chunks: Vec<SourceChunk>,
    cfg_files: HashSet<std::path::PathBuf>,
}
struct PendingCall {
    owner: usize,
    call: extract::Call,
}

fn build_limited(
    scope: &Path,
    cache: &OutlineCache,
    preferred: &str,
    node_limit: usize,
    site_limit: usize,
) -> Result<Graph, String> {
    let mut graph = Graph::default();
    let paths = collect_source_paths(scope, &mut graph.issues)?;
    let SourceIndex { chunks, cfg_files } = index_sources(paths, cache, &mut graph, node_limit);
    let pending = collect_calls(chunks, &mut graph, scope, preferred, site_limit);
    resolve_calls(&mut graph, pending, &cfg_files);
    Ok(graph)
}

fn collect_source_paths(
    scope: &Path,
    issues: &mut Issues,
) -> Result<Vec<std::path::PathBuf>, String> {
    std::fs::read_dir(scope).map_err(|e| format!("{}: {e}", scope.display()))?;
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    let mut builder = crate::search::base_walk_builder(scope);
    builder.sort_by_file_path(std::cmp::Ord::cmp);
    for (visited, entry) in builder.build().enumerate() {
        if visited == 100_000 {
            issues.add("directory-entry work limit reached (100000 entries)");
            break;
        }
        let Ok(entry) = entry else {
            issues.add("directory entries could not be read");
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if !matches!(
            crate::lang::detect_file_type(entry.path()),
            FileType::Code(_)
        ) {
            continue;
        }
        let path = entry
            .path()
            .canonicalize()
            .unwrap_or_else(|_| entry.path().to_owned());
        if !seen.insert(path.clone()) {
            continue;
        }
        paths.push(path);
    }
    // Walk the bounded directory set before applying the parsing cap. Taking a
    // lexical prefix lets a large language corpus exclude every other language.
    paths.sort();
    let total = paths.len();
    let mut languages: Vec<(Lang, VecDeque<std::path::PathBuf>)> = Vec::new();
    for path in paths {
        let FileType::Code(lang) = crate::lang::detect_file_type(&path) else {
            continue;
        };
        if let Some((_, files)) = languages.iter_mut().find(|(kind, _)| *kind == lang) {
            files.push_back(path);
        } else {
            languages.push((lang, VecDeque::from([path])));
        }
    }
    let mut paths = Vec::new();
    loop {
        let mut added = false;
        for (_, files) in &mut languages {
            if let Some(path) = files.pop_front() {
                paths.push(path);
                added = true;
                if paths.len() == MAX_FILES {
                    if total > MAX_FILES {
                        issues.add("source-file work limit reached (2000 files)");
                    }
                    return Ok(paths);
                }
            }
        }
        if !added {
            break;
        }
    }
    Ok(paths)
}

fn index_sources(
    paths: Vec<std::path::PathBuf>,
    cache: &OutlineCache,
    graph: &mut Graph,
    node_limit: usize,
) -> SourceIndex {
    let mut bytes = 0u64;
    let mut chunks = Vec::new();
    let mut cfg_files = HashSet::new();
    for path in paths {
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        if bytes.saturating_add(size) > MAX_BYTES {
            graph.issues.add("source-byte work limit reached (64 MiB)");
            break;
        }
        bytes += size;
        let Some(parsed) = cache.get_or_parse(&path) else {
            if let FileType::Code(lang) = crate::lang::detect_file_type(&path) {
                graph
                    .language_issues
                    .entry(lang)
                    .or_default()
                    .add("source files unreadable, unsupported, or larger than 1 MB");
            }
            continue;
        };
        if parsed.tree.root_node().has_error() {
            graph
                .language_issues
                .entry(parsed.lang)
                .or_default()
                .add("source files have parse errors");
        }
        if extract::has_rust_cfg(&parsed) {
            cfg_files.insert(path.clone());
        }
        if typescript::supported(parsed.lang) {
            graph.ts_sources.insert(path.clone(), parsed.clone());
        }
        let first = graph.nodes.len();
        for declaration in crate::lang::declarations::collect(&path, &parsed)
            .into_iter()
            .filter(|d| d.entry.kind == OutlineKind::Function)
        {
            if graph.nodes.len() == node_limit {
                graph.issues.add(format!(
                    "declaration work limit reached ({node_limit} nodes)"
                ));
                break;
            }
            if declaration.span.is_none() {
                graph
                    .issues
                    .add("declarations without exact source spans were skipped");
                continue;
            }
            graph.nodes.push(declaration);
        }
        if graph.nodes.len() == node_limit {
            graph.issues.add(format!(
                "declaration work limit reached ({node_limit} nodes)"
            ));
            break;
        }
        chunks.push(SourceChunk {
            first,
            last: graph.nodes.len(),
            parsed,
        });
    }

    SourceIndex { chunks, cfg_files }
}

fn collect_calls(
    mut chunks: Vec<SourceChunk>,
    graph: &mut Graph,
    scope: &Path,
    preferred: &str,
    site_limit: usize,
) -> Vec<PendingCall> {
    let mut pending = Vec::new();
    let preferred_ids = resolve(graph, preferred, scope);
    chunks.sort_by_key(|SourceChunk { first, last, .. }| {
        !preferred_ids.iter().any(|id| *first <= *id && *id < *last)
    });
    for SourceChunk {
        first,
        last,
        parsed,
    } in chunks
    {
        let (mut calls, problems) = extract::calls(&parsed);
        for problem in problems {
            if problem.contains("limit") {
                graph.issues.add(problem);
            } else {
                graph
                    .language_issues
                    .entry(parsed.lang)
                    .or_default()
                    .add(problem);
            }
        }
        calls.sort_by_key(|call| call.byte);
        let mut owners: Vec<_> = (first..last).collect();
        owners.sort_by_key(|id| {
            let (start, end) = graph.nodes[*id].span.unwrap();
            (start, std::cmp::Reverse(end))
        });
        let mut owner_cursor = 0;
        let mut active = Vec::new();
        for call in calls {
            while owner_cursor < owners.len()
                && graph.nodes[owners[owner_cursor]].span.unwrap().0 <= call.byte
            {
                active.push(owners[owner_cursor]);
                owner_cursor += 1;
            }
            active.retain(|id| graph.nodes[*id].span.unwrap().1 > call.byte);
            let Some(owner) = active
                .iter()
                .min_by_key(|id| {
                    let (a, b) = graph.nodes[**id].span.unwrap();
                    b - a
                })
                .copied()
            else {
                continue;
            };
            if parsed.lang == Lang::Rust
                && nested_rust_context(&parsed, call.byte, graph.nodes[owner].span.unwrap())
            {
                graph
                    .node_issues
                    .entry(owner)
                    .or_default()
                    .add("deferred context or unindexed nested function body omitted");
                continue;
            }
            if pending.len() == site_limit {
                graph
                    .issues
                    .add(format!("call-site work limit reached ({site_limit} sites)"));
                break;
            }
            pending.push(PendingCall { owner, call });
        }
        if pending.len() == site_limit {
            graph
                .issues
                .add(format!("call-site work limit reached ({site_limit} sites)"));
            break;
        }
    }

    pending
}

fn resolve_calls(
    graph: &mut Graph,
    pending: Vec<PendingCall>,
    cfg_files: &HashSet<std::path::PathBuf>,
) {
    let mut names: HashMap<(Lang, String), Vec<usize>> = HashMap::new();
    for (id, node) in graph.nodes.iter().enumerate() {
        names
            .entry((node.parsed.lang, key(node.parsed.lang, &node.entry.name)))
            .or_default()
            .push(id);
        if node.qualified != node.entry.name {
            names
                .entry((node.parsed.lang, key(node.parsed.lang, &node.qualified)))
                .or_default()
                .push(id);
        }
    }
    let ts_index = typescript::Index::new(&graph.ts_sources, &graph.nodes);
    let mut seen_edges = HashSet::new();
    for PendingCall { owner, call } in pending {
        let node = &graph.nodes[owner];
        let issues = graph.node_issues.entry(owner).or_default();
        for reason in &call.concerns {
            if typescript::supported(node.parsed.lang)
                && call.receiver.is_some()
                && *reason == "receiver dispatch is resolved by name only"
            {
                continue;
            }
            issues.add(*reason);
        }
        if typescript::supported(node.parsed.lang) && call.receiver.is_some() {
            issues.add(
                "receiver binding is inferred syntactically; runtime overrides are not evaluated",
            );
        }
        if call.concerns.iter().any(|reason| {
            matches!(
                *reason,
                "lexically bound Lisp function is not resolved"
                    | "lexically bound Rust callable is not resolved"
                    | "Rust macro invocation cannot resolve to a function declaration"
                    | "unsupported call expression is not resolved"
            )
        }) {
            continue;
        }
        let normalized = if node.parsed.lang == Lang::Rust {
            rust_call_name(node, &call.name)
        } else {
            call.name.clone()
        };
        let candidates = names
            .get(&(node.parsed.lang, key(node.parsed.lang, &normalized)))
            .map_or(&[][..], Vec::as_slice);
        let filtered;
        let receiver = call
            .concerns
            .contains(&"receiver dispatch is resolved by name only");
        let candidates = if typescript::supported(node.parsed.lang) && call.receiver.is_some() {
            match typescript::resolve_member(&graph.ts_sources, &ts_index, node, &call) {
                Ok(targets) => {
                    filtered = targets;
                    filtered.as_slice()
                }
                Err(reason) => {
                    issues.example(
                        reason,
                        format!("{}.{}", call.receiver.as_deref().unwrap_or("?"), call.name),
                    );
                    continue;
                }
            }
        } else if node.parsed.lang == Lang::Rust && !call.name.contains("::") && !receiver {
            let (module, _) = rust_context(node);
            filtered = candidates
                .iter()
                .copied()
                .filter(|id| {
                    let (target_module, ty) = rust_context(&graph.nodes[*id]);
                    target_module == module && ty.is_none()
                })
                .collect::<Vec<_>>();
            filtered.as_slice()
        } else {
            candidates
        };
        match candidates {
            [target] => {
                let mut concerns = call.concerns.clone();
                if typescript::supported(node.parsed.lang) && call.receiver.is_some() {
                    concerns
                        .retain(|reason| *reason != "receiver dispatch is resolved by name only");
                    concerns.push("receiver binding is inferred syntactically; runtime overrides are not evaluated");
                }
                if cfg_files.contains(&node.path) || cfg_files.contains(&graph.nodes[*target].path)
                {
                    concerns
                        .push("Rust conditional compilation is not evaluated on the selected path");
                }
                if node.parsed.tree.root_node().has_error()
                    || graph.nodes[*target].parsed.tree.root_node().has_error()
                {
                    concerns.push("parse errors affect a path declaration");
                }
                if node.parsed.lang == Lang::Rust
                    && !call.name.contains("::")
                    && !receiver
                    && node.path == graph.nodes[*target].path
                {
                    let (source_namespace, _) = rust_context(node);
                    let (target_namespace, target_type) = rust_context(&graph.nodes[*target]);
                    if source_namespace != target_namespace || target_type.is_some() {
                        issues.add("Rust namespace binding is unresolved; incompatible bare-name edge omitted");
                        continue;
                    }
                }
                if node.path != graph.nodes[*target].path {
                    issues.add("cross-file bindings are resolved heuristically by name");
                    concerns.push("cross-file bindings are resolved heuristically by name");
                }
                let source = graph.nodes[*target]
                    .entry
                    .signature
                    .as_deref()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if source.starts_with("(defmacro") {
                    issues.add("Lisp macro expansion may add or change calls");
                    concerns.push("Lisp macro expansion may add or change calls");
                }
                if source.starts_with("(defmethod") || source.starts_with("(defgeneric") {
                    issues.add("Lisp generic dispatch and method combination are unresolved");
                    concerns.push("Lisp generic dispatch and method combination are unresolved");
                }
                if seen_edges.insert((owner, *target, call.line)) {
                    graph.edges.push(Edge {
                        from: owner,
                        to: *target,
                        line: call.line,
                        concerns,
                    });
                }
            }
            [] if node.parsed.lang == Lang::Rust
                && !call.name.contains("::")
                && names.contains_key(&(node.parsed.lang, key(node.parsed.lang, &call.name))) =>
            {
                issues.add(
                    "Rust namespace binding is unresolved; incompatible bare-name edge omitted",
                );
            }
            [] if node.parsed.lang == Lang::Rust && call.name.contains("::") => issues.example(
                "Rust module paths or import aliases are unresolved",
                call.name,
            ),
            [] => issues.example(
                "unresolved calls (external, dynamic, or generated)",
                crate::types::truncate_str(&call.name, 60).to_owned(),
            ),
            _ => issues.example(
                "ambiguous call targets",
                format!(
                    "{} ({} declarations)",
                    crate::types::truncate_str(&call.name, 60),
                    candidates.len()
                ),
            ),
        }
    }
    graph
        .edges
        .sort_by_key(|edge| (edge.from, edge.line, edge.to));
}

fn resolve(graph: &Graph, target: &str, scope: &Path) -> Vec<usize> {
    let (name, line) = target
        .rsplit_once('@')
        .and_then(|(name, line)| line.parse::<u32>().ok().map(|line| (name, Some(line))))
        .unwrap_or((target, None));
    graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            let eq = |a: &str, b: &str| key(node.parsed.lang, a) == key(node.parsed.lang, b);
            let location = format!(
                "{}:{}",
                node.path
                    .strip_prefix(scope)
                    .unwrap_or(&node.path)
                    .display(),
                node.entry.start_line
            );
            (eq(name, &node.entry.name) || eq(name, &node.qualified) || target == location)
                && line.is_none_or(|line| node.entry.start_line == line)
        })
        .map(|(id, _)| id)
        .collect()
}
fn label(graph: &Graph, id: usize, scope: &Path) -> String {
    let node = &graph.nodes[id];
    format!(
        "{}  {}:{}",
        node.qualified,
        node.path
            .strip_prefix(scope)
            .unwrap_or(&node.path)
            .display(),
        node.entry.start_line
    )
}
fn endpoint(graph: &Graph, target: &str, scope: &Path) -> Result<usize, String> {
    let ids = resolve(graph, target, scope);
    match ids.as_slice(){
        [id]=>Ok(*id),
        []=>Err(format!("{target:?}: no indexed function declaration; generated or unindexed definitions may be missing")),
        _=>Err(format!("{target:?}: ambiguous ({} declarations); narrow scope or use qualified name/name@line/file:line\n{}",ids.len(),ids.iter().take(10).map(|id|format!("  {}",label(graph,*id,scope))).collect::<Vec<_>>().join("\n"))),
    }
}

pub(crate) fn run(
    scope: &Path,
    request: Request<'_>,
    cache: &OutlineCache,
) -> Result<String, String> {
    if request.from.trim().is_empty() || request.to.is_some_and(|s| s.trim().is_empty()) {
        return Err("trace endpoints must not be empty".into());
    }
    if !matches!(request.direction, "callers" | "callees") {
        return Err("direction must be callers or callees".into());
    }
    if !(1..=16).contains(&request.depth) {
        return Err("depth must be between 1 and 16".into());
    }
    let scope = scope.canonicalize().map_err(|e| e.to_string())?;
    let graph = build(&scope, cache, request.from)?;
    render_graph(graph, &scope, request)
}

struct WalkResult {
    selected: Vec<usize>,
    expanded: HashSet<usize>,
    found: bool,
}
fn walk_graph(
    graph: &Graph,
    start: usize,
    target: Option<usize>,
    reverse: bool,
    depth_limit: usize,
) -> WalkResult {
    let mut adjacency: HashMap<usize, Vec<usize>> = HashMap::new();
    for (id, edge) in graph.edges.iter().enumerate() {
        adjacency
            .entry(if reverse { edge.to } else { edge.from })
            .or_default()
            .push(id);
    }
    let mut queue = VecDeque::from([(start, 0)]);
    let mut visited = HashSet::from([start]);
    let mut parents = HashMap::new();
    let mut selected = Vec::new();
    let mut expanded = HashSet::new();
    let mut found = target == Some(start);
    while let Some((node, depth)) = queue.pop_front() {
        if found || depth == depth_limit {
            continue;
        }
        expanded.insert(node);
        for id in adjacency.get(&node).into_iter().flatten() {
            let edge = &graph.edges[*id];
            let next = if reverse { edge.from } else { edge.to };
            selected.push(*id);
            if visited.insert(next) {
                parents.insert(next, (node, *id));
                queue.push_back((next, depth + 1));
            }
            if target == Some(next) {
                found = true;
                break;
            }
        }
    }
    if let Some(target) = target {
        if found {
            selected.clear();
            visited.clear();
            let mut node = target;
            visited.insert(node);
            while node != start {
                let (parent, edge) = parents[&node];
                selected.push(edge);
                node = parent;
                visited.insert(node);
            }
            selected.reverse();
        }
    }
    WalkResult {
        selected,
        expanded,
        found,
    }
}

fn render_graph(mut graph: Graph, scope: &Path, request: Request<'_>) -> Result<String, String> {
    let global_issues = graph.issues.clone();
    let start_ids = resolve(&graph, request.from, scope);
    let relevant = start_ids
        .first()
        .filter(|_| start_ids.len() == 1)
        .map(|id| graph.nodes[*id].parsed.lang);
    for (lang, issues) in &graph.language_issues {
        if relevant.is_none_or(|relevant| relevant == *lang) {
            graph.issues.merge(issues);
        }
    }
    let mut header = format!(
        "# Trace: {:?} [{}; depth {}]\nStatic syntactic calls, resolved by name within {}.\n",
        request.from,
        request.direction,
        request.depth,
        scope.display()
    );
    let start = match endpoint(&graph, request.from, scope) {
        Ok(id) => id,
        Err(error) => {
            return Ok(finish(
                &format!("{header}\n{error}\n{}", graph.issues.render()),
                request.budget,
            ))
        }
    };
    let target = match request
        .to
        .map(|target| endpoint(&graph, target, scope))
        .transpose()
    {
        Ok(target) => target,
        Err(error) => {
            return Ok(finish(
                &format!("{header}\n{error}\n{}", graph.issues.render()),
                request.budget,
            ))
        }
    };
    let reverse = request.direction == "callers";
    let WalkResult {
        selected,
        expanded,
        found,
    } = walk_graph(&graph, start, target, reverse, request.depth);
    if target.is_some() {
        if found {
            header.push_str("Path found in the static name graph.\n");
        } else {
            let _ = writeln!(
                header,
                "No path found within depth {}. This does not establish unreachability.",
                request.depth
            );
        }
    }
    if target.is_some() && found {
        let local_witness = graph.nodes[start].parsed.lang == Lang::Rust
            && !graph.nodes[start].parsed.tree.root_node().has_error()
            && selected.iter().all(|id| {
                let edge = &graph.edges[*id];
                edge.concerns.is_empty() && graph.nodes[edge.from].path == graph.nodes[edge.to].path
            });
        if local_witness {
            graph.issues = global_issues;
        }
        for id in &selected {
            for concern in &graph.edges[*id].concerns {
                graph.issues.add(*concern);
            }
        }
    } else {
        for (owner, issues) in &graph.node_issues {
            if (reverse && graph.nodes[*owner].parsed.lang == graph.nodes[start].parsed.lang)
                || expanded.contains(owner)
            {
                graph.issues.merge(issues);
            }
        }
    }
    header.push_str(&graph.issues.render());
    let mut body = format!("\nStart: {}\n", label(&graph, start, scope));
    if selected.is_empty() {
        body.push_str("No indexed direct edges in this result.\n");
        if !reverse && target.is_none() {
            body.push_str(
                "Callees require definitions in scope. To find uses of this function, use --direction callers.\n",
            );
        }
    }
    for id in selected {
        let edge = &graph.edges[id];
        let site = graph.nodes[edge.from]
            .path
            .strip_prefix(scope)
            .unwrap_or(&graph.nodes[edge.from].path);
        let _ = writeln!(
            body,
            "{} -> {}\n  call site {}:{} [by-name]",
            label(&graph, edge.from, scope),
            label(&graph, edge.to, scope),
            site.display(),
            edge.line
        );
    }
    Ok(finish(&format!("{header}{body}"), request.budget))
}
fn finish(output: &str, budget: Option<u64>) -> String {
    let budget = budget.unwrap_or(crate::budget::DEFAULT_BUDGET);
    if crate::types::estimate_tokens(output.len() as u64) <= budget {
        return output.into();
    }
    crate::budget::apply(
        &format!("> Output incomplete: budget omitted trace details; increase budget.\n{output}"),
        budget,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_line_module_functions_keep_call_site_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "mod a { fn start() { self::end(); } fn end() {} }\n").unwrap();
        let cache = OutlineCache::new();
        let graph = build(dir.path(), &cache, "a::start").unwrap();
        let (calls, _) = extract::calls(&cache.get_or_parse(&path).unwrap());
        assert_eq!(
            graph.edges.len(),
            1,
            "nodes {:?}; calls {:?}; edges {:?}; issues {}",
            graph
                .nodes
                .iter()
                .map(|n| (&n.qualified, n.span, rust_context(n)))
                .collect::<Vec<_>>(),
            calls.iter().map(|c| (&c.name, c.byte)).collect::<Vec<_>>(),
            graph
                .edges
                .iter()
                .map(|e| (e.from, e.to))
                .collect::<Vec<_>>(),
            graph.issues.render()
        );
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;
    #[test]
    fn exact_boundary_stops_always_report_limits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn start() { end(); } fn end() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn other() { end(); }\n").unwrap();
        let graph = build_limited(dir.path(), &OutlineCache::new(), "start", 2, MAX_EDGES).unwrap();
        assert!(graph.issues.render().contains("declaration work limit"));
        let graph = build_limited(dir.path(), &OutlineCache::new(), "start", MAX_NODES, 1).unwrap();
        assert!(graph.issues.render().contains("call-site work limit"));
    }
    #[test]
    fn caps_survive_many_distinct_unresolved_calls() {
        let mut issues = Issues::default();
        for i in 0..100 {
            issues.example("unresolved calls", format!("name{i}"));
        }
        issues.add("source-byte work limit reached");
        let out = issues.render();
        assert!(out.contains("source-byte work limit"));
        assert_eq!(issues.examples["unresolved calls"].len(), 3);
    }
}

#[cfg(test)]
mod witness_tests {
    use super::*;
    #[test]
    fn positive_local_witness_retains_hard_work_limits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn start() { end(); } fn end() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn other() { end(); }\n").unwrap();
        let graph = build_limited(dir.path(), &OutlineCache::new(), "start", MAX_NODES, 1).unwrap();
        let out = render_graph(
            graph,
            dir.path(),
            Request {
                from: "start",
                to: Some("end"),
                direction: "callees",
                depth: 2,
                budget: None,
            },
        )
        .unwrap();
        assert!(
            out.contains("Path found") && out.contains("call-site work limit"),
            "{out}"
        );
    }
}
