//! tree-sitter AST/symbol index (M3.3, docs/18 § Index inputs: "tree-sitter
//! AST and symbol definitions"; docs/36: Syntax — DEPEND: tree-sitter).
//! Languages first: Rust, TypeScript/TSX, JavaScript, Python.
//!
//! Per indexed file it stores (a) DEFINITIONS — functions, methods,
//! classes, structs, enums, traits, modules, interfaces, type aliases —
//! parsed by tree-sitter queries, and (b) identifier OCCURRENCES with
//! spans. A reference to a name is the set of its occurrences minus the
//! definition sites, resolved at QUERY time against the current global
//! definition set — so a definition added later immediately resolves
//! references in every file without re-parsing them, and an incremental
//! update and a cold build answer identically (per-file purity).

use std::collections::BTreeMap;

use tree_sitter::{Language, Query, QueryCursor, StreamingIterator, Tree};

/// One symbol definition: identity shared by index and query paths
/// (QUAL gate: cross-language fixture proves identity consistency).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SymbolDef {
    pub name: String,
    /// function | method | class | struct | enum | trait | module |
    /// interface | type_alias
    pub kind: &'static str,
    pub path: String,
    /// 1-based definition line.
    pub line: usize,
}

/// One symbol reference site.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SymbolRef {
    pub path: String,
    /// 1-based reference line.
    pub line: usize,
}

/// Per-file parse result: definitions + identifier occurrences.
#[derive(Debug, Default)]
pub struct FileSymbols {
    pub defs: Vec<SymbolDef>,
    /// identifier name -> 1-based occurrence lines.
    pub occurrences: BTreeMap<String, Vec<usize>>,
}

/// The symbol index over the indexed file set.
#[derive(Debug, Default)]
pub struct SymbolIndex {
    files: BTreeMap<String, FileSymbols>,
}

/// The language a path is parsed with: ("rust" | "typescript" | "tsx" |
/// "javascript" | "python"); None = not parsed (file is still text-indexed).
pub fn language_of(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_lowercase();
    match ext.as_str() {
        "rs" => Some("rust"),
        "ts" => Some("typescript"),
        "tsx" => Some("tsx"),
        "js" | "jsx" | "mjs" | "cjs" => Some("javascript"),
        "py" => Some("python"),
        _ => None,
    }
}

fn tree_for(lang: &str, source: &[u8]) -> Option<Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language_for(lang)?).ok()?;
    parser.parse(source, None)
}

/// TS/TSX definition patterns (shared by both; the grammar parses the
/// other dialect's nodes as well).
const TS_DEF_QUERY: &str = r#"
(function_declaration name: (identifier) @name)
(generator_function_declaration name: (identifier) @name)
(method_definition name: (property_identifier) @name)
(class_declaration name: (type_identifier) @name)
(abstract_class_declaration name: (type_identifier) @name)
(interface_declaration name: (type_identifier) @name)
(type_alias_declaration name: (type_identifier) @name)
(enum_declaration name: (identifier) @name)
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression) (generator_function)])
"#;

/// Definition query patterns per language; the pattern ORDER maps to the
/// kind list (query match pattern_index -> kind).
fn def_query(lang: &str) -> Option<(Query, &'static [&'static str])> {
    let (source, kinds): (&str, &[&str]) = match lang {
        "rust" => (
            r#"
            (function_item name: (identifier) @name)
            (struct_item name: (type_identifier) @name)
            (enum_item name: (type_identifier) @name)
            (trait_item name: (type_identifier) @name)
            (mod_item name: (identifier) @name)
            (type_item name: (type_identifier) @name)
            "#,
            &["function", "struct", "enum", "trait", "module", "type_alias"],
        ),
        "typescript" => (
            TS_DEF_QUERY,
            &["function", "function", "method", "class", "class", "interface", "type_alias", "enum", "function"],
        ),
        "tsx" => (
            TS_DEF_QUERY,
            &["function", "function", "method", "class", "class", "interface", "type_alias", "enum", "function"],
        ),
        "javascript" => (
            r#"
            (function_declaration name: (identifier) @name)
            (generator_function_declaration name: (identifier) @name)
            (method_definition name: (property_identifier) @name)
            (class_declaration name: (identifier) @name)
            (variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression) (generator_function)])
            "#,
            &["function", "function", "method", "class", "function"],
        ),
        "python" => (
            r#"
            (function_definition name: (identifier) @name)
            (class_definition name: (identifier) @name)
            "#,
            &["function", "class"],
        ),
        _ => return None,
    };
    let language = language_for(lang)?;
    let query = Query::new(&language, source).ok()?;
    Some((query, kinds))
}

fn occurrence_query(lang: &str) -> Option<Query> {
    let source = match lang {
        // type_identifier covers type positions Rust stores separately.
        "rust" => r#"[(identifier) (type_identifier)] @id"#,
        _ => r#"(identifier) @id"#,
    };
    Query::new(&language_for(lang)?, source).ok()
}

fn language_for(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        _ => None,
    }
}

/// Parses one file into its symbol surface. Unsupported languages and
/// unparsable sources yield an empty (but present) entry for deletion
/// symmetry; callers index them anyway in the lexical/merkle layers.
pub fn parse_file(path: &str, bytes: &[u8]) -> FileSymbols {
    let mut out = FileSymbols::default();
    let Some(lang) = language_of(path) else {
        return out;
    };
    let Some(tree) = tree_for(lang, bytes) else {
        return out;
    };
    let source: &[u8] = bytes;

    if let Some((query, kinds)) = def_query(lang) {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            let Some(name_node) = m.captures().iter().find(|c| {
                query.capture_names()[c.index as usize] == "name"
            }) else {
                continue;
            };
            let name = name_node.node.utf8_text(source).unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let kind = kinds[m.pattern_index];
            out.defs.push(SymbolDef {
                name,
                kind,
                path: path.to_string(),
                line: name_node.node.start_position().row + 1,
            });
        }
    }

    if let Some(query) = occurrence_query(lang) {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            for cap in m.captures() {
                let name = cap.node.utf8_text(source).unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let line = cap.node.start_position().row + 1;
                let lines = out.occurrences.entry(name).or_default();
                if lines.last() != Some(&line) {
                    lines.push(line);
                }
            }
        }
    }

    out.defs.sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
    out
}

impl SymbolIndex {
    /// Replaces (or installs) one file's symbol surface.
    pub fn index_file(&mut self, path: &str, bytes: &[u8]) {
        self.files.insert(path.to_string(), parse_file(path, bytes));
    }

    /// Removes one file's symbol surface.
    pub fn remove_file(&mut self, path: &str) {
        self.files.remove(path);
    }

    /// All definitions of `name`, ordered by (path, line).
    pub fn definitions(&self, name: &str) -> Vec<SymbolDef> {
        let mut defs: Vec<SymbolDef> = self
            .files
            .values()
            .flat_map(|f| f.defs.iter().filter(|d| d.name == name))
            .cloned()
            .collect();
        defs.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
        defs
    }

    /// References to `name`: every recorded occurrence minus the
    /// definition sites, ordered by (path, line). Cross-file references
    /// resolve against the CURRENT definition set at query time.
    pub fn references(&self, name: &str) -> Vec<SymbolRef> {
        let def_sites: Vec<(&str, usize)> = self
            .files
            .values()
            .flat_map(|f| f.defs.iter().filter(|d| d.name == name))
            .map(|d| (d.path.as_str(), d.line))
            .collect();
        let mut refs: Vec<SymbolRef> = Vec::new();
        for (path, file) in &self.files {
            if let Some(lines) = file.occurrences.get(name) {
                for line in lines {
                    if def_sites.contains(&(path.as_str(), *line)) {
                        continue;
                    }
                    refs.push(SymbolRef {
                        path: path.clone(),
                        line: *line,
                    });
                }
            }
        }
        refs.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
        refs.dedup();
        refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUAL gate for the symbol surface: a cross-language fixture proves
    /// the SAME SymbolDef identity (name, kind, path, 1-based line)
    /// semantics hold for Rust, TypeScript, TSX, JavaScript, and Python,
    /// and that references resolve across files and languages.
    #[test]
    fn cross_language_symbol_identity_is_consistent() {
        let files: Vec<(&str, &[u8])> = vec![
            (
                "src/telemetry.rs",
                b"pub fn emit_telemetry(event: &str) -> bool {\n    event.len() > 0\n}\n\npub struct TelemetrySink {\n    pub buffered: bool,\n}\n",
            ),
            (
                "src/telemetry.ts",
                b"export function emitTelemetry(event: string): boolean {\n  return event.length > 0;\n}\n\nexport interface TelemetrySink {\n  buffered: boolean;\n}\n",
            ),
            (
                "src/telemetry.tsx",
                b"export function emitTelemetry(event: string): boolean {\n  return event.length > 0;\n}\n",
            ),
            (
                "src/telemetry_js.js",
                b"function emitTelemetry(event) {\n  return emitTelemetry(event);\n}\n",
            ),
            (
                "src/telemetry.py",
                b"def emit_telemetry(event):\n    return emit_telemetry(event)\n\nclass TelemetrySink:\n    buffered = True\n",
            ),
            // A caller file per language so references have somewhere to land.
            (
                "src/caller.rs",
                b"fn main() {\n    let ok = emit_telemetry(\"boot\");\n    let _ = ok;\n}\n",
            ),
            (
                "src/caller.py",
                b"if __name__ == '__main__':\n    emit_telemetry('boot')\n",
            ),
        ];

        let mut index = SymbolIndex::default();
        for (path, bytes) in &files {
            index.index_file(path, bytes);
        }

        // Definitions: same name, per-language kind + path + 1-based line.
        let rust = index.definitions("emit_telemetry");
        assert_eq!(rust.len(), 2, "rust + python defs: {rust:?}");
        assert!(rust.contains(&SymbolDef {
            name: "emit_telemetry".into(),
            kind: "function",
            path: "src/telemetry.rs".into(),
            line: 1,
        }));
        assert!(rust.contains(&SymbolDef {
            name: "emit_telemetry".into(),
            kind: "function",
            path: "src/telemetry.py".into(),
            line: 1,
        }));

        let ts = index.definitions("emitTelemetry");
        assert_eq!(ts.len(), 3, "ts + tsx + js defs: {ts:?}");
        assert!(ts.iter().all(|d| d.kind == "function" && d.line == 1));

        // Struct/class identity across languages.
        assert!(index.definitions("TelemetrySink").iter().any(|d| d.kind == "struct" && d.path == "src/telemetry.rs" && d.line == 5));
        assert!(index.definitions("TelemetrySink").iter().any(|d| d.kind == "class" && d.path == "src/telemetry.py" && d.line == 4));
        assert!(index.definitions("TelemetrySink").iter().any(|d| d.kind == "interface" && d.path == "src/telemetry.ts" && d.line == 5));

        // References: callers resolve, definition sites are excluded, and
        // recursion (self-reference) is a reference, not lost.
        let rust_refs = index.references("emit_telemetry");
        assert!(rust_refs.contains(&SymbolRef { path: "src/caller.rs".into(), line: 2 }));
        assert!(!rust_refs.contains(&SymbolRef { path: "src/telemetry.rs".into(), line: 1 }), "definition site excluded");

        let py_refs = index.references("emit_telemetry");
        assert!(py_refs.contains(&SymbolRef { path: "src/telemetry.py".into(), line: 2 }));
        assert!(py_refs.contains(&SymbolRef { path: "src/caller.py".into(), line: 2 }));

        // Unknown names are empty on both axes.
        assert!(index.definitions("nope").is_empty());
        assert!(index.references("nope").is_empty());

        // A definition added to a NEW file immediately resolves
        // references that were indexed BEFORE the definition existed
        // (occurrence-span purity: refs are a query-time view).
        index.index_file("src/late.rs", b"fn late_user() {\n    let _ = emit_telemetry(\"late\");\n}\n");
        index.index_file("src/def_holder.rs", b"fn emit_telemetry(x: u8) -> u8 { x }\n");
        let defs = index.definitions("emit_telemetry");
        assert!(defs.iter().any(|d| d.path == "src/def_holder.rs" && d.line == 1));
        assert!(index.references("emit_telemetry").iter().any(|r| r.path == "src/late.rs" && r.line == 2));

        // Removing a file drops its surface from both axes.
        index.remove_file("src/telemetry.py");
        assert!(!index.definitions("emit_telemetry").iter().any(|d| d.path == "src/telemetry.py"));
        assert!(!index.references("emit_telemetry").iter().any(|r| r.path == "src/telemetry.py" && r.line == 2), "removed file's self-use gone");
        assert!(index.references("emit_telemetry").iter().any(|r| r.path == "src/caller.py"), "unrelated files keep their references");
    }

    /// Unsupported languages parse to an empty surface (file stays
    /// text-indexed elsewhere); syntax errors never panic.
    #[test]
    fn unsupported_and_broken_sources_are_tolerated() {
        assert!(language_of("x.md").is_none());
        assert!(parse_file("x.md", b"plain").defs.is_empty());
        // Broken Rust: parser recovers; no panic either way.
        let _ = parse_file("broken.rs", b"fn ))){{{");
        // Empty source.
        let empty = parse_file("empty.ts", b"");
        assert!(empty.defs.is_empty());
    }
}
