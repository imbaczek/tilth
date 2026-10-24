//! Stable pagination shared by directory reads and recursive file listings.

use std::fmt::Write as _;

pub(crate) const DEFAULT_LIMIT: usize = 50;

#[derive(Clone, Copy)]
pub(crate) struct Page {
    pub offset: usize,
    pub limit: usize,
}

impl Page {
    pub fn new(offset: usize, limit: usize) -> Result<Self, String> {
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

    pub fn from_tuple(value: (usize, Option<usize>)) -> Result<Self, String> {
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
        Self::new(parse("offset", 0)?, parse("limit", DEFAULT_LIMIT)?)
    }

    pub fn bounds(self, total: usize) -> (usize, usize) {
        let start = self.offset.min(total);
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
        } else {
            out.push_str("\n> End of listing.");
        }
    }
}

/// A continuation must never survive an output budget that hides page entries,
/// including an earlier page in a batch or combined scope response.
pub(crate) fn finish(output: &str, budget: Option<u64>) -> String {
    let budget = budget.unwrap_or(crate::budget::DEFAULT_BUDGET);
    if crate::types::estimate_tokens(output.len() as u64) <= budget {
        return output.into();
    }
    let safe = output
        .lines()
        .filter(|line| !line.starts_with("> Next page:") && !line.starts_with("> End of listing."))
        .collect::<Vec<_>>()
        .join("\n");
    crate::budget::apply(&format!(
        "> Page or navigation omitted to fit budget; retry the same offset with a smaller limit or a larger budget.\n\n{safe}"
    ), budget)
}
