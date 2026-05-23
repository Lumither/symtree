use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ProjectSymbols {
    pub root: PathBuf,
    pub files: Vec<SymbolNode>,
    pub warnings: Vec<String>,
}

impl ProjectSymbols {
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn symbol_count(&self) -> usize {
        self.files
            .iter()
            .map(|file| count_symbols(&file.children))
            .sum()
    }
}

#[derive(Debug, Clone)]
pub struct SymbolNode {
    pub name: String,
    pub kind: SymbolKind,
    pub line: Option<usize>,
    pub detail: Option<String>,
    pub children: Vec<SymbolNode>,
    pub expanded: bool,
}

impl SymbolNode {
    pub fn new(
        name: impl Into<String>,
        kind: SymbolKind,
        line: Option<usize>,
        detail: Option<String>,
        children: Vec<SymbolNode>,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            line,
            detail,
            children,
            expanded: true,
        }
    }

    pub fn file(name: impl Into<String>, children: Vec<SymbolNode>) -> Self {
        let mut node = Self::new(name, SymbolKind::File, None, None, children);
        node.expanded = false;
        node
    }

    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }

    pub fn descendant_count(&self) -> usize {
        count_symbols(&self.children)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    File,
    Lsp(u64),
}

impl SymbolKind {
    pub fn from_lsp(value: u64) -> Self {
        Self::Lsp(value)
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Lsp(1) => "file",
            Self::Lsp(2) => "mod",
            Self::Lsp(3) => "ns",
            Self::Lsp(4) => "pkg",
            Self::Lsp(5) => "class",
            Self::Lsp(6) => "method",
            Self::Lsp(7) => "prop",
            Self::Lsp(8) => "field",
            Self::Lsp(9) => "ctor",
            Self::Lsp(10) => "enum",
            Self::Lsp(11) => "iface",
            Self::Lsp(12) => "fn",
            Self::Lsp(13) => "var",
            Self::Lsp(14) => "const",
            Self::Lsp(15) => "str",
            Self::Lsp(16) => "num",
            Self::Lsp(17) => "bool",
            Self::Lsp(18) => "array",
            Self::Lsp(19) => "obj",
            Self::Lsp(20) => "key",
            Self::Lsp(21) => "null",
            Self::Lsp(22) => "member",
            Self::Lsp(23) => "struct",
            Self::Lsp(24) => "event",
            Self::Lsp(25) => "op",
            Self::Lsp(26) => "type",
            Self::Lsp(_) => "sym",
        }
    }
}

fn count_symbols(nodes: &[SymbolNode]) -> usize {
    nodes
        .iter()
        .map(|node| 1 + count_symbols(&node.children))
        .sum()
}
