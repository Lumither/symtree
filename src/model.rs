use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub(crate) struct ProjectSymbols {
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

/// Serialized into the on-disk index; `expanded` is UI state, so it is skipped
/// and restored to its default on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SymbolNode {
    pub name: String,
    pub kind: SymbolKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SymbolNode>,
    #[serde(skip, default = "expanded_default")]
    pub expanded: bool,
}

fn expanded_default() -> bool {
    true
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SymbolKind {
    File,
    Lsp(LspSymbolKind),
}

impl SymbolKind {
    pub fn from_lsp(value: u64) -> Self {
        Self::Lsp(LspSymbolKind::from_u64(value))
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Lsp(kind) => kind.short_label(),
        }
    }
}

/// The LSP `SymbolKind` enumeration (LSP spec, values 1..=26), with an `Unknown`
/// arm for any value outside the spec. Modeling it as a closed enum keeps the
/// label and color mappings exhaustive and compiler-checked instead of scattering
/// magic numbers across modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum LspSymbolKind {
    File = 1,
    Module = 2,
    Namespace = 3,
    Package = 4,
    Class = 5,
    Method = 6,
    Property = 7,
    Field = 8,
    Constructor = 9,
    Enum = 10,
    Interface = 11,
    Function = 12,
    Variable = 13,
    Constant = 14,
    String = 15,
    Number = 16,
    Boolean = 17,
    Array = 18,
    Object = 19,
    Key = 20,
    Null = 21,
    EnumMember = 22,
    Struct = 23,
    Event = 24,
    Operator = 25,
    TypeParameter = 26,
    Unknown = 0,
}

impl LspSymbolKind {
    /// Every spec-defined kind, in protocol order. `Unknown` is excluded: it is a
    /// fallback for out-of-spec values, not a selectable kind.
    pub const ALL: [LspSymbolKind; 26] = [
        Self::File,
        Self::Module,
        Self::Namespace,
        Self::Package,
        Self::Class,
        Self::Method,
        Self::Property,
        Self::Field,
        Self::Constructor,
        Self::Enum,
        Self::Interface,
        Self::Function,
        Self::Variable,
        Self::Constant,
        Self::String,
        Self::Number,
        Self::Boolean,
        Self::Array,
        Self::Object,
        Self::Key,
        Self::Null,
        Self::EnumMember,
        Self::Struct,
        Self::Event,
        Self::Operator,
        Self::TypeParameter,
    ];

    pub fn from_u64(value: u64) -> Self {
        match value {
            1 => Self::File,
            2 => Self::Module,
            3 => Self::Namespace,
            4 => Self::Package,
            5 => Self::Class,
            6 => Self::Method,
            7 => Self::Property,
            8 => Self::Field,
            9 => Self::Constructor,
            10 => Self::Enum,
            11 => Self::Interface,
            12 => Self::Function,
            13 => Self::Variable,
            14 => Self::Constant,
            15 => Self::String,
            16 => Self::Number,
            17 => Self::Boolean,
            18 => Self::Array,
            19 => Self::Object,
            20 => Self::Key,
            21 => Self::Null,
            22 => Self::EnumMember,
            23 => Self::Struct,
            24 => Self::Event,
            25 => Self::Operator,
            26 => Self::TypeParameter,
            _ => Self::Unknown,
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Module => "mod",
            Self::Namespace => "ns",
            Self::Package => "pkg",
            Self::Class => "class",
            Self::Method => "method",
            Self::Property => "prop",
            Self::Field => "field",
            Self::Constructor => "ctor",
            Self::Enum => "enum",
            Self::Interface => "iface",
            Self::Function => "fn",
            Self::Variable => "var",
            Self::Constant => "const",
            Self::String => "str",
            Self::Number => "num",
            Self::Boolean => "bool",
            Self::Array => "array",
            Self::Object => "obj",
            Self::Key => "key",
            Self::Null => "null",
            Self::EnumMember => "member",
            Self::Struct => "struct",
            Self::Event => "event",
            Self::Operator => "op",
            Self::TypeParameter => "type",
            Self::Unknown => "sym",
        }
    }
}

/// Distinct labels accepted by the `kind:` query predicate, derived from the
/// kind enum so completions and matching can never drift apart.
pub(crate) fn kind_labels() -> Vec<&'static str> {
    LspSymbolKind::ALL
        .iter()
        .map(|kind| kind.short_label())
        .collect()
}

fn count_symbols(nodes: &[SymbolNode]) -> usize {
    nodes
        .iter()
        .map(|node| 1 + count_symbols(&node.children))
        .sum()
}
