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
    if estimate_tokens(output.len() as u64) > budget {
        if let Some(outline) = apply_outline(output, budget) {
            return outline;
        }
    }
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
    if header.starts_with("# Search:") {
        // Each query may already have a total before the joined response is
        // budgeted. Preserve those totals even when their sections are cut.
        let mut prior_tokens = 0u64;
        for line in output.lines() {
            if let Some(count) = line
                .strip_prefix("... total omitted: ")
                .and_then(|rest| rest.split_once(" tokens (budget: "))
                .and_then(|(count, rest)| rest.strip_suffix(')').map(|_| count))
                .and_then(|count| count.parse::<u64>().ok())
            {
                prior_tokens = prior_tokens.saturating_add(count);
            }
        }
        let diagnostic_bytes: usize = output[header_end + cut_point..]
            .split_inclusive('\n')
            .filter(|line| {
                let line = line.trim_start_matches('\n');
                line.starts_with("... total omitted: ")
                    || line.starts_with("!! expansion omitted")
                    || (line.starts_with("... ")
                        && line.contains("lower-value match(es) omitted to fit budget"))
            })
            .map(str::len)
            .sum();
        let clean_body: String = clean_body
            .split_inclusive('\n')
            .filter(|line| {
                !line
                    .trim_end_matches('\n')
                    .starts_with("... total omitted: ")
            })
            .collect();
        let total = prior_tokens.saturating_add(estimate_tokens(
            omitted_bytes.saturating_sub(diagnostic_bytes) as u64,
        ));
        format!("{header}{clean_body}\n\n... total omitted: {total} tokens (budget: {budget})")
    } else {
        format!(
            "{header}{clean_body}\n\n... truncated ({remaining_tokens} tokens omitted, budget: {budget})"
        )
    }
}

/// Code outlines have navigational entry rows followed by optional signature
/// rows. Compact those details before dropping whole entries from the end.
fn apply_outline(output: &str, budget: u64) -> Option<String> {
    use std::fmt::Write as _;
    let header = output.lines().next()?;
    if !(header.starts_with("# Scope: ")
        || (header.starts_with("# ") && header.ends_with("[outline]")))
    {
        return None;
    }
    // Each entry carries any immediately preceding scope/file headers so a
    // retained row always has its file context, including in joined reads.
    let mut entries: Vec<(String, bool, Vec<&str>)> = Vec::new();
    let mut pending = String::new();
    let mut in_outline = false;
    let mut capped = false;
    for line in output.lines() {
        if line.starts_with("# ") {
            if line.ends_with("[outline]") {
                in_outline = true;
            } else if line.starts_with("# Scope: ") {
                in_outline = false;
            } else {
                return None;
            }
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str(line);
            pending.push('\n');
            continue;
        }
        if line.is_empty() || line == "---" {
            continue;
        }
        let trimmed = line.trim_start();
        let is_entry = trimmed
            .strip_prefix('[')
            .and_then(|s| s.split_once(']'))
            .is_some_and(|(range, rest)| {
                !range.is_empty()
                    && range.chars().all(|c| c.is_ascii_digit() || c == '-')
                    && rest.starts_with(' ')
            });
        if is_entry && in_outline {
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str(line);
            pending.push('\n');
            entries.push((std::mem::take(&mut pending), false, Vec::new()));
        } else if pending.is_empty()
            && !entries.is_empty()
            && line.starts_with(' ')
            && !trimmed.starts_with('>')
        {
            entries.last_mut()?.1 = true;
        } else if line.starts_with("> outline truncated") {
            capped = true;
        } else if !pending.is_empty() {
            pending.push_str(line);
            pending.push('\n');
        } else {
            entries.last_mut()?.2.push(line);
        }
    }
    if entries.is_empty() || !pending.is_empty() {
        return None;
    }
    let total = entries.len();
    let signatures = entries.iter().any(|entry| entry.1);
    let mut result = String::new();
    let mut with_notes = String::new();
    let mut ends = vec![0];
    let mut note_ends = vec![0];
    let mut note_counts = vec![0];
    for entry in &entries {
        result.push_str(&entry.0);
        with_notes.push_str(&entry.0);
        for note in &entry.2 {
            with_notes.push_str(note);
            with_notes.push('\n');
        }
        ends.push(result.len());
        note_ends.push(with_notes.len());
        note_counts.push(note_counts.last()? + entry.2.len());
    }
    for keep in (0..=total).rev() {
        result.truncate(ends[keep]);
        with_notes.truncate(note_ends[keep]);
        let marker = format!(
            "... outline compacted ({} entries omitted{}{})",
            total - keep,
            if signatures {
                "; signatures omitted"
            } else {
                ""
            },
            if capped {
                "; source outline capped"
            } else {
                ""
            }
        );
        if estimate_tokens((with_notes.len() + marker.len()) as u64) <= budget {
            with_notes.push_str(&marker);
            return Some(with_notes);
        }
        if note_counts[keep] > 0 {
            result.push_str(marker.trim_end_matches(')'));
            let _ = write!(result, "; {} notes omitted)", note_counts[keep]);
        } else {
            result.push_str(&marker);
        }
        if estimate_tokens(result.len() as u64) <= budget {
            return Some(result);
        }
    }
    let marker = "... outline omitted (budget too small)";
    Some(if estimate_tokens(marker.len() as u64) <= budget {
        marker.into()
    } else {
        String::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outline_budget_drops_signatures_before_entries() {
        let input = format!("# demo.ts (200 lines, ~8k tokens) [outline]\n\n[1-100]      class Demo\n  [2-50]       fn first\n             first({})\n  [51-99]      fn second\n             second({})", "参数".repeat(100), "x".repeat(500));
        let result = apply(&input, 80);
        assert!(
            result.contains("class Demo")
                && result.contains("fn first")
                && result.contains("fn second"),
            "{result}"
        );
        assert!(
            !result.contains("first(") && !result.contains("second("),
            "{result}"
        );
        assert!(
            result.contains("0 entries omitted; signatures omitted"),
            "{result}"
        );
        assert!(estimate_tokens(result.len() as u64) <= 80);
        assert_eq!(apply(&input, 1000), input);
    }

    #[test]
    fn outline_budget_never_splits_entry_names() {
        let input = format!("# demo.ts [outline]\n\n[1-100] class Demo\n  [2-3] fn {}\n  [4-5] fn last\n\n> outline truncated — more symbols exist below cap.", "名称".repeat(100));
        let result = apply(&input, 40);
        assert!(
            !result.contains("名称") && !result.contains("fn last"),
            "{result}"
        );
        assert!(
            result.contains("2 entries omitted") && result.contains("source outline capped"),
            "{result}"
        );
        assert!(estimate_tokens(result.len() as u64) <= 40);
        assert!(apply(&input, 0).is_empty());
    }

    #[test]
    fn outline_budget_preserves_or_reports_navigation_notes() {
        let input = format!("# demo.ts [outline]\n\n[1-100] fn demo\n             demo({})\n\n> Related: sibling.ts", "x".repeat(1000));
        let result = apply(&input, 50);
        assert!(
            result.contains("fn demo") && result.contains("> Related: sibling.ts"),
            "{result}"
        );
        let input = input.replace("sibling.ts", &"large-related-path".repeat(100));
        let result = apply(&input, 50);
        assert!(
            result.contains("fn demo") && result.contains("1 notes omitted"),
            "{result}"
        );
        assert!(estimate_tokens(result.len() as u64) <= 50);
    }

    #[test]
    fn outline_budget_compacts_joined_scope_and_file_outlines() {
        let file = |name: &str| {
            format!("# {name}.ts [outline]\n\n[1-10] class Container\n  [2] fn first\n             first({})\n  [3] fn second\n             second({})", "x".repeat(400), "y".repeat(400))
        };
        let a = file("a");
        let b = file("b");
        for input in [
            format!("{a}\n\n{b}"),
            format!("# Scope: a\n\n{a}\n\n---\n# Scope: b\n\n{b}"),
        ] {
            let result = apply(&input, 120);
            assert_eq!(result.matches("class Container").count(), 2, "{result}");
            assert_eq!(result.matches("fn first").count(), 2, "{result}");
            assert_eq!(result.matches("fn second").count(), 2, "{result}");
            assert!(
                result.contains("# a.ts") && result.contains("# b.ts"),
                "{result}"
            );
            assert!(
                result.contains("0 entries omitted; signatures omitted"),
                "{result}"
            );
            assert!(estimate_tokens(result.len() as u64) <= 120);
            let tight = apply(&input, 50);
            if tight.contains("# b.ts") {
                assert_eq!(tight.matches("class Container").count(), 2, "{tight}");
            }
            assert!(estimate_tokens(tight.len() as u64) <= 50);
        }
    }

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
    fn search_budget_uses_total_omitted_marker() {
        let mut input = String::from("# Search: \"needle\"\n\n");
        for i in 0..200 {
            let _ = writeln!(input, "### file{i}.rs:1 [usage]\nmatch");
        }
        let out = apply(&input, 80);
        assert!(out.contains("... total omitted: "), "missing total: {out}");
        assert!(out.contains("tokens (budget: 80)"), "missing count: {out}");
        assert!(!out.contains("... truncated"), "old marker remained: {out}");
    }

    #[test]
    fn search_budget_carries_totals_from_all_sections() {
        let input = format!(
            "# Search: \"two queries\"\n\n## First\n{}\n\n... total omitted: 100 tokens (budget: 80)\n\n---\n\n## Second\n{}\n\n!! expansion omitted (~90 tokens; budget)\n\n... total omitted: 200 tokens (budget: 80)",
            "first match\n".repeat(20),
            "second match\n".repeat(20),
        );
        let out = apply(&input, 150);
        assert_eq!(out.matches("... total omitted:").count(), 1, "{out}");
        assert!(out.contains("## First"), "{out}");
        assert!(!out.contains("## Second"), "{out}");
        assert!(!out.contains("100 tokens (budget: 80)"), "{out}");
        let total = out
            .split("... total omitted: ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|count| count.parse::<u64>().ok())
            .unwrap();
        let omitted = &input[input.find("\n\n## Second").unwrap()..];
        let source_bytes = omitted.len()
            - "!! expansion omitted (~90 tokens; budget)\n".len()
            - "... total omitted: 200 tokens (budget: 80)".len();
        assert_eq!(total, 300 + estimate_tokens(source_bytes as u64), "{out}");
    }

    #[test]
    fn search_budget_preserves_expansion_note_and_prior_total() {
        let input = format!(
            "# Search: \"one query\"\n\n## Match\n!! expansion omitted (~90 tokens; budget)\n{}\n\n... total omitted: 200 tokens (budget: 80)",
            "source line\n".repeat(30),
        );
        let out = apply(&input, 100);
        assert!(
            out.contains("!! expansion omitted (~90 tokens; budget)"),
            "{out}"
        );
        assert_eq!(out.matches("... total omitted:").count(), 1, "{out}");
        let prefix = out.split("\n\n... total omitted:").next().unwrap();
        assert!(input.starts_with(prefix), "unexpected cut: {out}");
        let omitted = &input[prefix.len()..];
        let source_bytes = omitted.len() - "... total omitted: 200 tokens (budget: 80)".len();
        let expected = 200 + estimate_tokens(source_bytes as u64);
        assert!(
            out.contains(&format!("... total omitted: {expected} tokens")),
            "{out}"
        );
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
