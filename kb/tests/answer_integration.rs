//! end-to-end test of build step 5: vectors, prefixes, hybrid retrieval, the `answer` hook, and
//! `kb eval`, against a real (temporary) git repository. no network calls: the embed and answer
//! providers are fakes injected through `index::build_with_providers` and
//! `answer::answer_with_provider`.
use kb::answer::{self, AnswerOpts};
use kb::config::Config;
use kb::git;
use kb::index::{self, SearchMode, SearchOpts};
use kb::provider::{Block, CompletionRequest, CompletionResponse, Provider, Usage};
use kb::runlog::{RunLog, verify};
use kb::store::usage as usage_store;
use std::path::Path;

const ID_NETWORK: &str = "01HZZZZZZZ0000000000000NET";
const ID_INSTALL: &str = "01HZZZZZZZ0000000000000SET";
const ID_BACKUP: &str = "01HZZZZZZZ0000000000000BAK";
const ID_GARDEN: &str = "01HZZZZZZZ0000000000000GDN";
const ID_COFFEE: &str = "01HZZZZZZZ0000000000000CFE";
const ID_BIRD: &str = "01HZZZZZZZ0000000000000BRD";
const ID_BOGUS: &str = "01HZZZZZZZ0000000000000ZZZ";

fn write_doc(root: &Path, id: &str, title: &str, body: &str) {
    let text = format!(
        "+++\nid = \"{id}\"\nlayer = \"reference\"\ntitle = \"{title}\"\nstatus = \"canon\"\nagent_policy = \"editable\"\nowner = \"caleb\"\n+++\n\n{body}\n"
    );
    std::fs::write(root.join("docs").join(format!("{id}.md")), text).unwrap();
}

/// six documents: `install` and `network` are the two probes (a plain fts hit, and a paraphrase
/// only a vector search should find), `backup` rounds out the eval set, and the rest are
/// unrelated distractors that share no vocabulary with any query or probe document.
fn setup_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    git::init_for_tests(root).unwrap();

    write_doc(root, ID_INSTALL, "Install steps", "# Install steps\n\nRun the installer to prepare the tool.\n");
    write_doc(root, ID_NETWORK, "Network configuration", "# Network configuration\n\nAssign fixed address to interface preventing change after reboot.\n");
    write_doc(root, ID_BACKUP, "Backup policy", "# Backup policy\n\nRun backups nightly and verify restores weekly.\n");
    write_doc(root, ID_GARDEN, "Gardening tips", "# Gardening tips\n\nWater plants in the early morning for best results.\n");
    write_doc(root, ID_COFFEE, "Coffee brewing basics", "# Coffee brewing basics\n\nGrind beans fresh and use filtered water.\n");
    write_doc(root, ID_BIRD, "Bird watching basics", "# Bird watching basics\n\nBring binoculars and a field guide.\n");

    git::run(root, &["add", "-A"]).unwrap();
    git::run(root, &["-c", "user.name=Ada Human", "-c", "user.email=ada@example.com", "commit", "-q", "-m", "seed six documents"]).unwrap();
    dir
}

/// bumps the target commit with no content change, so a second `index::build*` call is a real
/// rebuild rather than the `<commit>.sqlite` fast path.
fn bump_commit(root: &Path, msg: &str) {
    git::run(root, &["-c", "user.name=Ada Human", "-c", "user.email=ada@example.com", "commit", "-q", "-m", msg, "--allow-empty"]).unwrap();
}

/// three concepts, each with two spellings that share no substring: a synonym-aware fake embedder
/// maps both spellings of a concept to the identical vector, so a paraphrase using the other
/// spelling of all three concepts embeds identically to the original passage, while sharing zero
/// literal tokens with it (so fts5 cannot match it at all).
fn concept_bucket(word: &str) -> Option<&'static str> {
    match word {
        "assign" | "configure" => Some("concept_configure"),
        "fixed" | "permanent" => Some("concept_static"),
        "address" | "ip" => Some("concept_address"),
        _ => None,
    }
}

/// a small deterministic hash spread over 8 dimensions (fnv-1a over the bucket name, one byte per
/// dimension), so two texts mapping to the same set of concept buckets get exactly the same
/// vector regardless of their surface wording.
fn hash8(seed: &str) -> [f32; 8] {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let mut out = [0f32; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = ((h >> (i as u32 * 7)) & 0xFF) as f32 / 255.0;
    }
    out
}

/// an `embed`-only provider: no network, no completion support, a fixed 8-dim vector per text
/// (see `concept_bucket`/`hash8`) so the hybrid-search test is fully deterministic.
struct FakeEmbedProvider;

impl Provider for FakeEmbedProvider {
    fn name(&self) -> &str {
        "fake-embed"
    }
    fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        anyhow::bail!("FakeEmbedProvider does not support complete")
    }
    fn supports_embed(&self) -> bool {
        true
    }
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut acc = [0f32; 8];
                for word in t.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
                    if let Some(bucket) = concept_bucket(word) {
                        let h = hash8(bucket);
                        for (a, b) in acc.iter_mut().zip(h) {
                            *a += b;
                        }
                    }
                }
                acc.to_vec()
            })
            .collect())
    }
}

/// a completion-only provider: one canned answer citing a real passage and a bogus one.
struct FakeAnswerProvider {
    text: String,
}

impl Provider for FakeAnswerProvider {
    fn name(&self) -> &str {
        "fake-answer"
    }
    fn complete(&self, _req: &CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse {
            blocks: vec![Block::Text { text: self.text.clone() }],
            usage: Usage::default(),
            stop_reason: "end_turn".to_string(),
            raw: serde_json::json!({}),
        })
    }
}

#[test]
fn build_step_5_vectors_prefixes_hybrid_answer_and_eval() {
    let dir = setup_repo();
    let root = dir.path();
    let cfg = Config::load(root).unwrap();

    // 1. build with no providers at all: fts only, but every chunk still gets a deterministic
    // prefix (design.md build step 5, point 2: "without a provider, use the deterministic
    // prefix ... so fts and vectors always have one").
    let report1 = index::build(&cfg, None).unwrap();
    assert!(!report1.already_built);
    assert_eq!(report1.document_count, 6);
    assert!(!report1.vectors_enabled, "no embed provider configured yet");
    assert_eq!(report1.prefixes_generated, report1.chunk_count, "every chunk needed a prefix on a first build");

    let conn1 = index::open_newest(&cfg).unwrap();
    let prefix: String = conn1
        .query_row("select prefix from chunks where doc_id = ?1", [ID_INSTALL], |r| r.get(0))
        .unwrap();
    assert_eq!(prefix, "Install steps > install-steps", "deterministic prefix format: '<title> > <heading_path>'");

    // search in fts mode finds the literal match.
    let opts = SearchOpts { mode: Some(SearchMode::Fts), ..Default::default() };
    let hits = index::search(&conn1, "install steps", &opts, None, None).unwrap();
    assert!(hits.iter().any(|h| h.doc_id == ID_INSTALL), "fts should find the install document by its own words");

    // the paraphrase below shares no token with the network document (or any other document) --
    // fts must not find it.
    let paraphrase = "how do you configure permanent ip on the box";
    let fts_only = index::search(&conn1, paraphrase, &opts, None, None).unwrap();
    assert!(!fts_only.iter().any(|h| h.doc_id == ID_NETWORK), "fts should miss the paraphrase entirely");

    // 2. rebuild (a new commit, so it is a real rebuild, not the <commit>.sqlite fast path) with
    // a fake embed provider injected through the build_with_providers seam.
    bump_commit(root, "trigger a rebuild with vectors enabled");
    let embed_provider = FakeEmbedProvider;
    let report2 = index::build_with_providers(&cfg, None, None, Some(&embed_provider)).unwrap();
    assert!(report2.vectors_enabled, "an embed provider was injected this build");
    assert_eq!(report2.vector_count, report2.chunk_count, "every chunk in this fixture is a canon document chunk");

    let conn2 = index::open_newest(&cfg).unwrap();
    assert_eq!(conn2.query_row::<String, _, _>("select value from meta where key = 'commit'", [], |r| r.get(0)).unwrap(), report2.commit);

    // hybrid search: embed the paraphrase with the same fake provider and confirm it now finds
    // the network document that fts alone could not.
    let query_vec = embed_provider.embed(&[paraphrase.to_string()]).unwrap().pop().unwrap();
    let hybrid_opts = SearchOpts { mode: Some(SearchMode::Hybrid), ..Default::default() };
    let hybrid_hits = index::search(&conn2, paraphrase, &hybrid_opts, Some(&query_vec), None).unwrap();
    assert!(hybrid_hits.iter().any(|h| h.doc_id == ID_NETWORK), "hybrid search should find the network document via its embedding, got {hybrid_hits:?}");

    let fts_only_again = index::search(&conn2, paraphrase, &SearchOpts { mode: Some(SearchMode::Fts), ..Default::default() }, None, None).unwrap();
    assert!(!fts_only_again.iter().any(|h| h.doc_id == ID_NETWORK), "fts still should not find the paraphrase on the rebuilt index");

    // 3. answer: a fake completion cites the install passage and a bogus id; the bogus one must
    // be dropped and noted, and usage rows and a sealed run log must exist.
    let good_citation = format!("[kb:{ID_INSTALL}#install-steps]");
    let bad_citation = format!("[kb:{ID_BOGUS}#nonexistent]");
    let answer_text = format!("run the installer as described {good_citation}. also see {bad_citation} for background.");
    let answer_provider = FakeAnswerProvider { text: answer_text };

    // fts5 joins bareword query terms with an implicit AND (see `sanitize_fts_query`'s doc
    // comment in index/query.rs), so the question has to be answerable from words that are all
    // actually present in the target passage's prefix or text -- a full natural-language
    // question would need a query embedding (hybrid mode) to match, which is exercised above.
    let result = answer::answer_with_provider(&cfg, "install steps", &AnswerOpts::default(), Some(Box::new(answer_provider))).unwrap();
    assert!(!result.deferred);
    assert_eq!(result.citations.len(), 1, "only the real passage should resolve");
    assert_eq!(result.citations[0].doc_id, ID_INSTALL);
    assert_eq!(result.citations[0].heading_path, "install-steps");
    assert_eq!(result.dropped_citations.len(), 1);
    assert!(result.dropped_citations[0].contains(ID_BOGUS));
    assert!(!result.text.contains(ID_BOGUS), "the bogus citation must not survive into the answer text");
    assert!(result.text.contains(ID_INSTALL), "the valid citation should remain in the answer text");
    assert!(!result.hits.is_empty(), "the question should have retrieved at least the install passage");

    // usage: reads for every retrieved document, a cite for the install document.
    let (present, usage_by_doc) = usage_store::aggregate_90d(&cfg, chrono::Utc::now()).unwrap();
    assert!(present, "answer should have written usage rows");
    let (reads, cites) = usage_by_doc.get(ID_INSTALL).copied().unwrap_or((0, 0));
    assert!(reads >= 1, "install should have at least one read");
    assert_eq!(cites, 1, "install should have exactly one cite");

    // a sealed run log exists and its hash chain verifies.
    let opened = RunLog::open(&cfg, &result.run_id).unwrap();
    assert_eq!(opened.run_id(), result.run_id);
    let report = verify::verify(opened.path()).unwrap();
    assert!(report.ok, "{report:?}");

    // 4. eval over a 3-query set runs and prints.
    let eval_dir = root.join("eval");
    std::fs::create_dir_all(&eval_dir).unwrap();
    let queries = vec![
        answer::eval::QueryRow { text: "install steps".to_string(), expected: vec![format!("kb:{ID_INSTALL}")], area: None, seeded: false },
        answer::eval::QueryRow { text: "network configuration".to_string(), expected: vec![format!("kb:{ID_NETWORK}")], area: None, seeded: false },
        answer::eval::QueryRow { text: "backup policy".to_string(), expected: vec![format!("kb:{ID_BACKUP}")], area: None, seeded: false },
    ];
    answer::eval::write_queries(&eval_dir.join("queries.toml"), &queries).unwrap();
    let loaded = answer::eval::load_queries(&eval_dir.join("queries.toml")).unwrap();
    assert_eq!(loaded.len(), 3);
    let metrics = answer::eval::run_eval(&cfg, answer::eval::Feature::Fts, &loaded).unwrap();
    println!("eval (fts): mrr@10={:.4} recall@5={:.4} n={}", metrics.mrr_at_10, metrics.recall_at_5, metrics.n);
    assert_eq!(metrics.n, 3);
    assert!(metrics.mrr_at_10 > 0.0, "each query is a literal match for its own document; mrr should be positive");
}
