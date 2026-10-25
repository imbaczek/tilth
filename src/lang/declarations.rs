//! Declaration identities and exact AST spans shared by symbol reads and tracing.
use crate::{
    cache::ParsedFile,
    types::{Lang, OutlineEntry},
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) struct Declaration {
    pub path: PathBuf,
    pub qualified: String,
    pub entry: OutlineEntry,
    pub parsed: std::sync::Arc<ParsedFile>,
    pub span: Option<(usize, usize)>,
}

fn flatten(
    entries: Vec<OutlineEntry>,
    parents: &[String],
    path: &Path,
    parsed: &std::sync::Arc<ParsedFile>,
    out: &mut Vec<Declaration>,
    spans: &mut SpanIndex,
) {
    for mut entry in entries {
        let mut names = parents.to_vec();
        names.push(entry.name.trim_start_matches("impl ").to_owned());
        let children = std::mem::take(&mut entry.children);
        let span = claim_span(&entry, spans);
        out.push(Declaration {
            path: path.to_owned(),
            qualified: names.join("::"),
            entry,
            parsed: parsed.clone(),
            span,
        });
        flatten(children, &names, path, parsed, out, spans);
    }
}

struct SourceSpan {
    name: Option<String>,
    grouped_go: bool,
    bytes: (usize, usize),
}
type SpanIndex = HashMap<(u32, u32), Vec<SourceSpan>>;

fn index_spans(parsed: &ParsedFile) -> SpanIndex {
    let lines: Vec<_> = parsed.content.lines().collect();
    let mut spans = SpanIndex::new();
    for node in crate::lang::walk::named_descendants(parsed.tree.root_node()) {
        let spec = crate::lang::spec::spec(parsed.lang);
        let name = if spec.definition_kinds.contains(&node.kind()) {
            (spec.definitions.extract_name)(node, &lines)
        } else {
            None
        };
        let grouped_go = parsed.lang == Lang::Go
            && matches!(node.kind(), "var_spec" | "const_spec" | "type_spec");
        if name.is_some() || grouped_go {
            let source_node = if grouped_go {
                node.parent()
                    .filter(|parent| {
                        matches!(
                            parent.kind(),
                            "var_declaration" | "const_declaration" | "type_declaration"
                        ) && parent.start_position().row == node.start_position().row
                            && parent.end_position().row == node.end_position().row
                    })
                    .unwrap_or(node)
            } else {
                node
            };
            spans
                .entry((
                    node.start_position().row as u32 + 1,
                    node.end_position().row as u32 + 1,
                ))
                .or_default()
                .push(SourceSpan {
                    name,
                    grouped_go,
                    bytes: (source_node.start_byte(), source_node.end_byte()),
                });
        }
    }
    spans
}

fn claim_span(entry: &OutlineEntry, spans: &mut SpanIndex) -> Option<(usize, usize)> {
    let candidates = spans.get_mut(&(entry.start_line, entry.end_line))?;
    let index = candidates.iter().position(|candidate| {
        candidate.name.as_deref() == Some(entry.name.as_str()) || candidate.grouped_go
    })?;
    Some(candidates.remove(index).bytes)
}

pub(crate) fn exact_body(d: &Declaration) -> Option<String> {
    let (start, end) = d.span?;
    d.parsed
        .content
        .get(start..end)
        .map(|body| format!("```\n{body}\n```"))
}

pub(crate) fn collect(path: &Path, parsed: &std::sync::Arc<ParsedFile>) -> Vec<Declaration> {
    let entries = crate::lang::outline::get_outline_entries(&parsed.content, parsed.lang);
    let mut out = Vec::new();
    flatten(
        entries,
        &[],
        path,
        parsed,
        &mut out,
        &mut index_spans(parsed),
    );
    out
}

/// Full source header, unlike the abbreviated label used by structural outlines.
pub(crate) fn signature_text(declaration: &Declaration) -> Option<String> {
    let (start, source_end) = declaration.span?;
    let end = declaration.entry.signature_end?;
    let line_start = declaration
        .parsed
        .content
        .split_inclusive('\n')
        .take(end.line.saturating_sub(1) as usize)
        .map(str::len)
        .sum::<usize>();
    let end = line_start.checked_add(end.column)?;
    if end < start || end > source_end {
        return None;
    }
    declaration
        .parsed
        .content
        .get(start..end)
        .map(|text| text.trim_end().to_owned())
}
