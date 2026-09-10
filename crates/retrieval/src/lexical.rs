//! Tantivy-backed BM25 lexical index (M3.2, docs/18 § Concrete local
//! indexing stack: "BM25: Tantivy index per repository snapshot family";
//! docs/36: Lexical search — DEPEND: Tantivy). One in-RAM Tantivy index
//! per task index; one document per indexed file, incrementally replaced
//! by path term deletion on every refresh. Query syntax errors yield no
//! hits — lexical ranking is one fused source among several, never a
//! correctness path (docs/18: exact/BM25/AST remain the fallback stack).

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::document::Value;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, TEXT,
};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

/// The lexical index over indexed file bodies.
pub struct LexicalIndex {
    index: Index,
    writer: IndexWriter,
    path_field: Field,
    body_field: Field,
}

impl LexicalIndex {
    pub fn new() -> Result<Self, tantivy::TantivyError> {
        let mut builder = Schema::builder();
        // Path: raw-tokenized (keyword) so term deletion targets exact files.
        let path_field = builder.add_text_field(
            "path",
            TextOptions::default()
                .set_stored()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("raw")
                        .set_index_option(IndexRecordOption::Basic),
                ),
        );
        // Body: default tokenization, BM25 scoring (freqs + positions).
        let body_field = builder.add_text_field("body", TEXT);
        let schema = builder.build();
        let index = Index::create_in_ram(schema.clone());
        let writer = index.writer(20_000_000)?;
        Ok(LexicalIndex {
            index,
            writer,
            path_field,
            body_field,
        })
    }

    /// Replaces one file's document (delete by exact path term + add).
    pub fn replace(&mut self, path: &str, bytes: &[u8]) {
        self.writer
            .delete_term(Term::from_field_text(self.path_field, path));
        let mut doc = TantivyDocument::default();
        doc.add_text(self.path_field, path);
        doc.add_text(self.body_field, String::from_utf8_lossy(bytes).as_ref());
        let _ = self.writer.add_document(doc);
    }

    /// Removes one file's document.
    pub fn remove(&mut self, path: &str) {
        self.writer
            .delete_term(Term::from_field_text(self.path_field, path));
    }

    /// Commits the pending delta batch so searchers see it.
    pub fn commit(&mut self) {
        let _ = self.writer.commit();
    }

    /// Ranked BM25 search: (path, score) descending by score, ties broken
    /// by path for determinism. Unparsable queries return no hits.
    pub fn search(&self, query: &str, limit: usize) -> Vec<(String, f64)> {
        if query.trim().is_empty() {
            return Vec::new();
        }
        let Ok(reader) = self.index.reader() else { return Vec::new() };
        let searcher = reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.body_field]);
        let Ok(parsed) = parser.parse_query(query) else {
            return Vec::new();
        };
        let Ok(top) = searcher.search(&parsed, &TopDocs::with_limit(limit).order_by_score()) else {
            return Vec::new();
        };
        let mut hits: Vec<(String, f64)> = top
            .into_iter()
            .filter_map(|(score, addr)| {
                let doc: TantivyDocument = searcher.doc(addr).ok()?;
                let path = doc.get_first(self.path_field)?.as_str()?.to_string();
                Some((path, score as f64))
            })
            .collect();
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        hits.dedup_by(|a, b| a.0 == b.0);
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> LexicalIndex {
        let mut idx = LexicalIndex::new().expect("ram index");
        idx.replace(
            "src/retry.rs",
            b"retry backoff timeout transient failure retry policy\n",
        );
        idx.replace("src/ui/button.rs", b"button click render focus style widget\n");
        idx.replace(
            "docs/retry-notes.md",
            b"retry notes about the retry mechanism and its backoff\n",
        );
        idx.commit();
        idx
    }

    /// M3.2: BM25 ranking through the real Tantivy engine — rare terms
    /// carry the score, no-hit queries are empty, and an incremental
    /// replace yields the same ranking as a cold index over the same set.
    #[test]
    fn tantivy_bm25_ranks_and_replaces_incrementally() {
        let idx = index();
        let hits = idx.search("retry backoff", 10);
        assert_eq!(hits[0].0, "src/retry.rs", "both terms in one doc wins: {hits:?}");
        assert!(hits.iter().all(|(p, _)| p != "src/ui/button.rs"));
        assert!(idx.search("xylophone", 10).is_empty());

        // Deterministic tie-break: identical scores sort by path.
        let mut tie = LexicalIndex::new().unwrap();
        tie.replace("b.txt", b"alpha beta");
        tie.replace("a.txt", b"alpha beta");
        tie.commit();
        let hits = tie.search("alpha", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, "a.txt");
        assert!((hits[0].1 - hits[1].1).abs() < f64::EPSILON);

        // Incremental replace == cold rebuild of the same document set.
        let mut inc = index();
        inc.replace("src/retry.rs", b"completely different words now\n");
        inc.commit();
        let mut cold = LexicalIndex::new().unwrap();
        cold.replace(
            "src/retry.rs",
            b"completely different words now\n",
        );
        cold.replace("src/ui/button.rs", b"button click render focus style widget\n");
        cold.replace(
            "docs/retry-notes.md",
            b"retry notes about the retry mechanism and its backoff\n",
        );
        cold.commit();
        assert_eq!(
            inc.search("completely words", 10)
                .into_iter()
                .map(|(p, _)| p)
                .collect::<Vec<_>>(),
            cold.search("completely words", 10)
                .into_iter()
                .map(|(p, _)| p)
                .collect::<Vec<_>>(),
        );
        assert!(inc.search("policy", 10).is_empty(), "replaced body gone (policy was unique to it)");

        // Removal drops the document.
        let mut rm = index();
        rm.remove("docs/retry-notes.md");
        rm.commit();
        assert!(rm.search("mechanism", 10).is_empty());

        // Unparsable query syntax is empty, never a panic.
        assert!(index().search("(unclosed AND", 10).is_empty());
    }

    /// The document map stays unique per path: replacing the same path
    /// twice never duplicates hits (BTreeMap-style identity contract used
    /// by the fusion layer).
    #[test]
    fn replace_keeps_one_doc_per_path() {
        let mut idx = LexicalIndex::new().unwrap();
        idx.replace("f.rs", b"word one\n");
        idx.replace("f.rs", b"word two\n");
        idx.commit();
        let hits = idx.search("word", 10);
        assert_eq!(hits.len(), 1, "one doc per path");
        assert!(idx.search("two", 10).len() == 1);
    }
}
