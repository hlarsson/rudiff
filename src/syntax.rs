//! Tree-sitter integration: syntax highlighting, enclosing-function context for
//! hunk headers, and defined-symbol extraction (used by the related-files
//! panel). All of it degrades gracefully — an unknown language or a parse
//! failure simply yields no highlighting / no context, never an error.

use std::collections::HashMap;

use ratatui::style::Style;
use tree_sitter::{Node, Parser, Point};
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use crate::theme::Theme;

/// A styled run of text within a single source line.
pub type Seg = (String, Style);

/// Skip highlighting for very large files — it isn't worth the latency and the
/// payoff (visible region) is tiny.
const MAX_HIGHLIGHT_BYTES: usize = 2 * 1024 * 1024;

/// Capture names we recognize. Tree-sitter maps each query capture (e.g.
/// `function.method`) to the longest matching entry here, so listing the base
/// names is enough.
const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constructor",
    "escape",
    "function",
    "keyword",
    "label",
    "module",
    "number",
    "operator",
    "property",
    "punctuation",
    "string",
    "string.special",
    "tag",
    "type",
    "variable",
];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Lang {
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Go,
    Java,
}

impl Lang {
    /// Detect a language from a file path's extension.
    pub fn from_path(path: &std::path::Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "js" | "jsx" | "mjs" | "cjs" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "java" => Lang::Java,
            _ => return None,
        })
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
        }
    }

    fn highlights_query(self) -> &'static str {
        match self {
            Lang::Rust => tree_sitter_rust::HIGHLIGHTS_QUERY,
            Lang::JavaScript => tree_sitter_javascript::HIGHLIGHT_QUERY,
            Lang::TypeScript | Lang::Tsx => tree_sitter_typescript::HIGHLIGHTS_QUERY,
            Lang::Python => tree_sitter_python::HIGHLIGHTS_QUERY,
            Lang::Go => tree_sitter_go::HIGHLIGHTS_QUERY,
            Lang::Java => tree_sitter_java::HIGHLIGHTS_QUERY,
        }
    }

    /// Node kinds that form an enclosing "context" (function/type/etc.) for
    /// hunk headers, in priority order from container to function.
    fn context_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[
                "impl_item",
                "trait_item",
                "struct_item",
                "enum_item",
                "mod_item",
                "function_item",
            ],
            Lang::Python => &["class_definition", "function_definition"],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &[
                "class_declaration",
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
            ],
            Lang::Go => &[
                "type_declaration",
                "function_declaration",
                "method_declaration",
            ],
            Lang::Java => &[
                "class_declaration",
                "interface_declaration",
                "enum_declaration",
                "method_declaration",
                "constructor_declaration",
            ],
        }
    }

    /// Node kinds representing a call/invocation, for the `calls` verb.
    fn call_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["call_expression", "macro_invocation"],
            Lang::Python => &["call"],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &["call_expression"],
            Lang::Go => &["call_expression"],
            Lang::Java => &["method_invocation"],
        }
    }

    /// Node kinds representing an import/use, for the `imports` verb.
    fn import_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["use_declaration"],
            Lang::Python => &["import_statement", "import_from_statement"],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &["import_statement"],
            Lang::Go => &["import_declaration", "import_spec"],
            Lang::Java => &["import_declaration"],
        }
    }

    /// Node kinds that define a named symbol (for the related-files index).
    fn definition_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[
                "function_item",
                "struct_item",
                "enum_item",
                "trait_item",
                "type_item",
                "const_item",
                "static_item",
                "macro_definition",
                "mod_item",
                "union_item",
            ],
            Lang::Python => &["function_definition", "class_definition"],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &[
                "function_declaration",
                "generator_function_declaration",
                "class_declaration",
                "method_definition",
                "interface_declaration",
                "type_alias_declaration",
                "enum_declaration",
            ],
            Lang::Go => &[
                "function_declaration",
                "method_declaration",
                "type_spec",
                "type_declaration",
            ],
            Lang::Java => &[
                "class_declaration",
                "interface_declaration",
                "enum_declaration",
                "method_declaration",
                "record_declaration",
            ],
        }
    }
}

/// Map a context node kind to a short keyword for the hunk-header label.
fn kind_keyword(kind: &str) -> &'static str {
    match kind {
        "impl_item" => "impl",
        "trait_item" | "interface_declaration" => "trait",
        "struct_item" | "record_declaration" => "struct",
        "enum_item" | "enum_declaration" => "enum",
        "mod_item" => "mod",
        "class_definition" | "class_declaration" => "class",
        "type_declaration" | "type_spec" | "type_alias_declaration" => "type",
        "function_item"
        | "function_declaration"
        | "generator_function_declaration"
        | "function_definition" => "fn",
        "method_definition" | "method_declaration" | "constructor_declaration" => "fn",
        _ => "",
    }
}

/// Holds the (lazily-built, cached) highlight configs and reusable engines.
pub struct Syntax {
    highlighter: Highlighter,
    configs: HashMap<Lang, Option<HighlightConfiguration>>,
    parser: Parser,
    /// Precomputed styles aligned to `HIGHLIGHT_NAMES`.
    styles: Vec<Style>,
}

impl Syntax {
    pub fn new(theme: &Theme) -> Syntax {
        let styles = HIGHLIGHT_NAMES
            .iter()
            .map(|n| theme.syntax_style(n))
            .collect();
        Syntax {
            highlighter: Highlighter::new(),
            configs: HashMap::new(),
            parser: Parser::new(),
            styles,
        }
    }

    /// Highlight `text`, returning per-source-line styled segments. `None` if
    /// the language is unknown, the file is too large, or highlighting fails.
    pub fn highlight(&mut self, text: &str, lang: Lang) -> Option<Vec<Vec<Seg>>> {
        if text.len() > MAX_HIGHLIGHT_BYTES {
            return None;
        }
        // Ensure the config is cached, then borrow `configs` and `highlighter`
        // as disjoint fields (a single `&mut self` method call can't do both).
        self.configs
            .entry(lang)
            .or_insert_with(|| build_config(lang));
        let config = self.configs.get(&lang).and_then(Option::as_ref)?;
        let styles = &self.styles;
        let src = text.as_bytes();

        let events = self
            .highlighter
            .highlight(config, src, None, |_| None)
            .ok()?;

        let mut lines: Vec<Vec<Seg>> = vec![Vec::new()];
        let mut stack: Vec<Style> = Vec::new();
        for event in events {
            match event.ok()? {
                HighlightEvent::HighlightStart(h) => {
                    stack.push(styles.get(h.0).copied().unwrap_or_default());
                }
                HighlightEvent::HighlightEnd => {
                    stack.pop();
                }
                HighlightEvent::Source { start, end } => {
                    let style = stack.last().copied().unwrap_or_default();
                    let piece = &text[start..end];
                    let mut first = true;
                    for part in piece.split('\n') {
                        if !first {
                            lines.push(Vec::new());
                        }
                        first = false;
                        if !part.is_empty() {
                            lines.last_mut().unwrap().push((part.to_string(), style));
                        }
                    }
                }
            }
        }
        Some(lines)
    }

    fn parse(&mut self, text: &str, lang: Lang) -> Option<tree_sitter::Tree> {
        self.parser.set_language(&lang.language()).ok()?;
        self.parser.parse(text, None)
    }

    /// Compute enclosing-context labels (e.g. `impl Store::refresh`) for the
    /// given 0-based new-file rows, parsing the head text only once.
    pub fn contexts(&mut self, text: &str, lang: Lang, rows: &[usize]) -> Vec<Option<String>> {
        let Some(tree) = self.parse(text, lang) else {
            return vec![None; rows.len()];
        };
        let src = text.as_bytes();
        rows.iter()
            .map(|&r| context_at(&tree, src, lang, r))
            .collect()
    }

    /// Extract the facts used to relate files: the symbols this file defines,
    /// and the identifiers it uses / calls / imports.
    pub fn analyze(&mut self, text: &str, lang: Lang) -> FileFacts {
        let mut facts = FileFacts::default();
        let Some(tree) = self.parse(text, lang) else {
            return facts;
        };
        let src = text.as_bytes();
        walk_facts(tree.root_node(), src, lang, &mut facts);
        facts.defines.sort_unstable();
        facts.defines.dedup();
        facts
    }
}

/// Symbols a file defines plus the identifiers it uses, with calls and imports
/// distinguished so the related panel can show a verb.
#[derive(Default, Clone)]
pub struct FileFacts {
    pub defines: Vec<String>,
    pub uses: std::collections::HashSet<String>,
    pub calls: std::collections::HashSet<String>,
    pub imports: std::collections::HashSet<String>,
}

/// How verb-ranked references describe a related file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Calls,
    Imports,
    References,
}

impl Verb {
    pub fn label(self) -> &'static str {
        match self {
            Verb::Calls => "calls",
            Verb::Imports => "imports",
            Verb::References => "references",
        }
    }
}

/// Single recursive pass collecting defines/uses/calls/imports.
fn walk_facts(node: Node, src: &[u8], lang: Lang, facts: &mut FileFacts) {
    let kind = node.kind();

    if lang.definition_kinds().contains(&kind)
        && let Some(name) = node_name(node, src)
    {
        facts.defines.push(name);
    }
    if kind.ends_with("identifier")
        && let Ok(t) = node.utf8_text(src)
    {
        facts.uses.insert(t.to_string());
    }
    if lang.call_kinds().contains(&kind)
        && let Some(name) = callee_name(node, src)
    {
        facts.calls.insert(name);
    }
    if lang.import_kinds().contains(&kind) {
        collect_identifiers(node, src, &mut facts.imports);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_facts(child, src, lang, facts);
    }
}

/// The name of the function/method being called: the last identifier within
/// the callee expression (handles `a.b.c()` → `c`, `Type::method()` → `method`).
fn callee_name(call: Node, src: &[u8]) -> Option<String> {
    let callee = call
        .child_by_field_name("function")
        .or_else(|| call.child_by_field_name("name"))
        .unwrap_or(call);
    // The called name is the *rightmost* identifier in the callee expression:
    // `a.b.c()` → `c`, `Type::method()` → `method`.
    let mut best: Option<(usize, String)> = None;
    let mut cursor = callee.walk();
    let mut stack = vec![callee];
    while let Some(n) = stack.pop() {
        if n.kind().ends_with("identifier")
            && let Ok(t) = n.utf8_text(src)
        {
            let start = n.start_byte();
            if best.as_ref().is_none_or(|(b, _)| start >= *b) {
                best = Some((start, t.to_string()));
            }
        }
        for child in n.children(&mut cursor) {
            stack.push(child);
        }
    }
    best.map(|(_, name)| name)
}

fn collect_identifiers(node: Node, src: &[u8], out: &mut std::collections::HashSet<String>) {
    if node.kind().ends_with("identifier")
        && let Ok(t) = node.utf8_text(src)
    {
        out.insert(t.to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifiers(child, src, out);
    }
}

/// Enclosing-context label for a row given an already-parsed tree.
fn context_at(tree: &tree_sitter::Tree, src: &[u8], lang: Lang, row: usize) -> Option<String> {
    let point = Point::new(row, 0);
    let mut node = tree.root_node().descendant_for_point_range(point, point)?;
    let kinds = lang.context_kinds();
    let mut chain: Vec<(String, &str)> = Vec::new();
    loop {
        if kinds.contains(&node.kind())
            && let Some(name) = node_name(node, src)
        {
            chain.push((name, node.kind()));
        }
        match node.parent() {
            Some(p) => node = p,
            None => break,
        }
        if chain.len() >= 3 {
            break;
        }
    }
    if chain.is_empty() {
        return None;
    }
    chain.reverse(); // outermost first
    let kw = kind_keyword(chain[0].1);
    let names: Vec<&str> = chain.iter().map(|(n, _)| n.as_str()).collect();
    let joined = names.join("::");
    Some(if kw.is_empty() {
        joined
    } else {
        format!("{kw} {joined}")
    })
}

fn build_config(lang: Lang) -> Option<HighlightConfiguration> {
    let mut config =
        HighlightConfiguration::new(lang.language(), "", lang.highlights_query(), "", "").ok()?;
    config.configure(HIGHLIGHT_NAMES);
    Some(config)
}

/// Extract a node's name: prefer the `name` field, then `type` (Rust `impl`),
/// then the first identifier-ish child.
fn node_name(node: Node, src: &[u8]) -> Option<String> {
    for field in ["name", "type"] {
        if let Some(child) = node.child_by_field_name(field)
            && let Ok(t) = child.utf8_text(src)
        {
            return Some(t.to_string());
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if (k.contains("identifier") || k == "type_identifier")
            && let Ok(t) = child.utf8_text(src)
        {
            return Some(t.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_languages() {
        use std::path::Path;
        assert_eq!(Lang::from_path(Path::new("a/b.rs")), Some(Lang::Rust));
        assert_eq!(Lang::from_path(Path::new("x.tsx")), Some(Lang::Tsx));
        assert_eq!(Lang::from_path(Path::new("README.md")), None);
    }

    #[test]
    fn extracts_rust_symbols() {
        let theme = Theme::detect();
        let mut s = Syntax::new(&theme);
        let src = "struct Session;\nfn refresh_session() {}\nimpl Session { fn go(&self) {} }\n";
        let syms = s.analyze(src, Lang::Rust).defines;
        assert!(syms.contains(&"Session".to_string()));
        assert!(syms.contains(&"refresh_session".to_string()));
        assert!(syms.contains(&"go".to_string()));
    }

    #[test]
    fn function_context_for_rust() {
        let theme = Theme::detect();
        let mut s = Syntax::new(&theme);
        let src = "impl Store {\n    fn refresh(&self) {\n        let x = 1;\n    }\n}\n";
        // Row 2 (0-based) is `let x = 1;`, inside fn refresh inside impl Store.
        let ctx = s.contexts(src, Lang::Rust, &[2])[0].clone().unwrap();
        assert!(ctx.contains("refresh"), "got: {ctx}");
        assert!(ctx.contains("Store"), "got: {ctx}");
    }

    #[test]
    fn highlights_without_panicking() {
        let theme = Theme::detect();
        let mut s = Syntax::new(&theme);
        let lines = s
            .highlight("fn main() { let x = 1; }\n", Lang::Rust)
            .unwrap();
        assert!(!lines.is_empty());
        // The first line should contain at least one styled segment.
        assert!(
            lines[0]
                .iter()
                .any(|(t, _)| t.contains("fn") || t.contains("main"))
        );
    }
}
