//! Independently paged Grok relationships; no body expansion or dedup side effects.
use super::{display_rel, grok_collect, kind_label, GrokCaps, GrokResult};
use crate::error::TilthError;
use crate::index::bloom::BloomFilterCache;
use crate::listing::Page;
use crate::session::Session;
use std::fmt::Write as _;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Section {
    Callers,
    Callees,
    Siblings,
    Tests,
}

impl Section {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "callers" => Ok(Self::Callers),
            "callees" => Ok(Self::Callees),
            "siblings" => Ok(Self::Siblings),
            "tests" => Ok(Self::Tests),
            _ => Err(format!(
                "unknown Grok section: {value}. Use callers, callees, siblings, or tests"
            )),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Callers => "callers",
            Self::Callees => "callees",
            Self::Siblings => "siblings",
            Self::Tests => "tests",
        }
    }
}

pub(super) fn qualified_owner(selector: &str) -> Option<String> {
    let super::TargetSpec::Symbol(name) = super::parse_target_spec(selector) else {
        return None;
    };
    let bare = super::split_qualified(&name);
    (bare != name && !bare.is_empty()).then(|| {
        name[..name.len() - bare.len() - 1]
            .trim_end_matches(':')
            .to_string()
    })
}

pub(super) fn section_siblings(
    entries: &[crate::types::OutlineEntry],
    target: &super::ResolvedTarget,
    owner: Option<&str>,
    content: &str,
    lang: crate::types::Lang,
) -> Vec<super::SiblingEntry> {
    fn parent<'a>(
        entries: &'a [crate::types::OutlineEntry],
        target: &super::ResolvedTarget,
    ) -> Option<&'a crate::types::OutlineEntry> {
        for entry in entries {
            if entry.children.iter().any(|child| {
                child.start_line == target.start_line
                    && child.name == target.name
                    && child.signature == target.signature
            }) {
                return Some(entry);
            }
            if let Some(found) = parent(&entry.children, target) {
                return Some(found);
            }
        }
        None
    }
    let qualified;
    let candidates = if let Some(owner) = owner {
        // Use the full qualified owner and exact AST order; never fall back to
        // another same-named owner when the outline has a depth limit.
        qualified = crate::lang::outline::qualified_declarations(content, lang, owner, None, true)
            .into_iter()
            .map(|(entry, _)| entry)
            .collect::<Vec<_>>();
        qualified.as_slice()
    } else {
        parent(entries, target).map_or(entries, |entry| entry.children.as_slice())
    };
    candidates
        .iter()
        .filter(|entry| {
            !matches!(
                entry.kind,
                crate::types::OutlineKind::Import | crate::types::OutlineKind::Export
            )
        })
        .filter(|entry| {
            !(entry.start_line == target.start_line
                && entry.name == target.name
                && entry.signature == target.signature)
        })
        .map(|entry| super::SiblingEntry {
            name: entry.name.clone(),
            kind: entry.kind,
            start_line: entry.start_line,
            end_line: entry.end_line,
            signature: entry.signature.clone(),
        })
        .collect()
}

pub(crate) fn section_page(
    target: &str,
    scope: &Path,
    bloom: &BloomFilterCache,
    session: &Session,
    root: Option<&Path>,
    section: Section,
    page: Page,
    budget: Option<u64>,
    mcp: bool,
    prelude: &str,
) -> Result<String, TilthError> {
    let caps = GrokCaps {
        max_callees: usize::MAX,
        max_callers: usize::MAX,
        max_siblings: usize::MAX,
        max_tests: usize::MAX,
        max_body_lines: 0,
    };
    let result = grok_collect(target, scope, bloom, session, caps, root, Some(section))?;
    Ok(format_page(
        &result, target, scope, section, page, budget, mcp, prelude,
    ))
}

fn format_page(
    result: &GrokResult,
    selector: &str,
    scope: &Path,
    section: Section,
    page: Page,
    budget: Option<u64>,
    mcp: bool,
    prelude: &str,
) -> String {
    enum Callee<'a> {
        Internal(&'a super::ResolvedCallee),
        External(&'a str),
    }
    let target = &result.target;
    let mut header = format!(
        "{prelude}# grok: {} [{}:{}]\n\n## {}",
        target.name,
        display_rel(&target.path, scope),
        target.start_line,
        section.name()
    );
    if target.other_def_count > 0 {
        let _=write!(header,"\n> ambiguous: {} other definitions match this name; keep the target selector/scope unchanged or use a qualified selector.",target.other_def_count);
    }
    if let Some(note) = &target.resolution_note {
        let _ = write!(header, "\n> {note}");
    }
    if let Some(signature) = &target.signature {
        let _ = write!(header, "\n\nSignature: {signature}");
    }
    let _ = write!(
        header,
        "\n\n> Source: {}:{}-{}; use the file reader's section/source-line paging for the body.",
        target.path.display(),
        target.start_line,
        target.end_line
    );
    let (total, selected) = match section {
        Section::Callers => {
            let mut rows: Vec<_> = result.callers.iter().collect();
            rows.sort_by(|a, b| {
                (
                    &a.path,
                    a.line,
                    &a.calling_function,
                    &a.receiver,
                    &a.call_text,
                )
                    .cmp(&(
                        &b.path,
                        b.line,
                        &b.calling_function,
                        &b.receiver,
                        &b.call_text,
                    ))
            });
            let total = rows.len();
            let (start, end) = page.bounds(total);
            let selected = rows[start..end]
                .iter()
                .map(|row| {
                    format!(
                        "{}:{}  in {}()",
                        display_rel(&row.path, scope),
                        row.line,
                        row.calling_function
                    )
                })
                .collect::<Vec<_>>();
            (total, selected)
        }
        Section::Tests => {
            let mut rows: Vec<_> = result.tests.iter().collect();
            rows.sort_by(|a, b| {
                (&a.path, a.line, &a.test_name).cmp(&(&b.path, b.line, &b.test_name))
            });
            let total = rows.len();
            let (start, end) = page.bounds(total);
            let selected = rows[start..end]
                .iter()
                .map(|row| {
                    format!(
                        "{}:{}  {}",
                        display_rel(&row.path, scope),
                        row.line,
                        row.test_name
                    )
                })
                .collect::<Vec<_>>();
            (total, selected)
        }
        Section::Siblings => {
            let mut rows: Vec<_> = result.siblings.iter().collect();
            // Stable sorting keeps AST source order for declarations on the same line.
            rows.sort_by_key(|row| row.start_line);
            let total = rows.len();
            let (start, end) = page.bounds(total);
            let selected = rows[start..end]
                .iter()
                .map(|row| {
                    format!(
                        "{}:{}-{}  {}  {}",
                        display_rel(&target.path, scope),
                        row.start_line,
                        row.end_line,
                        row.name,
                        row.signature
                            .as_deref()
                            .unwrap_or_else(|| kind_label(row.kind))
                    )
                })
                .collect::<Vec<_>>();
            (total, selected)
        }
        Section::Callees => {
            let mut internal: Vec<_> = result.callees_internal.iter().collect();
            internal.sort_by(|a, b| {
                (&a.file, a.start_line, a.end_line, &a.name, &a.signature).cmp(&(
                    &b.file,
                    b.start_line,
                    b.end_line,
                    &b.name,
                    &b.signature,
                ))
            });
            let mut external: Vec<_> = result.callees_external.iter().map(String::as_str).collect();
            external.sort_unstable();
            let rows: Vec<_> = internal
                .into_iter()
                .map(Callee::Internal)
                .chain(external.into_iter().map(Callee::External))
                .collect();
            let total = rows.len();
            let (start, end) = page.bounds(total);
            let _ = write!(
                header,
                "\n> {} internal, {} external callees.",
                result.total_callees_internal, result.total_callees_external
            );
            let selected = rows[start..end]
                .iter()
                .map(|row| match row {
                    Callee::Internal(row) => format!(
                        "{}:{}-{}  {}  {}",
                        display_rel(&row.file, scope),
                        row.start_line,
                        row.end_line,
                        row.name,
                        row.signature.as_deref().unwrap_or("")
                    ),
                    Callee::External(name) => format!("{name}  extern"),
                })
                .collect::<Vec<_>>();
            (total, selected)
        }
    };
    let mut output = format!("{header}\n\n{}", page.summary(total, section.name()));
    for entry in selected {
        output.push_str("\n\n");
        output.push_str(&entry);
    }
    let (_, end) = page.bounds(total);
    if end < total {
        if mcp {
            // Preserve the original selector: path:line alone can select a different
            // declaration when two qualified methods are on the same source line.
            let _ = write!(
                output,
                "\n> Next page: {} (keep root/scope and source unchanged).",
                serde_json::json!({
                "target":selector,"section":section.name(),"offset":end,"limit":page.limit})
            );
        } else {
            let _=write!(output,"\n> Next page: --section {} --offset {end} --limit {} (keep target, scope and source unchanged).",section.name(),page.limit);
        }
    } else {
        output.push_str("\n> End of listing.");
    }
    crate::listing::finish(&output, budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_collection_never_claims_target_or_delegate_body_expansion() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("code.rs");
        let mut source = String::from("pub fn work() {\n");
        for i in 0..90 {
            writeln!(source, "let value{i} = {i};").unwrap();
        }
        source.push_str("}\npub fn wrapper() { work(); }\nfn caller() { wrapper(); }\n");
        std::fs::write(&path, &source).unwrap();
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let session = Session::new();
        let bloom = BloomFilterCache::new();
        let caps = GrokCaps {
            max_callees: usize::MAX,
            max_callers: usize::MAX,
            max_siblings: usize::MAX,
            max_tests: usize::MAX,
            max_body_lines: 0,
        };
        for section in [
            Section::Callers,
            Section::Callees,
            Section::Siblings,
            Section::Tests,
        ] {
            let result = grok_collect(
                "wrapper",
                root.path(),
                &bloom,
                &session,
                caps,
                None,
                Some(section),
            )
            .unwrap();
            assert!(result.body.is_empty() && result.delegate_body.is_none());
            if section != Section::Callers {
                assert!(result.callers.is_empty());
            }
            if section != Section::Tests {
                assert!(result.tests.is_empty());
            }
            if section != Section::Callees {
                assert!(result.callees_internal.is_empty() && result.callees_external.is_empty());
            }
            if section != Section::Siblings {
                assert!(result.siblings.is_empty());
            }
            assert!(!session.is_expanded(&path, 93, mtime));
            assert!(!session.is_expanded(&path, 1, mtime));
        }
        let overview = super::super::grok(
            "wrapper",
            root.path(),
            &bloom,
            &session,
            GrokCaps::default(),
        )
        .unwrap();
        let (_, delegate) = overview.delegate_body.unwrap();
        assert!(!delegate.contains("shown earlier"), "{delegate}");
        assert!(session.is_expanded(&path, 1, mtime));
        let first = super::super::grok(
            "work",
            root.path(),
            &bloom,
            &Session::new(),
            GrokCaps::default(),
        )
        .unwrap();
        assert!(!first.body.contains("shown earlier"));
    }

    #[test]
    fn local_sections_do_not_attempt_the_repository_caller_walk() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("code.rs");
        std::fs::write(&path, "pub fn needle() { local(); }\nfn local() {}\n").unwrap();
        for section in [Section::Siblings, Section::Callees] {
            let result = section_page(
                &format!("{}:1", path.display()),
                &root.path().join("missing"),
                &BloomFilterCache::new(),
                &Session::new(),
                None,
                section,
                Page::new(0, 1).unwrap(),
                None,
                false,
                "",
            )
            .unwrap();
            assert!(result.contains("local"), "{result}");
        }
    }

    #[test]
    fn navigation_only_budget_overflow_retains_retry_notice() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("code.rs"),
            "fn needle() {}\nfn caller_a() { needle(); }\nfn caller_b() { needle(); }\n",
        )
        .unwrap();
        let result = grok_collect(
            "needle",
            root.path(),
            &BloomFilterCache::new(),
            &Session::new(),
            GrokCaps::full(),
            None,
            Some(Section::Callers),
        )
        .unwrap();
        let output = format_page(
            &result,
            "needle",
            root.path(),
            Section::Callers,
            Page::new(0, 1).unwrap(),
            None,
            true,
            "",
        );
        let cut = output.find("\n> Next page:").unwrap();
        let budget = crate::types::estimate_tokens(cut as u64) + 1;
        let output = format_page(
            &result,
            "needle",
            root.path(),
            Section::Callers,
            Page::new(0, 1).unwrap(),
            Some(budget),
            true,
            "",
        );
        assert!(output.contains("Page or navigation omitted"), "{output}");
        assert!(!output.contains("Next page:"), "{output}");
    }
}
