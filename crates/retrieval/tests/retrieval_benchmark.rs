//! Retrieval benchmark at a fixed revision (Future-tasks Phase 3 item 4,
//! docs/53 § Retrieval comparison + § Engineering benchmarks; M3.9): a
//! DETERMINISTIC corpus (seeded generator, fixed revision tag) built
//! through the REAL indexing pipeline (walker → tantivy BM25 +
//! tree-sitter symbols → fused context_query), queried with
//! ground-truth-by-construction suites, comparing
//!   profile A — exact/substring baseline (the search.grep class),
//!   profile F — Modbit's fused index (BM25 + path + symbol).
//! Gates (always-on, this test IS the CI gate):
//!   recall@5(F) ≥ recall@5(A)  — the fused index beats/matches exact,
//!   recall@5(F) ≥ 0.90         — the index actually retrieves,
//!   warm hybrid p95 < 300 ms   — docs/53 L1 warm bound (small repo).
//! Cold index time is REPORTED (docs/53 records it, no CI gate: runner
//! variance). ADR-D / MOD-EMB-001 (semantic embeddings) is only pulled
//! in if the recall gate MISSES — the gate passing keeps embeddings
//! deferred (docs/72: never a correctness dependency).
//! Set MODBIT_BENCH_OUT=<path> to write the full report JSON (used for
//! the evidence bundle; CI asserts gates only).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use modbit_retrieval::task_index::TaskIndex;

/// Fixed revision the corpus is benchmarked at (docs/53: "benchmark at a
/// fixed revision"; bump to re-baseline).
const CORPUS_REVISION: u64 = 1;
const CORPUS_TAG: &str = "phase3-fixed-rev-1";

const CODE_FILES: usize = 2_400;
const TOPIC_DOCS: usize = 200;
const DIRS: usize = 40;

/// Deterministic LCG (no rng dependency; the corpus must be reproducible
/// byte-for-byte on every runner).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

const WORDS: [&str; 24] = [
    "quartz", "lantern", "meridian", "cobalt", "harbor", "falcon", "meadow", "cascade",
    "summit", "willow", "ember", "tundra", "glacier", "canyon", "orchid", "zenith",
    "beacon", "drizzle", "fossil", "garnet", "hollow", "island", "jasper", "krypton",
];

/// Generates the corpus: code files with unique handler functions and
/// cross-file call sites, plus topic docs with distinctive vocabulary.
/// Returns (tree, queries) where each query carries its relevant set.
struct BenchQuery {
    text: String,
    relevant: Vec<String>,
    family: &'static str,
}

fn corpus() -> (BTreeMap<String, Vec<u8>>, Vec<BenchQuery>) {
    let mut rng = Lcg(0x6d6f64626974); // "modbit"
    let mut tree = BTreeMap::new();
    let mut queries: Vec<BenchQuery> = Vec::new();

    for i in 0..CODE_FILES {
        let dir = i % DIRS;
        // Distinctive filler vocabulary per file (deterministic).
        let w1 = WORDS[(i * 7) % WORDS.len()];
        let w2 = WORDS[(i * 13 + 5) % WORDS.len()];
        let caller = if i > 0 && i.is_multiple_of(6) {
            format!(
                "\npub fn relay_{i}() -> u32 {{\n    fn_{0}_handler(1)\n}}\n",
                i - 1
            )
        } else {
            String::new()
        };
        let content = format!(
            "// {w1} {w2} module {i}\npub fn fn_{i}_handler(mode: u32) -> u32 {{\n    let basis = {};\n    match mode {{ 0 => basis, _ => basis + {i} }}\n}}\n{caller}",
            i % 97
        );
        tree.insert(format!("mod{dir}/impl_{i}.rs"), content.into_bytes());
    }

    for t in 0..TOPIC_DOCS {
        let wa = WORDS[t % WORDS.len()];
        let wb = WORDS[(t * 7 + 3) % WORDS.len()];
        let wc = WORDS[(t * 13 + 11) % WORDS.len()];
        // A UNIQUE discriminator per topic (like a real specific query
        // term): shared vocabulary alone collides across topics.
        let mark = format!("topicmark{t}");
        let mut body = format!("# Topic {t} {mark}: {wa} {wb} {wc}\n\n");
        for k in 0..30 {
            body.push_str(&format!(
                "The {wa} approach complements the {wb} constraints and the {wc} fallbacks ({mark}-{k}).\n"
            ));
        }
        tree.insert(format!("docs/topic_{t}.md"), body.into_bytes());
    }

    // Symbol queries: the definer AND any caller file are relevant.
    for q in 0..40 {
        let i = (q * 617) % CODE_FILES; // spread, deterministic
        let mut relevant = vec![format!("mod{}/impl_{}.rs", i % DIRS, i)];
        if i > 0 && i.is_multiple_of(6) {
            relevant.push(format!("mod{}/impl_{}.rs", (i - 1) % DIRS, i - 1));
        }
        relevant.sort();
        queries.push(BenchQuery {
            text: format!("fn_{i}_handler"),
            relevant,
            family: "symbol",
        });
    }
    // Caller queries: the callee name plus a verb; the caller file is the
    // target (the definer is also acceptable evidence).
    for q in 0..20 {
        let i = 6 * ((q * 53 + 7) % (CODE_FILES / 6)); // always a caller file
        let relevant = vec![
            format!("mod{}/impl_{}.rs", i % DIRS, i),
            format!("mod{}/impl_{}.rs", (i - 1) % DIRS, i - 1),
        ];
        queries.push(BenchQuery {
            text: format!("relay_{i} fn_{0}_handler", i - 1),
            relevant,
            family: "caller",
        });
    }
    // Topic queries: the unique discriminator plus shared vocabulary;
    // the topic doc is the target.
    for t in 0..40 {
        let tt = (t * 5) % TOPIC_DOCS;
        let wa = WORDS[tt % WORDS.len()];
        let wb = WORDS[(tt * 7 + 3) % WORDS.len()];
        queries.push(BenchQuery {
            text: format!("topicmark{tt} {wa} {wb}"),
            relevant: vec![format!("docs/topic_{tt}.md")],
            family: "topic",
        });
    }
    let _ = rng.next();
    (tree, queries)
}

/// Profile A: exact/substring baseline — count case-insensitive term
/// occurrences per file (the search.grep ranking class), ties by path.
fn exact_search(tree: &BTreeMap<String, Vec<u8>>, query: &str, top_k: usize) -> Vec<String> {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();
    let mut scored: Vec<(String, usize)> = tree
        .iter()
        .map(|(path, bytes)| {
            let text = String::from_utf8_lossy(bytes).to_lowercase();
            let mut count = 0usize;
            for term in &terms {
                let mut from = 0usize;
                while let Some(found) = text[from..].find(term.as_str()) {
                    count += 1;
                    from += found + term.len();
                }
            }
            (path.clone(), count)
        })
        .filter(|(_, c)| *c > 0)
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(top_k);
    scored.into_iter().map(|(p, _)| p).collect()
}

fn recall_at_k(top: &[String], relevant: &[String], k: usize) -> f64 {
    let hits = top
        .iter()
        .take(k)
        .filter(|p| relevant.contains(p))
        .count();
    hits as f64 / relevant.len().max(1) as f64
}

/// The benchmark + gates. Runs the REAL pipeline on a REAL temp tree.
#[test]
fn retrieval_benchmark_fixed_revision_gates() {
    let (tree, queries) = corpus();
    assert_eq!(tree.len(), CODE_FILES + TOPIC_DOCS);

    // REAL cold build: write the corpus to a temp dir, walk + index it.
    let root: PathBuf = std::env::temp_dir().join(format!(
        "retrieval-bench-{CORPUS_TAG}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    for (path, bytes) in &tree {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, bytes).unwrap();
    }
    let cold_started = Instant::now();
    let index = TaskIndex::build_at(&root, CORPUS_REVISION);
    let cold_index_ms = cold_started.elapsed().as_millis();
    assert_eq!(index.repo.files.len(), tree.len(), "every corpus file indexed");

    // Warm the reader before measuring (docs/53: warm p95).
    let _ = index.context_query("warmup fn_0_handler quartz", 5);

    let mut recall_f: Vec<(String, f64)> = Vec::new();
    let mut recall_a: Vec<(String, f64)> = Vec::new();
    let mut precision_f: Vec<f64> = Vec::new();
    let mut latencies: Vec<Duration> = Vec::new();

    for q in &queries {
        let started = Instant::now();
        let top_f: Vec<String> = index
            .context_query(&q.text, 5)
            .into_iter()
            .map(|h| h.path)
            .collect();
        latencies.push(started.elapsed());

        let top_a = exact_search(&tree, &q.text, 5);
        recall_f.push((q.family.to_string(), recall_at_k(&top_f, &q.relevant, 5)));
        recall_a.push((q.family.to_string(), recall_at_k(&top_a, &q.relevant, 5)));
        if !top_f.is_empty() {
            let hits = top_f.iter().filter(|p| q.relevant.contains(*p)).count();
            precision_f.push(hits as f64 / top_f.len() as f64);
        }
    }

    let mean = |vals: &[f64]| vals.iter().sum::<f64>() / vals.len().max(1) as f64;
    let overall_f = mean(&recall_f.iter().map(|(_, r)| *r).collect::<Vec<_>>());
    let overall_a = mean(&recall_a.iter().map(|(_, r)| *r).collect::<Vec<_>>());
    let per_family_f: std::collections::BTreeMap<String, f64> = ["symbol", "caller", "topic"]
        .iter()
        .map(|f| {
            (
                f.to_string(),
                mean(
                    &recall_f
                        .iter()
                        .filter(|(fam, _)| fam == f)
                        .map(|(_, r)| *r)
                        .collect::<Vec<_>>(),
                ),
            )
        })
        .collect();
    latencies.sort();
    let p95 = latencies[latencies.len() * 95 / 100];
    let median = latencies[latencies.len() / 2];
    let evidence_precision = mean(&precision_f);

    println!(
        "retrieval benchmark: files {} queries {} | recall@5 fused {overall_f:.3} vs exact {overall_a:.3} (per-family fused {per_family_f:?}) | evidence precision@5 {evidence_precision:.3} | warm median {:?} p95 {:?} | cold {}ms",
        tree.len(),
        queries.len(),
        median,
        p95,
        cold_index_ms
    );

    // THE GATES (docs/53; phase item 4).
    assert!(
        overall_f >= overall_a,
        "GATE recall: fused ({overall_f:.3}) must beat/match exact baseline ({overall_a:.3})"
    );
    assert!(
        overall_f >= 0.90,
        "GATE recall floor: fused recall@5 {overall_f:.3} < 0.90"
    );
    assert!(
        p95 < Duration::from_millis(300),
        "GATE latency: warm hybrid p95 {p95:?} >= 300ms (docs/53 L1 bound)"
    );

    let report = serde_json::json!({
        "corpus_tag": CORPUS_TAG,
        "revision": CORPUS_REVISION,
        "files": tree.len(),
        "queries": queries.len(),
        "recall_at_5": {
            "fused": overall_f,
            "exact_baseline": overall_a,
            "fused_per_family": per_family_f,
        },
        "evidence_precision_at_5_fused": evidence_precision,
        "latency": {
            "warm_median_ms": median.as_millis(),
            "warm_p95_ms": p95.as_millis(),
            "gate": "< 300ms (docs/53 L1)",
        },
        "cold_index_ms": cold_index_ms,
        "gates": { "recall_fused_ge_exact": true, "recall_floor_090": true, "warm_p95_lt_300ms": true },
        "adr_d_embeddings": "not required: the recall gate passed with BM25 + path + symbols (docs/72 MOD-EMB-001 stays provisional/deferred)",
    });
    println!("retrieval benchmark report:\n{report}");

    if let Ok(out) = std::env::var("MODBIT_BENCH_OUT") {
        std::fs::write(out, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    let _ = std::fs::remove_dir_all(&root);
}
