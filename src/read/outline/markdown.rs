/// Markdown outlines share section spans and TOC addresses with section
/// selection, including headings nested inside blockquotes and lists.
use crate::lang::outline::parse_markdown;
use crate::read::heading_sections_from_tree;

pub fn outline(buf: &[u8], max_lines: usize) -> String {
    let Ok(content) = std::str::from_utf8(buf) else {
        return String::new();
    };
    let Some(tree) = parse_markdown(content) else {
        return String::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let sections = heading_sections_from_tree(tree.root_node(), &lines, max_lines);
    let mut entries: Vec<_> = sections
        .iter()
        .map(|section| {
            let indent = "  ".repeat(usize::from(section.level).saturating_sub(1));
            let hashes = "#".repeat(usize::from(section.level));
            let display = if section.title.len() > 80 {
                format!("{}...", crate::types::truncate_str(&section.title, 77))
            } else {
                section.title.clone()
            };
            format!(
                "[{}-{}] {} {indent}{hashes} {display}",
                section.start, section.end, section.address
            )
        })
        .collect();

    // Preserve the capped outline's code-block summary: count only blocks
    // visited before the final heading that exhausted the entry cap.
    let until_line = if sections.len() >= max_lines {
        sections.last().map_or(1, |section| section.start)
    } else {
        usize::MAX
    };
    let code_block_count = count_code_blocks(tree.root_node(), until_line);
    if code_block_count > 0 {
        entries.push(format!("\n({code_block_count} code blocks)"));
    }
    entries.join("\n")
}

fn count_code_blocks(node: tree_sitter::Node, until_line: usize) -> usize {
    if node.start_position().row + 1 >= until_line {
        return 0;
    }
    if node.kind() == "fenced_code_block" {
        return 1;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .map(|child| count_code_blocks(child, until_line))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_outline_address_selects_its_displayed_range() {
        let input = b"## Root\n###### Shared\nfirst\n\n> # Nested\n> ## Shared\n> inside\n\n### Shared\noutside\n## Next\nend\n";
        let listed = String::from_utf8_lossy(input).replace(
            "> # Nested\n> ## Shared\n> inside",
            "- # Nested\n  ## Shared\n  inside",
        );
        for input in [input.as_slice(), listed.as_bytes()] {
            let result = outline(input, 100);
            assert!(result.contains("toc:1.2     ### Shared"), "{result}");
            for entry in result.lines().filter(|line| line.starts_with('[')) {
                let mut parts = entry.split_whitespace();
                let displayed = parts.next().unwrap().trim_matches(['[', ']']);
                let address = parts.next().unwrap();
                assert!(address.starts_with("toc:"), "{entry}");
                assert_eq!(
                    crate::read::resolve_range(input, address).unwrap(),
                    crate::read::parse_range(displayed).unwrap(),
                    "{entry}"
                );
            }
        }
    }

    #[test]
    fn capped_outline_addresses_match_uncapped_prefix() {
        let input = b"# Root\n~~~md\n# Fake\n~~~\n### Child\n## Sibling\n~~~\ncode\n~~~\n# Next\n";
        let full = outline(input, 100);
        let capped = outline(input, 2);
        let prefix = full.lines().take(2).collect::<Vec<_>>().join("\n");
        assert!(capped.starts_with(&prefix), "{capped}");
        assert!(capped.contains("(1 code blocks)"), "{capped}");
        assert!(!capped.contains("Sibling"), "{capped}");
        assert!(outline(input, 0).is_empty());
    }

    #[test]
    fn basic_headings() {
        let input = b"# H1\nSome text\n## H2\nMore text\n";
        let result = outline(input, 100);
        let lines: Vec<&str> = result.lines().collect();

        assert_eq!(lines.len(), 2);
        // H1 extends to end of file (line 4) since no other H1
        assert_eq!(lines[0], "[1-4] toc:1 # H1");
        // H2 also extends to end of file (line 4)
        assert_eq!(lines[1], "[3-4] toc:1.1   ## H2");
    }

    #[test]
    fn code_blocks_skipped() {
        let input = b"# Real Heading\n\n```\ncode\n```\n";
        let result = outline(input, 100);

        // Should only find the real heading, not any inside code block
        assert!(result.starts_with("[1-5] toc:1 # Real Heading"));
        assert!(result.contains("(1 code blocks)"));
        assert!(!result.contains("Fake Heading"));
    }

    #[test]
    fn code_block_count() {
        let input = b"# Heading\n```\ncode\n```\n```\nmore\n```\n";
        let result = outline(input, 100);

        assert!(result.contains("(2 code blocks)"));
    }

    #[test]
    fn nested_heading_ranges() {
        let input = b"# A\ntext\n## B\ntext\n## C\ntext\n# D\ntext\n";
        let result = outline(input, 100);
        let lines: Vec<&str> = result.lines().collect();

        assert_eq!(lines.len(), 4);
        // A extends until D (line 7), so ends at line 6
        assert_eq!(lines[0], "[1-6] toc:1 # A");
        // B extends until C (line 5), so ends at line 4
        assert_eq!(lines[1], "[3-4] toc:1.1   ## B");
        // C extends until D (line 7), so ends at line 6
        assert_eq!(lines[2], "[5-6] toc:1.2   ## C");
        // D extends to end of file (line 8)
        assert_eq!(lines[3], "[7-8] toc:2 # D");
    }

    #[test]
    fn last_heading_to_eof() {
        let input = b"# Heading\nline 2\nline 3\nline 4\n";
        let result = outline(input, 100);

        // Heading should extend to line 4 (total line count)
        assert_eq!(result, "[1-4] toc:1 # Heading");
    }

    #[test]
    fn empty_file() {
        let input = b"";
        let result = outline(input, 100);

        assert_eq!(result, "");
    }

    /// AST handles fenced code blocks at the parser level — a `# foo` Python
    /// comment inside a fenced block is part of the `fenced_code_block` node,
    /// not an `atx_heading`. The hand-rolled scanner needed a manual fence
    /// pre-pass to avoid treating it as a heading; the AST gets this for free.
    #[test]
    fn hash_inside_fenced_code_does_not_become_heading() {
        let input = b"# Real\n\n```python\n# fake heading\nprint('x')\n```\n\n## Also Real\n";
        let result = outline(input, 100);
        let lines: Vec<&str> = result.lines().collect();
        let heading_lines: Vec<&&str> = lines.iter().filter(|l| l.starts_with('[')).collect();
        assert_eq!(heading_lines.len(), 2);
        assert!(heading_lines[0].contains("# Real"));
        assert!(heading_lines[1].contains("## Also Real"));
    }

    /// Setext headings (`Top\n====`) are not handled — the block grammar puts
    /// every setext heading as a sibling inside one document-spanning section
    /// rather than nesting them, so section-span computation doesn't apply.
    /// The old hand-rolled scanner only matched ATX too; we preserve that.
    #[test]
    fn setext_headings_silently_ignored() {
        let input = b"Top\n===\n\ncontent\n";
        let result = outline(input, 100);
        assert_eq!(result, "");
    }
}
