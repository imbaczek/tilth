use crate::types::estimate_tokens;

/// Default cap applied to MCP tool responses when no explicit `budget` is given.
/// Sits just under the host's ~25K-token tool-response limit so a broad
/// `tilth_read full:true`, `regex`, or `diff` can't blow the host budget.
pub const DEFAULT_BUDGET: u64 = 24_000;

/// Apply token budget to output. Works backwards from the cap:
/// 1. Reserve 50 tokens for header
/// 2. Truncate content at section boundaries to avoid broken output
/// 3. Never exceed the budget
pub fn apply(output: &str, budget: u64) -> String {
    if let Some(errors_start) = output.find("\n\n## Scope errors\n") {
        let lines_start = errors_start + "\n\n## Scope errors\n".len();
        let lines_end = output[lines_start..]
            .find("\n\n")
            .map_or(output.len(), |offset| lines_start + offset);
        let error_lines: Vec<&str> = output[lines_start..lines_end]
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        if !error_lines.is_empty() && estimate_tokens(output.len() as u64) > budget {
            let mut protected = output[..lines_start].to_string();
            let byte_limit = budget.saturating_mul(4) as usize;
            let mut included = 0usize;
            for line in &error_lines {
                if protected.len() + line.len() + 1 > byte_limit {
                    break;
                }
                protected.push_str(line);
                protected.push('\n');
                included += 1;
            }
            let mut omitted = error_lines.len() - included;
            if omitted > 0 {
                let mut marker = format!("... {omitted} scope errors omitted");
                while included > 0 && protected.len() + marker.len() > byte_limit {
                    let cut = protected
                        .rfind("\n- ")
                        .expect("included scope error has a line boundary");
                    protected.truncate(cut + 1);
                    included -= 1;
                    omitted += 1;
                    marker = format!("... {omitted} scope errors omitted");
                }
                if protected.len() + marker.len() > byte_limit {
                    return "... truncated".to_string();
                }
                protected.push_str(&marker);
            } else if lines_end < output.len() {
                let marker = "... truncated optional results";
                if protected.len() + marker.len() <= byte_limit {
                    protected.push_str(marker);
                }
            }
            return protected.trim_end().to_string();
        }
    }
    let current = estimate_tokens(output.len() as u64);
    if current <= budget {
        return output.to_string();
    }

    let header_reserve = 50u64;
    let content_budget = budget.saturating_sub(header_reserve);
    let max_bytes = (content_budget * 4) as usize; // inverse of estimate_tokens

    // Find the first newline after the header (first line)
    let header_end = output.find('\n').unwrap_or(0);
    let header = &output[..header_end];
    let body = &output[header_end..];

    if body.len() <= max_bytes {
        return output.to_string();
    }

    let safe_max = body.floor_char_boundary(max_bytes);
    let truncated = &body[..safe_max];

    // Prefer section boundaries (\n\n##) to avoid cutting mid-match in search results.
    // Fallback is `safe_max` (= truncated.len()), never `max_bytes`: `max_bytes` may
    // land mid-UTF-8-codepoint and would panic `&body[..cut_point]` on emoji-heavy
    // single-line content with no newline in the truncated region.
    //
    // Reject `\n\n` cuts at position 0: body always starts with the structural
    // header/body separator, and for code-rendered output (every line carries
    // a `<n>:<hash>|` prefix, so blank source lines are still non-empty) that's
    // the *only* `\n\n` in the body. Without this filter, every truncated code
    // file would return zero content lines.
    let cut_point = truncated
        .rfind("\n\n##")
        .filter(|&p| p > 0)
        .or_else(|| truncated.rfind("\n\n").filter(|&p| p > 0))
        .or_else(|| truncated.rfind('\n').filter(|&p| p > 0))
        .unwrap_or(safe_max);

    let clean_body = &body[..cut_point];

    let omitted_bytes = output.len() - header_end - cut_point;
    let remaining_tokens = estimate_tokens(omitted_bytes as u64);
    format!(
        "{header}{clean_body}\n\n... truncated ({remaining_tokens} tokens omitted, budget: {budget})"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_error_budget_keeps_whole_entries_or_reports_omission() {
        let output = "# Search: \"target\"\n\n## Scope errors\n- /a: denied\n- /b: io failure\n\n## Matches\nvery long optional result".repeat(8);
        let short = apply(&output, 14);
        assert!(!short.contains("very long optional result"));
        assert!(!short.contains("- /b: io failur\n"));
        assert!(short.contains("scope errors omitted") || short.contains("... truncated"));

        let enough = apply(&output, 30);
        assert!(enough.contains("- /a: denied"));
        assert!(enough.contains("- /b: io failure"));
    }

    #[test]
    fn multi_symbol_final_budget_protects_error_prelude() {
        let output = format!(
            "# Search: multiple symbols\n\n## Scope errors\n- /missing: not found\n\n{}",
            "# Search: \"alpha\"\n\n## Matches\noptional expansion\n\n---\n".repeat(30)
        );
        let result = apply(&output, 40);
        assert!(result.contains("- /missing: not found"), "{result}");
        assert!(!result.contains("optional expansion"), "{result}");
    }
    use std::fmt::Write as _;

    #[test]
    fn apply_roomy_budget_returns_input_unchanged() {
        // 20 chars ≈ 5 tokens; budget of 1000 is way over.
        let input = "# header\nshort body\n";
        assert_eq!(apply(input, 1000), input);
    }

    #[test]
    fn apply_tight_budget_truncates_with_marker() {
        // Build a multi-line body large enough to force truncation.
        let mut input = String::from("# header\n");
        for i in 1..=200 {
            let _ = writeln!(input, "line {i}");
        }
        let out = apply(&input, 80);
        assert!(out.contains("... truncated"), "marker line missing: {out}");
        assert!(out.len() < input.len(), "must shrink: {out}");
    }

    #[test]
    fn apply_emoji_no_newline_does_not_panic() {
        // Single-line UTF-8 with no \n in the truncated region: `max_bytes` may land
        // mid-codepoint, so the fallback must clamp to a char boundary.
        let body: String = "🦀".repeat(500); // 2000 bytes, no newlines
        let input = format!("# header\n{body}");
        // Pick a budget so max_bytes lands somewhere mid-crab.
        let out = apply(&input, 100);
        assert!(out.contains("... truncated"), "expected truncation: {out}");
    }

    #[test]
    fn single_long_line_body_does_not_collapse() {
        // Regression: a body that is one very long line with no interior \n has no
        // `\n\n` to land on, so rfind('\n') finds only the leading separator at
        // offset 0. Without the `filter(|&p| p > 0)` guard on that arm, cut_point
        // becomes 0, clean_body is empty, and the response collapses to
        // `header\n\n... truncated` with zero content.
        let long_line = "x".repeat(10_000);
        let input = format!("# header\n{long_line}");
        let out = apply(&input, 400);
        // The truncation marker must appear (body is 10k+ chars).
        assert!(out.contains("truncated"), "marker line missing: {out:.80?}");
        // The body content must survive — the output must contain some 'x' chars.
        // If cut_point landed at 0, clean_body would be empty and no 'x' survives.
        assert!(
            out.contains('x'),
            "body content collapsed to empty — single-long-line truncation bug: {out:.80?}"
        );
    }

    #[test]
    fn apply_code_format_survives_header_separator() {
        // Code-rendered output has `<n>:<hash>|<content>\n` on every line, so
        // the only `\n\n` in the body is the header/body separator at position
        // 0. Pre-fix, `rfind("\n\n")` returned 0 and `clean_body` was empty —
        // the response was just `<header>\n\n... truncated` regardless of how
        // generous the budget was. Verify the cut now lands deep enough that
        // real content survives.
        let mut input = String::from("# src/foo.rs (200 lines, ~2k tokens) [full]\n\n");
        for i in 1..=200 {
            let _ = writeln!(input, "{i}:abc|let x_{i} = {i};");
        }
        // Tight budget — must truncate, but should leave room for many lines.
        let out = apply(&input, 400);
        assert!(out.contains("... truncated"), "must truncate: {out}");
        assert!(
            out.contains("1:abc|let x_1 ="),
            "first code line must survive truncation: {out}"
        );
        // Cut must land deep into the body (the position-0 `\n\n` is rejected),
        // so several content lines survive — not just the header separator.
        let kept_code_lines = out.lines().filter(|l| l.contains(":abc|let x_")).count();
        assert!(
            kept_code_lines > 5,
            "must keep many content lines, not cut at header separator: {out}"
        );
    }
}
