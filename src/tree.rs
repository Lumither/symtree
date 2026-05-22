use std::path::{Path, PathBuf};

use crate::model::SymbolNode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleNode {
    pub path: Vec<usize>,
    pub depth: usize,
    pub is_last: bool,
    pub ancestor_is_last: Vec<bool>,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionTarget {
    pub file: PathBuf,
    pub line: usize,
    pub label: String,
}

pub fn flatten_visible(nodes: &[SymbolNode], filter: &str) -> Vec<VisibleNode> {
    let filter = normalized_filter(filter);
    let mut visible = Vec::new();
    let mut path = Vec::new();
    let mut ancestor_is_last = Vec::new();
    append_visible_nodes(
        nodes,
        filter.as_deref(),
        &mut path,
        &mut ancestor_is_last,
        &mut visible,
    );
    visible
}

pub fn get_node<'a>(nodes: &'a [SymbolNode], path: &[usize]) -> Option<&'a SymbolNode> {
    let (first, rest) = path.split_first()?;
    let node = nodes.get(*first)?;
    if rest.is_empty() {
        Some(node)
    } else {
        get_node(&node.children, rest)
    }
}

pub fn get_node_mut<'a>(nodes: &'a mut [SymbolNode], path: &[usize]) -> Option<&'a mut SymbolNode> {
    let (first, rest) = path.split_first()?;
    let node = nodes.get_mut(*first)?;
    if rest.is_empty() {
        Some(node)
    } else {
        get_node_mut(&mut node.children, rest)
    }
}

pub fn selection_target(
    root: &Path,
    nodes: &[SymbolNode],
    path: &[usize],
) -> Option<SelectionTarget> {
    let file_node = nodes.get(*path.first()?)?;
    let selected_node = get_node(nodes, path)?;
    let line = selected_node.line.unwrap_or(1);

    Some(SelectionTarget {
        file: root.join(&file_node.name),
        line,
        label: selected_node.name.clone(),
    })
}

fn append_visible_nodes(
    nodes: &[SymbolNode],
    filter: Option<&str>,
    path: &mut Vec<usize>,
    ancestor_is_last: &mut Vec<bool>,
    visible: &mut Vec<VisibleNode>,
) {
    let included_indices: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| should_include(node, filter).then_some(index))
        .collect();

    for (visible_index, node_index) in included_indices.iter().enumerate() {
        let node = &nodes[*node_index];
        let is_last = visible_index + 1 == included_indices.len();

        path.push(*node_index);
        visible.push(VisibleNode {
            path: path.clone(),
            depth: path.len().saturating_sub(1),
            is_last,
            ancestor_is_last: ancestor_is_last.clone(),
            matched: filter.is_some_and(|filter| node_matches(node, filter)),
        });

        let should_descend = if filter.is_some() {
            true
        } else {
            node.expanded
        };
        if should_descend {
            ancestor_is_last.push(is_last);
            append_visible_nodes(&node.children, filter, path, ancestor_is_last, visible);
            ancestor_is_last.pop();
        }

        path.pop();
    }
}

fn should_include(node: &SymbolNode, filter: Option<&str>) -> bool {
    match filter {
        Some(filter) => {
            node_matches(node, filter)
                || node
                    .children
                    .iter()
                    .any(|child| should_include(child, Some(filter)))
        }
        None => true,
    }
}

fn node_matches(node: &SymbolNode, filter: &str) -> bool {
    node.name.to_lowercase().contains(filter)
        || node.kind.short_label().contains(filter)
        || node
            .detail
            .as_ref()
            .is_some_and(|detail| detail.to_lowercase().contains(filter))
}

fn normalized_filter(filter: &str) -> Option<String> {
    let filter = filter.trim().to_lowercase();
    (!filter.is_empty()).then_some(filter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SymbolKind, SymbolNode};

    #[test]
    fn flatten_respects_expansion_when_not_filtering() {
        let mut root = SymbolNode::file(
            "src/main.rs",
            vec![SymbolNode::new(
                "outer",
                SymbolKind::Lsp(12),
                Some(3),
                None,
                vec![SymbolNode::new(
                    "inner",
                    SymbolKind::Lsp(12),
                    Some(4),
                    None,
                    Vec::new(),
                )],
            )],
        );
        root.children[0].expanded = false;

        let visible = flatten_visible(&[root], "");

        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].path, vec![0]);
        assert_eq!(visible[1].path, vec![0, 0]);
    }

    #[test]
    fn filter_includes_matching_descendant_and_ancestors() {
        let root = SymbolNode::file(
            "src/lib.rs",
            vec![SymbolNode::new(
                "parent",
                SymbolKind::Lsp(2),
                Some(1),
                None,
                vec![SymbolNode::new(
                    "needle",
                    SymbolKind::Lsp(12),
                    Some(9),
                    None,
                    Vec::new(),
                )],
            )],
        );

        let visible = flatten_visible(&[root], "needle");

        assert_eq!(
            visible
                .iter()
                .map(|node| node.path.clone())
                .collect::<Vec<_>>(),
            vec![vec![0], vec![0, 0], vec![0, 0, 0]]
        );
        assert!(!visible[0].matched);
        assert!(!visible[1].matched);
        assert!(visible[2].matched);
    }

    #[test]
    fn selection_target_uses_top_level_file_and_selected_line() {
        let root = Path::new("/workspace");
        let nodes = vec![SymbolNode::file(
            "src/lib.rs",
            vec![SymbolNode::new(
                "Thing",
                SymbolKind::Lsp(23),
                Some(42),
                None,
                Vec::new(),
            )],
        )];

        let target = selection_target(root, &nodes, &[0, 0]).expect("selection target");

        assert_eq!(target.file, PathBuf::from("/workspace/src/lib.rs"));
        assert_eq!(target.line, 42);
        assert_eq!(target.label, "Thing");
    }
}
