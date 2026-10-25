//! Iterative source-order traversal shared by structural analyses.
use tree_sitter::Node;

/// Yield named nodes in source-order preorder, including `node` itself when named.
/// The iterative cursor stays within the supplied node's subtree.
pub(crate) fn named_descendants(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut cursor = node.walk();
    let mut depth = 0usize;
    let mut done = false;
    std::iter::from_fn(move || loop {
        if done {
            return None;
        }
        let current = cursor.node();
        if cursor.goto_first_child() {
            depth += 1;
        } else {
            loop {
                if depth == 0 {
                    done = true;
                    break;
                }
                if cursor.goto_next_sibling() {
                    break;
                }
                cursor.goto_parent();
                depth -= 1;
            }
        }
        if current.is_named() {
            return Some(current);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_order_and_subtree_boundaries_are_preserved() {
        let source = "fn first(){ second(); } fn outside(){}";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let first = tree.root_node().named_child(0).unwrap();
        let names: Vec<_> = named_descendants(first)
            .filter(|n| n.kind() == "identifier")
            .map(|n| n.utf8_text(source.as_bytes()).unwrap())
            .collect();
        assert_eq!(names, ["first", "second"]);
        let names: Vec<_> = named_descendants(tree.root_node())
            .filter(|n| n.kind() == "identifier")
            .map(|n| n.utf8_text(source.as_bytes()).unwrap())
            .collect();
        assert_eq!(names, ["first", "second", "outside"]);
    }
}
