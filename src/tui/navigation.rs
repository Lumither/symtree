//! Tree navigation and selection: cursor movement, sibling/parent jumps,
//! selection accessors, and restoring the selection across a reload. All
//! methods on `App`.

use ratatui::widgets::ListState;

use super::App;
use crate::tree::{SelectionTarget, get_node, get_node_mut, selection_target};

impl App {
    pub(super) fn clamp_selection(&mut self) {
        let len = self.visible_nodes().len();
        if len == 0 {
            self.selected = 0;
            self.list_state.select(None);
        } else {
            self.selected = self.selected.min(len - 1);
            self.list_state.select(Some(self.selected));
        }
    }

    pub(super) fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(super) fn move_down(&mut self) {
        let len = self.visible_nodes().len();
        if len > 0 {
            self.selected = (self.selected + 1).min(len - 1);
        }
    }

    pub(super) fn move_by(&mut self, delta: isize) {
        let len = self.visible_nodes().len();
        if len == 0 {
            self.selected = 0;
            return;
        }

        let next = self
            .selected
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
        self.selected = next;
    }

    pub(super) fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(10);
    }

    pub(super) fn page_down(&mut self) {
        let len = self.visible_nodes().len();
        if len > 0 {
            self.selected = (self.selected + 10).min(len - 1);
        }
    }

    pub(super) fn move_home(&mut self) {
        self.selected = 0;
    }

    pub(super) fn move_end(&mut self) {
        let len = self.visible_nodes().len();
        if len > 0 {
            self.selected = len - 1;
        }
    }

    pub(super) fn toggle_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        if let Some(node) = get_node_mut(&mut self.project.files, &path)
            && node.has_children()
        {
            node.expanded = !node.expanded;
            self.invalidate_visible();
        }
    }

    pub(super) fn move_to_parent(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };

        if path.len() <= 1 {
            return;
        }

        let parent = path[..path.len() - 1].to_vec();
        let index = self
            .visible_nodes()
            .iter()
            .position(|candidate| candidate.path == parent);
        if let Some(index) = index {
            self.selected = index;
        }
    }

    pub(super) fn move_to_first_child(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };

        if let Some(node) = get_node_mut(&mut self.project.files, &path)
            && node.has_children()
        {
            node.expanded = true;
            self.invalidate_visible();
        }

        let index = {
            let visible = self.visible_nodes();
            let Some(current) = visible.get(self.selected) else {
                return;
            };
            let child_depth = current.depth + 1;
            visible
                .iter()
                .enumerate()
                .skip(self.selected + 1)
                .take_while(|(_, node)| node.depth > current.depth)
                .find_map(|(index, node)| (node.depth == child_depth).then_some(index))
        };
        if let Some(index) = index {
            self.selected = index;
        }
    }

    pub(super) fn move_to_previous_sibling(&mut self) {
        let index = {
            let visible = self.visible_nodes();
            let Some(current) = visible.get(self.selected) else {
                return;
            };
            let current_path = current.path.clone();
            visible[..self.selected]
                .iter()
                .rposition(|node| has_same_parent(&node.path, &current_path))
        };
        if let Some(index) = index {
            self.selected = index;
        }
    }

    pub(super) fn move_to_next_sibling(&mut self) {
        let offset = {
            let visible = self.visible_nodes();
            let Some(current) = visible.get(self.selected) else {
                return;
            };
            let current_path = current.path.clone();
            visible[self.selected + 1..]
                .iter()
                .position(|node| has_same_parent(&node.path, &current_path))
        };
        if let Some(offset) = offset {
            self.selected += offset + 1;
        }
    }

    pub(super) fn selected_path(&self) -> Option<Vec<usize>> {
        self.visible_nodes()
            .get(self.selected)
            .map(|node| node.path.clone())
    }

    pub(super) fn selected_node(&self) -> Option<&crate::model::SymbolNode> {
        let path = self.selected_path()?;
        get_node(&self.project.files, &path)
    }

    pub(super) fn selected_target(&self) -> Option<SelectionTarget> {
        let path = self.selected_path()?;
        selection_target(&self.root, &self.project.files, &path)
    }

    pub(super) fn restore_selection(
        &mut self,
        previous_path: Option<&[usize]>,
        previous_row: usize,
    ) {
        enum Restored {
            Empty,
            Index(usize),
            Fallback(usize),
        }
        let action = {
            let visible = self.visible_nodes();
            if visible.is_empty() {
                Restored::Empty
            } else if let Some(path) = previous_path
                && let Some(index) = visible.iter().position(|node| node.path.as_slice() == path)
            {
                Restored::Index(index)
            } else if let Some(path) = previous_path {
                let mut found = None;
                for len in (1..path.len()).rev() {
                    let ancestor = &path[..len];
                    if let Some(index) = visible
                        .iter()
                        .position(|node| node.path.as_slice() == ancestor)
                    {
                        found = Some(index);
                        break;
                    }
                }
                match found {
                    Some(i) => Restored::Index(i),
                    None => Restored::Fallback(previous_row.min(visible.len() - 1)),
                }
            } else {
                Restored::Fallback(previous_row.min(visible.len() - 1))
            }
        };
        match action {
            Restored::Empty => {
                self.selected = 0;
                self.list_state = ListState::default();
            }
            Restored::Index(i) | Restored::Fallback(i) => self.selected = i,
        }
    }
}

fn has_same_parent(left: &[usize], right: &[usize]) -> bool {
    left.len() == right.len()
        && left.split_last().map(|(_, parent)| parent).unwrap_or(&[])
            == right.split_last().map(|(_, parent)| parent).unwrap_or(&[])
}
