//! Stable pagination shared by directory reads and recursive file listings.

use std::fmt::Write as _;

pub(crate) const DEFAULT_LIMIT: usize = 50;

#[derive(Clone, Copy)]
pub(crate) struct Page {
    pub offset: i64,
    pub limit: usize,
}

impl Page {
    pub fn new(offset: i64, limit: usize) -> Result<Self, String> {
        if limit == 0 {
            return Err("limit must be an integer >= 1".into());
        }
        Ok(Self { offset, limit })
    }

    pub fn optional(args: &serde_json::Value) -> Result<Option<Self>, String> {
        if args.get("offset").is_some() || args.get("limit").is_some() {
            Self::from_args(args).map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn from_tuple(value: (i64, Option<usize>)) -> Result<Self, String> {
        Self::new(value.0, value.1.unwrap_or(DEFAULT_LIMIT))
    }

    /// Page complete logical blocks. Headers and entry context are never counted.
    pub fn render(self, header: &str, entries: &[String], noun: &str, mcp: bool) -> String {
        let (start, end) = self.bounds(entries.len());
        self.render_selected(header, &entries[start..end], entries.len(), noun, mcp)
    }

    /// Render blocks already selected from typed records, without formatting hidden entries.
    pub fn render_selected(
        self,
        header: &str,
        selected: &[String],
        total: usize,
        noun: &str,
        mcp: bool,
    ) -> String {
        let mut out = format!("{header}\n\n{}", self.summary(total, noun));
        for entry in selected {
            out.push_str("\n\n");
            out.push_str(entry);
        }
        self.append_navigation(&mut out, total, mcp);
        out
    }

    pub fn from_args(args: &serde_json::Value) -> Result<Self, String> {
        let parse = |name: &str, default: usize| {
            args.get(name).map_or(Ok(default), |value| {
                value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| {
                        format!("{name} must be a nonnegative integer that fits this platform")
                    })
            })
        };
        let offset = args.get("offset").map_or(Ok(0), |value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().map(|n| i64::try_from(n).unwrap_or(i64::MAX)))
                .ok_or_else(|| "offset must be an integer".to_string())
        })?;
        Self::new(offset, parse("limit", DEFAULT_LIMIT)?)
    }

    pub fn prefix(self) -> usize {
        if self.offset < 0 {
            usize::MAX
        } else {
            (self.offset as usize).saturating_add(self.limit)
        }
    }

    pub fn bounds(self, total: usize) -> (usize, usize) {
        let start = if self.offset < 0 {
            total.saturating_sub(usize::try_from(self.offset.unsigned_abs()).unwrap_or(usize::MAX))
        } else {
            usize::try_from(self.offset)
                .unwrap_or(usize::MAX)
                .min(total)
        };
        (start, start.saturating_add(self.limit).min(total))
    }

    pub fn summary(self, total: usize, noun: &str) -> String {
        let (start, end) = self.bounds(total);
        let range = if start == end {
            "0".to_string()
        } else {
            format!("{}-{end}", start + 1)
        };
        format!(
            "> Showing {noun} {range} of {total} (offset {}, limit {}). If budget truncates this page, retry the same offset with a smaller limit.",
            self.offset, self.limit
        )
    }

    /// Place continuation after all entries, so a truncated response cannot
    /// advertise an offset past entries hidden by the output budget.
    pub fn append_navigation(self, out: &mut String, total: usize, mcp: bool) {
        let (_, end) = self.bounds(total);
        if end < total {
            if mcp {
                let _ = write!(
                    out,
                    "\n> Next page: {} (keep path/scope and filters unchanged).",
                    serde_json::json!({"offset": end, "limit": self.limit})
                );
            } else {
                let _ = write!(
                    out,
                    "\n> Next page: --offset {end} --limit {} (keep path/scope and filters unchanged).",
                    self.limit
                );
            }
        }
    }
}

/// A continuation must never survive an output budget that hides page entries,
/// including an earlier page in a batch or combined scope response.
pub(crate) fn finish(output: &str, budget: Option<u64>) -> String {
    let budget = budget.unwrap_or(crate::budget::DEFAULT_BUDGET);
    finish_with_reported_budget(output, budget, budget)
}

pub(crate) fn finish_with_reported_budget(
    output: &str,
    budget: u64,
    reported_budget: u64,
) -> String {
    if crate::types::estimate_tokens(output.len() as u64) <= budget {
        return output.into();
    }
    let safe = output
        .lines()
        .filter(|line| !line.starts_with("> Next page:"))
        .collect::<Vec<_>>()
        .join("\n");
    crate::budget::apply_with_reported_budget(&format!(
        "> Page or navigation omitted to fit budget; retry the same offset with a smaller limit or a larger budget.\n\n{safe}"
    ), budget, reported_budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_offsets_count_from_end_and_saturate() {
        assert_eq!(Page::new(-20, 10).unwrap().bounds(30), (10, 20));
        assert_eq!(Page::new(-1, 10).unwrap().bounds(30), (29, 30));
        assert_eq!(Page::new(i64::MIN, 10).unwrap().bounds(30), (0, 10));
        assert_eq!(Page::new(-1, 10).unwrap().bounds(0), (0, 0));
    }

    #[test]
    fn terminal_pages_omit_marker_but_keep_continuations_when_needed() {
        let entries = vec!["first".to_string(), "second".to_string()];
        let first = Page::new(0, 1)
            .unwrap()
            .render("# Test", &entries, "items", true);
        let last = Page::new(1, 1)
            .unwrap()
            .render("# Test", &entries, "items", true);
        assert!(first.contains("Next page:"), "{first}");
        assert!(
            !last.contains("Next page:") && !last.contains("End of listing."),
            "{last}"
        );
        assert!(last.contains("Showing items 2-2 of 2"), "{last}");
    }

    #[test]
    fn budgeted_output_keeps_literal_end_marker_text() {
        let source = format!(
            "# File\n> End of listing. literal source\n\n{}",
            "line\n".repeat(500)
        );
        let output = finish(&source, Some(150));
        assert!(
            output.contains("> End of listing. literal source"),
            "{output}"
        );
    }
}
