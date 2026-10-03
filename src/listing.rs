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
