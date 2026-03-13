use std::path::Path;
use std::sync::Mutex;

use ndarray::{Array2, Axis};
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use crate::error::{SparksError, Result};

const MODEL_REPO: &str = "sentence-transformers/all-MiniLM-L6-v2";
const MODEL_FILE: &str = "model.onnx";
const TOKENIZER_FILE: &str = "tokenizer.json";
const MAX_LENGTH: usize = 256;

pub struct Embedder {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
}

impl Embedder {
    /// Load an ONNX embedding model + tokenizer from `model_dir`.
    pub fn new(model_dir: &Path) -> Result<Self> {
        let model_path = model_dir.join(MODEL_FILE);
        let tokenizer_path = model_dir.join(TOKENIZER_FILE);

        if !model_path.exists() || !tokenizer_path.exists() {
            return Err(SparksError::Internal(format!(
                "Embedding model files not found in {}. Run ensure_model() first.",
                model_dir.display()
            )));
        }

        let session = Session::builder()
            .and_then(|b| b.with_intra_threads(1))
            .and_then(|b| b.commit_from_file(&model_path))
            .map_err(|e| SparksError::Internal(format!("Failed to load ONNX model: {}", e)))?;

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| SparksError::Internal(format!("Failed to load tokenizer: {}", e)))?;

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
        })
    }

    /// Download model files from HuggingFace if they don't exist locally.
    pub fn ensure_model(model_dir: &Path) -> Result<()> {
        let model_path = model_dir.join(MODEL_FILE);
        let tokenizer_path = model_dir.join(TOKENIZER_FILE);

        if model_path.exists() && tokenizer_path.exists() {
            return Ok(());
        }

        std::fs::create_dir_all(model_dir)
            .map_err(|e| SparksError::Internal(format!("Failed to create model dir: {}", e)))?;

        let base_url = format!("https://huggingface.co/{}/resolve/main", MODEL_REPO);

        if !model_path.exists() {
            tracing::info!("Downloading embedding model (~23MB)...");
            download_file(&format!("{}/onnx/{}", base_url, MODEL_FILE), &model_path)?;
        }

        if !tokenizer_path.exists() {
            tracing::info!("Downloading tokenizer...");
            download_file(&format!("{}/{}", base_url, TOKENIZER_FILE), &tokenizer_path)?;
        }

        tracing::info!("Embedding model ready at {}", model_dir.display());
        Ok(())
    }

    /// Tokenize, run ONNX inference, mean-pool, and L2-normalize to produce
    /// a 384-dimensional embedding vector.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| SparksError::Internal(format!("Tokenization failed: {}", e)))?;

        let mut input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
        let mut attention_mask: Vec<i64> = encoding
            .get_attention_mask()
            .iter()
            .map(|&m| m as i64)
            .collect();

        // Truncate to MAX_LENGTH
        input_ids.truncate(MAX_LENGTH);
        attention_mask.truncate(MAX_LENGTH);

        let seq_len = input_ids.len();

        let input_ids_array = Array2::from_shape_vec((1, seq_len), input_ids)
            .map_err(|e| SparksError::Internal(format!("Shape error: {}", e)))?;
        let attention_mask_array = Array2::from_shape_vec((1, seq_len), attention_mask.clone())
            .map_err(|e| SparksError::Internal(format!("Shape error: {}", e)))?;

        // token_type_ids: all zeros for single-sentence encoding (required by BERT-based models)
        let token_type_ids_array = Array2::<i64>::zeros((1, seq_len));

        let input_ids_tensor = Tensor::from_array(input_ids_array)
            .map_err(|e| SparksError::Internal(format!("Input tensor creation failed: {}", e)))?;
        let attention_mask_tensor = Tensor::from_array(attention_mask_array)
            .map_err(|e| SparksError::Internal(format!("Input tensor creation failed: {}", e)))?;
        let token_type_ids_tensor = Tensor::from_array(token_type_ids_array)
            .map_err(|e| SparksError::Internal(format!("Input tensor creation failed: {}", e)))?;

        let mut session = self
            .session
            .lock()
            .map_err(|e| SparksError::Internal(format!("Session lock poisoned: {}", e)))?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => input_ids_tensor,
                "attention_mask" => attention_mask_tensor,
                "token_type_ids" => token_type_ids_tensor,
            ])
            .map_err(|e| SparksError::Internal(format!("ONNX inference failed: {}", e)))?;

        // Output shape: (1, seq_len, hidden_size=384)
        let token_embeddings = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|e| SparksError::Internal(format!("Output extraction failed: {}", e)))?;

        let token_embeddings = token_embeddings
            .into_dimensionality::<ndarray::Ix3>()
            .map_err(|e| SparksError::Internal(format!("Dimension error: {}", e)))?;

        // Mean pooling: average token embeddings weighted by attention mask
        let mask: Vec<f32> = attention_mask.iter().map(|&m| m as f32).collect();
        let mask_array = Array2::from_shape_vec((1, seq_len), mask)
            .map_err(|e| SparksError::Internal(format!("Mask shape error: {}", e)))?;

        let embeddings_2d = token_embeddings.index_axis(Axis(0), 0); // (seq_len, hidden)
        let mask_1d = mask_array.index_axis(Axis(0), 0); // (seq_len,)

        let hidden_size = embeddings_2d.shape()[1];
        let mut pooled = vec![0.0f32; hidden_size];
        let mut mask_sum = 0.0f32;

        for i in 0..seq_len {
            let m = mask_1d[i];
            mask_sum += m;
            for j in 0..hidden_size {
                pooled[j] += embeddings_2d[[i, j]] * m;
            }
        }

        if mask_sum > 0.0 {
            for v in &mut pooled {
                *v /= mask_sum;
            }
        }

        // L2 normalize
        let norm: f32 = pooled.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut pooled {
                *v /= norm;
            }
        }

        Ok(pooled)
    }
}

/// Cosine similarity between two L2-normalized vectors (just a dot product).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_identical_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_orthogonal_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_opposite_vectors() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_empty_vectors() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    /// Large-scale precision-focused memory retrieval benchmark.
    ///
    /// 50 memories organized in dense topic clusters with intentional overlap.
    /// 60 queries across 8 categories testing disambiguation, distractor resistance,
    /// and fine-grained precision.
    ///
    /// Metrics: Hit@1, Hit@3, Hit@5, MRR, Precision@1, Precision@3, Precision@5,
    ///          false positive rate, distractor rate per cluster.
    ///
    /// Run with: cargo test bench_memory_retrieval -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_memory_retrieval() {
        // ── Setup ──────────────────────────────────────────────────
        let model_dir = dirs::home_dir()
            .unwrap()
            .join(".sparks/models/all-MiniLM-L6-v2");
        Embedder::ensure_model(&model_dir).expect("model download failed");
        let embedder = Embedder::new(&model_dir).expect("embedder creation failed");

        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY,
                category TEXT NOT NULL,
                content TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                embedding BLOB
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(content);
            CREATE TABLE IF NOT EXISTS conversations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_key TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_conversations_session ON conversations(session_key, created_at);"
        ).unwrap();
        let store = crate::memory::MemoryStore::new_with_hnsw(
            conn,
            30.0,
            0.95,
            256,
            crate::memory::HnswIndexConfig::default(),
        );

        let corpus = build_bench_corpus();

        // Store with embeddings
        let mut memory_contents: Vec<String> = Vec::new();
        for (cat, _cluster, content) in &corpus {
            let emb = embedder.embed(content).unwrap();
            store.store(cat, content, Some(&emb)).unwrap();
            memory_contents.push(content.to_string());
        }

        let cases = build_bench_test_cases();

        // ── Run benchmark ──────────────────────────────────────────
        let semantic_threshold = crate::memory::SEMANTIC_THRESHOLD;
        let mut results: Vec<QueryResult> = Vec::new();

        for case in &cases {
            let q_emb = embedder.embed(case.query).unwrap();
            let hybrid = store.search_hybrid(case.query, Some(&q_emb), 5).unwrap();
            let semantic: Vec<(String, f32)> = store
                .search_semantic(&q_emb, 5)
                .unwrap()
                .into_iter()
                .map(|(m, score)| (m.content, score))
                .collect();
            results.push(evaluate_bench_query(
                case,
                &hybrid,
                &semantic,
                &memory_contents,
            ));
        }

        // ── Print report ───────────────────────────────────────────
        let sep = "=".repeat(100);
        println!("\n{}", sep);
        println!("  MEMORY RETRIEVAL PRECISION BENCHMARK");
        println!(
            "  {} memories | {} queries | semantic threshold: {}",
            corpus.len(),
            results.len(),
            semantic_threshold
        );
        println!("{}\n", sep);

        print_bench_per_query(&results);
        print_bench_aggregate(&results);
    }

    #[derive(Default)]
    struct Totals {
        n: usize,
        hit1: usize,
        hit3: usize,
        hit5: usize,
        rr: f32,
        p1: f32,
        p3: f32,
        p5: f32,
        dist: usize,
    }

    struct TestCase {
        query: &'static str,
        expected: Vec<usize>,
        forbidden: Vec<usize>,
        category: &'static str,
    }

    #[allow(dead_code, reason = "retained for serde/db compatibility")]
    struct QueryResult {
        query: String,
        category: String,
        expected: Vec<usize>,
        forbidden: Vec<usize>,
        retrieved_1: Vec<usize>,
        retrieved_3: Vec<usize>,
        retrieved_5: Vec<usize>,
        top_scores: Vec<f32>,
        hit_1: bool,
        hit_3: bool,
        hit_5: bool,
        rr: f32,
        prec_1: f32,
        prec_3: f32,
        prec_5: f32,
        distractor_count: usize,
    }

    fn build_bench_corpus() -> Vec<(&'static str, &'static str, &'static str)> {
        vec![
            // ── [PL] Programming Languages cluster (5 confusable) ──
            ("fact", "pl", "I prefer Python over Go for scripting tasks"), // 0
            (
                "fact",
                "pl",
                "I use Rust for systems programming and CLI tools",
            ), // 1
            (
                "fact",
                "pl",
                "TypeScript is my go-to for frontend web development",
            ), // 2
            (
                "fact",
                "pl",
                "I learned Java in college but rarely use it now",
            ), // 3
            ("fact", "pl", "I write shell scripts in Bash for automation"), // 4
            // ── [DB] Database cluster (4 confusable) ────────────────
            (
                "pref",
                "db",
                "My preferred database is PostgreSQL for relational data",
            ), // 5
            ("fact", "db", "I use Redis for caching and session storage"), // 6
            (
                "fact",
                "db",
                "MongoDB is our document store for user profiles",
            ), // 7
            (
                "fact",
                "db",
                "We run SQLite for local development and testing",
            ), // 8
            // ── [FD] Food cluster (5 confusable) ────────────────────
            (
                "fact",
                "fd",
                "My favorite food is sushi, especially salmon nigiri",
            ), // 9
            ("fact", "fd", "I'm allergic to peanuts and tree nuts"), // 10
            ("fact", "fd", "I drink oat milk lattes every morning"), // 11
            (
                "fact",
                "fd",
                "My go-to lunch is a burrito from the taqueria on Mission St",
            ), // 12
            (
                "fact",
                "fd",
                "I'm trying to eat less red meat for health reasons",
            ), // 13
            // ── [GEO] Geography/Weather cluster (4 confusable) ──────
            (
                "fact",
                "geo",
                "The weather is usually foggy in SF during summer",
            ), // 14
            (
                "fact",
                "geo",
                "I grew up in Portland, Oregon where it rains constantly",
            ), // 15
            (
                "fact",
                "geo",
                "I moved to San Francisco three years ago for work",
            ), // 16
            (
                "fact",
                "geo",
                "I want to visit Tokyo next spring for the cherry blossoms",
            ), // 17
            // ── [HW] Hardware/Setup cluster (4 confusable) ──────────
            (
                "fact",
                "hw",
                "My laptop runs macOS with 32GB of RAM and M2 Pro chip",
            ), // 18
            (
                "fact",
                "hw",
                "I have a 27-inch 4K monitor on my desk at home",
            ), // 19
            (
                "fact",
                "hw",
                "My mechanical keyboard is a Keychron Q1 with brown switches",
            ), // 20
            (
                "fact",
                "hw",
                "I use AirPods Pro for noise cancellation during focus time",
            ), // 21
            // ── [ED] Education cluster (3 confusable) ───────────────
            ("fact", "ed", "I studied computer science at MIT"), // 22
            (
                "fact",
                "ed",
                "I took Andrew Ng's machine learning course on Coursera",
            ), // 23
            (
                "fact",
                "ed",
                "I'm currently reading Designing Data-Intensive Applications",
            ), // 24
            // ── [PET] Pets cluster (3 confusable) ───────────────────
            (
                "fact",
                "pet",
                "I have a golden retriever named Max who is 4 years old",
            ), // 25
            ("fact", "pet", "My cat Luna likes to sleep on my keyboard"), // 26
            (
                "fact",
                "pet",
                "We adopted Max from a rescue shelter in Oakland",
            ), // 27
            // ── [ML] Machine Learning cluster (4 confusable) ────────
            (
                "lesson",
                "ml",
                "Neural networks use gradient descent for training",
            ), // 28
            (
                "lesson",
                "ml",
                "Transformer models use self-attention instead of recurrence",
            ), // 29
            (
                "lesson",
                "ml",
                "Random forests often outperform neural nets on tabular data",
            ), // 30
            (
                "lesson",
                "ml",
                "Fine-tuning a pretrained LLM requires much less data than training from scratch",
            ), // 31
            // ── [FIT] Fitness/Health cluster (4 confusable) ─────────
            ("fact", "fit", "I enjoy hiking in Marin County on weekends"), // 32
            (
                "fact",
                "fit",
                "I run 5K three times a week in Golden Gate Park",
            ), // 33
            (
                "fact",
                "fit",
                "I usually wake up at 7am and meditate for 10 minutes",
            ), // 34
            (
                "fact",
                "fit",
                "I've been doing yoga every Tuesday and Thursday evening",
            ), // 35
            // ── [WRK] Work/Career cluster (4 confusable) ────────────
            (
                "fact",
                "wrk",
                "I work at a startup in San Francisco as a senior engineer",
            ), // 36
            (
                "fact",
                "wrk",
                "The project deadline is March 15th for the v2 launch",
            ), // 37
            (
                "fact",
                "wrk",
                "Our team uses two-week sprints with Monday standups",
            ), // 38
            (
                "fact",
                "wrk",
                "The API rate limit is 100 requests per minute",
            ), // 39
            // ── [TOOL] Dev Tools cluster (5 confusable) ─────────────
            ("fact", "tool", "I use VS Code as my primary code editor"), // 40
            ("fact", "tool", "I switched from vim to Neovim last year"), // 41
            (
                "fact",
                "tool",
                "Git and GitHub are essential to my workflow",
            ), // 42
            (
                "fact",
                "tool",
                "I use Docker for local development environments",
            ), // 43
            (
                "fact",
                "tool",
                "My terminal emulator is Warp with the Catppuccin theme",
            ), // 44
            // ── [MISC] Miscellaneous (5 unrelated) ──────────────────
            ("fact", "misc", "My phone number ends in 4242"), // 45
            ("fact", "misc", "I listen to lo-fi hip hop while coding"), // 46
            (
                "fact",
                "misc",
                "The office WiFi password is taped under the router",
            ), // 47
            ("fact", "misc", "I prefer dark mode in every application"), // 48
            ("fact", "misc", "My Spotify wrapped top artist was Tycho"), // 49
        ]
    }

    fn build_bench_test_cases() -> Vec<TestCase> {
        vec![
            // ═══════════════════════════════════════════════════════════
            // CATEGORY: "disambig" — pick the right one from a dense cluster
            // ═══════════════════════════════════════════════════════════
            TestCase {
                query: "which language for scripting?",
                expected: vec![0],
                forbidden: vec![1, 2, 3, 4],
                category: "disambig",
            },
            TestCase {
                query: "what do I use for systems-level programming?",
                expected: vec![1],
                forbidden: vec![0, 2, 3, 4],
                category: "disambig",
            },
            TestCase {
                query: "what language for web frontend?",
                expected: vec![2],
                forbidden: vec![0, 1, 3, 4],
                category: "disambig",
            },
            TestCase {
                query: "which language did I learn in university?",
                expected: vec![3],
                forbidden: vec![0, 1, 2, 4],
                category: "disambig",
            },
            TestCase {
                query: "what do I use for shell automation?",
                expected: vec![4],
                forbidden: vec![0, 1, 2, 3],
                category: "disambig",
            },
            TestCase {
                query: "which database for caching?",
                expected: vec![6],
                forbidden: vec![5, 7, 8],
                category: "disambig",
            },
            TestCase {
                query: "what is our document database?",
                expected: vec![7],
                forbidden: vec![5, 6, 8],
                category: "disambig",
            },
            TestCase {
                query: "what database for local dev?",
                expected: vec![8],
                forbidden: vec![5, 6, 7],
                category: "disambig",
            },
            TestCase {
                query: "tell me about my dog",
                expected: vec![25],
                forbidden: vec![26, 27],
                category: "disambig",
            },
            TestCase {
                query: "tell me about my cat",
                expected: vec![26],
                forbidden: vec![25, 27],
                category: "disambig",
            },
            TestCase {
                query: "where did we get our pet from?",
                expected: vec![27],
                forbidden: vec![26],
                category: "disambig",
            },
            TestCase {
                query: "how does self-attention work in modern AI?",
                expected: vec![29],
                forbidden: vec![28, 30, 31],
                category: "disambig",
            },
            TestCase {
                query: "what works best for tabular data?",
                expected: vec![30],
                forbidden: vec![28, 29, 31],
                category: "disambig",
            },
            TestCase {
                query: "what are the benefits of fine-tuning LLMs?",
                expected: vec![31],
                forbidden: vec![28, 29, 30],
                category: "disambig",
            },
            // ═══════════════════════════════════════════════════════════
            // CATEGORY: "semantic" — zero/minimal keyword overlap
            // ═══════════════════════════════════════════════════════════
            TestCase {
                query: "deep learning optimization techniques",
                expected: vec![28],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "outdoor recreation activities nearby",
                expected: vec![32],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "food allergies and dietary restrictions",
                expected: vec![10],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "my education and academic background",
                expected: vec![22],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "what computer hardware specs do I have?",
                expected: vec![18],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "Bay Area summer climate",
                expected: vec![14],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "Japanese cuisine preferences",
                expected: vec![9],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "relational data management system",
                expected: vec![5],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "morning wellness routine",
                expected: vec![34],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "cardiovascular exercise routine",
                expected: vec![33],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "music for concentration",
                expected: vec![46],
                forbidden: vec![],
                category: "semantic",
            },
            TestCase {
                query: "UI color scheme preference",
                expected: vec![48],
                forbidden: vec![],
                category: "semantic",
            },
            // ═══════════════════════════════════════════════════════════
            // CATEGORY: "exact" — keyword/exact match precision
            // ═══════════════════════════════════════════════════════════
            TestCase {
                query: "PostgreSQL",
                expected: vec![5],
                forbidden: vec![6, 7, 8],
                category: "exact",
            },
            TestCase {
                query: "Redis",
                expected: vec![6],
                forbidden: vec![5, 7, 8],
                category: "exact",
            },
            TestCase {
                query: "MongoDB",
                expected: vec![7],
                forbidden: vec![5, 6, 8],
                category: "exact",
            },
            TestCase {
                query: "VS Code",
                expected: vec![40],
                forbidden: vec![41, 42, 43, 44],
                category: "exact",
            },
            TestCase {
                query: "Neovim",
                expected: vec![41],
                forbidden: vec![40, 42, 43, 44],
                category: "exact",
            },
            TestCase {
                query: "Docker",
                expected: vec![43],
                forbidden: vec![40, 41, 42, 44],
                category: "exact",
            },
            TestCase {
                query: "golden retriever",
                expected: vec![25],
                forbidden: vec![26, 27],
                category: "exact",
            },
            TestCase {
                query: "Keychron keyboard",
                expected: vec![20],
                forbidden: vec![18, 19, 21],
                category: "exact",
            },
            // ═══════════════════════════════════════════════════════════
            // CATEGORY: "multi" — multiple correct answers in same cluster
            // ═══════════════════════════════════════════════════════════
            TestCase {
                query: "tell me about San Francisco",
                expected: vec![14, 16, 36],
                forbidden: vec![],
                category: "multi",
            },
            TestCase {
                query: "my development environment and tools",
                expected: vec![18, 40, 43, 44],
                forbidden: vec![],
                category: "multi",
            },
            TestCase {
                query: "my exercise and fitness activities",
                expected: vec![32, 33, 35],
                forbidden: vec![],
                category: "multi",
            },
            TestCase {
                query: "tell me about all my pets",
                expected: vec![25, 26, 27],
                forbidden: vec![],
                category: "multi",
            },
            TestCase {
                query: "what databases do we use?",
                expected: vec![5, 6, 7, 8],
                forbidden: vec![],
                category: "multi",
            },
            TestCase {
                query: "what programming languages do I know?",
                expected: vec![0, 1, 2, 3, 4],
                forbidden: vec![],
                category: "multi",
            },
            // ═══════════════════════════════════════════════════════════
            // CATEGORY: "negative" — nothing in corpus should match
            // ═══════════════════════════════════════════════════════════
            TestCase {
                query: "quantum entanglement in particle physics",
                expected: vec![],
                forbidden: vec![],
                category: "negative",
            },
            TestCase {
                query: "the fall of the Roman Empire",
                expected: vec![],
                forbidden: vec![],
                category: "negative",
            },
            TestCase {
                query: "how to change a flat tire",
                expected: vec![],
                forbidden: vec![],
                category: "negative",
            },
            TestCase {
                query: "recipe for sourdough bread",
                expected: vec![],
                forbidden: vec![],
                category: "negative",
            },
            TestCase {
                query: "rules of cricket",
                expected: vec![],
                forbidden: vec![],
                category: "negative",
            },
            TestCase {
                query: "volcanic eruptions on Mars",
                expected: vec![],
                forbidden: vec![],
                category: "negative",
            },
            // ═══════════════════════════════════════════════════════════
            // CATEGORY: "cross" — query spans multiple clusters
            // ═══════════════════════════════════════════════════════════
            TestCase {
                query: "what is my work setup like?",
                expected: vec![18, 19, 20, 40, 44],
                forbidden: vec![],
                category: "cross",
            },
            TestCase {
                query: "things I do for my health",
                expected: vec![13, 33, 34, 35],
                forbidden: vec![],
                category: "cross",
            },
            TestCase {
                query: "what have I studied or learned?",
                expected: vec![22, 23, 24],
                forbidden: vec![],
                category: "cross",
            },
            // ═══════════════════════════════════════════════════════════
            // CATEGORY: "tricky" — adversarial/edge-case queries
            // ═══════════════════════════════════════════════════════════
            TestCase {
                query: "Python", // should find PL, not the pet snake
                expected: vec![0],
                forbidden: vec![],
                category: "tricky",
            },
            TestCase {
                query: "Max", // dog's name — should find dog, not numbers
                expected: vec![25],
                forbidden: vec![],
                category: "tricky",
            },
            TestCase {
                query: "Go", // should find PL mention, tricky short word
                expected: vec![0],
                forbidden: vec![],
                category: "tricky",
            },
            TestCase {
                query: "spring", // Tokyo trip, not a framework
                expected: vec![17],
                forbidden: vec![],
                category: "tricky",
            },
            TestCase {
                query: "mission",
                expected: vec![12],
                forbidden: vec![],
                category: "tricky",
            },
            // ═══════════════════════════════════════════════════════════
            // CATEGORY: "paraphrase" — same intent, different phrasing
            // ═══════════════════════════════════════════════════════════
            TestCase {
                query: "do I have any food sensitivities?",
                expected: vec![10],
                forbidden: vec![9, 11, 12, 13],
                category: "paraphrase",
            },
            TestCase {
                query: "what time does my alarm go off?",
                expected: vec![34],
                forbidden: vec![32, 33, 35],
                category: "paraphrase",
            },
            TestCase {
                query: "which text editor is my daily driver?",
                expected: vec![40],
                forbidden: vec![41, 42, 43, 44],
                category: "paraphrase",
            },
            TestCase {
                query: "where did I go to school?",
                expected: vec![22],
                forbidden: vec![23, 24],
                category: "paraphrase",
            },
            TestCase {
                query: "what's my caffeine habit?",
                expected: vec![11],
                forbidden: vec![9, 10, 12, 13],
                category: "paraphrase",
            },
            TestCase {
                query: "what container tool do I rely on?",
                expected: vec![43],
                forbidden: vec![40, 41, 42, 44],
                category: "paraphrase",
            },
        ]
    }

    fn evaluate_bench_query(
        case: &TestCase,
        hybrid: &[crate::memory::Memory],
        semantic_scores: &[(String, f32)],
        memory_contents: &[String],
    ) -> QueryResult {
        let top_scores: Vec<f32> = semantic_scores.iter().map(|(_, s)| *s).collect();
        let top_first = top_scores.first().copied().unwrap_or(0.0);
        let all_indices: Vec<usize> = hybrid
            .iter()
            .filter_map(|m| memory_contents.iter().position(|c| c == &m.content))
            .collect();

        let retrieved_1: Vec<usize> = all_indices.iter().take(1).copied().collect();
        let retrieved_3: Vec<usize> = all_indices.iter().take(3).copied().collect();
        let retrieved_5: Vec<usize> = all_indices.clone();

        let compute_hit = |retrieved: &[usize]| -> bool {
            if case.expected.is_empty() {
                return top_first <= 0.4;
            }
            retrieved.iter().any(|idx| case.expected.contains(idx))
        };

        let precision_at = |retrieved: &[usize], k: usize| -> f32 {
            if case.expected.is_empty() {
                return if top_first <= 0.4 { 1.0 } else { 0.0 };
            }
            let hits = retrieved
                .iter()
                .filter(|i| case.expected.contains(i))
                .count();
            hits as f32 / k as f32
        };

        let hit_1 = compute_hit(&retrieved_1);
        let hit_3 = compute_hit(&retrieved_3);
        let hit_5 = compute_hit(&retrieved_5);

        let rr = if case.expected.is_empty() {
            if top_first > 0.4 {
                0.0
            } else {
                1.0
            }
        } else {
            let mut first_rank = 0usize;
            for (rank, idx) in retrieved_5.iter().enumerate() {
                if case.expected.contains(idx) && first_rank == 0 {
                    first_rank = rank + 1;
                }
            }
            if first_rank > 0 {
                1.0 / first_rank as f32
            } else {
                0.0
            }
        };

        let distractor_count = retrieved_5
            .iter()
            .filter(|i| case.forbidden.contains(i))
            .count();

        let prec_1 = precision_at(&retrieved_1, 1);
        let prec_3 = precision_at(&retrieved_3, 3);
        let prec_5 = precision_at(&retrieved_5, 5);

        QueryResult {
            query: case.query.to_string(),
            category: case.category.to_string(),
            expected: case.expected.clone(),
            forbidden: case.forbidden.clone(),
            retrieved_1,
            retrieved_3,
            retrieved_5,
            top_scores,
            hit_1,
            hit_3,
            hit_5,
            rr,
            prec_1,
            prec_3,
            prec_5,
            distractor_count,
        }
    }

    fn print_bench_per_query(results: &[QueryResult]) {
        for r in results {
            let status = if r.hit_1 {
                " @1"
            } else if r.hit_3 {
                " @3"
            } else if r.hit_5 {
                " @5"
            } else {
                "  X"
            };
            let scores_str = r
                .top_scores
                .iter()
                .take(3)
                .map(|s| format!("{:.3}", s))
                .collect::<Vec<_>>()
                .join(", ");
            let expected_str = if r.expected.is_empty() {
                "(none)".into()
            } else {
                r.expected
                    .iter()
                    .map(|i| format!("{}", i))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let retrieved_str = if r.retrieved_5.is_empty() {
                "(none)".into()
            } else {
                r.retrieved_5
                    .iter()
                    .map(|i| format!("{}", i))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let dist_warn = if r.distractor_count > 0 {
                format!("  ⚠ {} distractor(s)", r.distractor_count)
            } else {
                String::new()
            };
            println!("[{}] {:<12} \"{}\"", status, r.category, r.query);
            println!("      expect: [{:<16}]  got: [{:<20}]  sim: [{}]  RR:{:.2}  P@1:{:.0} P@3:{:.0} P@5:{:.0}{}",
                expected_str, retrieved_str, scores_str, r.rr,
                r.prec_1 * 100.0, r.prec_3 * 100.0, r.prec_5 * 100.0, dist_warn);
        }
    }

    fn print_bench_aggregate(results: &[QueryResult]) {
        let thin = "-".repeat(100);
        println!("\n{}", thin);
        println!("  AGGREGATE METRICS BY CATEGORY\n");
        println!(
            "  {:<14} {:>5} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}  {:>10}",
            "Category", "N", "Hit@1", "Hit@3", "Hit@5", "MRR", "P@1", "P@3", "P@5", "Distract"
        );

        let categories = [
            "disambig",
            "semantic",
            "exact",
            "multi",
            "negative",
            "cross",
            "tricky",
            "paraphrase",
        ];
        let mut totals = Totals::default();

        for cat in &categories {
            let cr: Vec<&QueryResult> = results.iter().filter(|r| r.category == *cat).collect();
            if cr.is_empty() {
                continue;
            }
            let n = cr.len();
            let hit1 = cr.iter().filter(|r| r.hit_1).count();
            let hit3 = cr.iter().filter(|r| r.hit_3).count();
            let hit5 = cr.iter().filter(|r| r.hit_5).count();
            let mrr: f32 = cr.iter().map(|r| r.rr).sum::<f32>() / n as f32;
            let p1: f32 = cr.iter().map(|r| r.prec_1).sum::<f32>() / n as f32;
            let p3: f32 = cr.iter().map(|r| r.prec_3).sum::<f32>() / n as f32;
            let p5: f32 = cr.iter().map(|r| r.prec_5).sum::<f32>() / n as f32;
            let dist: usize = cr.iter().map(|r| r.distractor_count).sum();

            println!("  {:<14} {:>5} {:>6.0}% {:>6.0}% {:>6.0}% {:>7.3} {:>6.0}% {:>6.0}% {:>6.0}%  {:>5}/{:<4}",
                cat, n,
                100.0 * hit1 as f32 / n as f32,
                100.0 * hit3 as f32 / n as f32,
                100.0 * hit5 as f32 / n as f32,
                mrr, p1 * 100.0, p3 * 100.0, p5 * 100.0,
                dist, n * 5);

            totals.n += n;
            totals.hit1 += hit1;
            totals.hit3 += hit3;
            totals.hit5 += hit5;
            totals.rr += cr.iter().map(|r| r.rr).sum::<f32>();
            totals.p1 += cr.iter().map(|r| r.prec_1).sum::<f32>();
            totals.p3 += cr.iter().map(|r| r.prec_3).sum::<f32>();
            totals.p5 += cr.iter().map(|r| r.prec_5).sum::<f32>();
            totals.dist += dist;
        }

        let n = totals.n as f32;
        println!("  {}", thin);
        println!("  {:<14} {:>5} {:>6.0}% {:>6.0}% {:>6.0}% {:>7.3} {:>6.0}% {:>6.0}% {:>6.0}%  {:>5}/{:<4}",
            "OVERALL", totals.n,
            100.0 * totals.hit1 as f32 / n,
            100.0 * totals.hit3 as f32 / n,
            100.0 * totals.hit5 as f32 / n,
            totals.rr / n,
            totals.p1 / n * 100.0,
            totals.p3 / n * 100.0,
            totals.p5 / n * 100.0,
            totals.dist, totals.n * 5);
        println!();

        // Distractor analysis
        let disambig_results: Vec<&QueryResult> = results
            .iter()
            .filter(|r| {
                r.category == "disambig" || r.category == "paraphrase" || r.category == "exact"
            })
            .collect();
        let total_distractors: usize = disambig_results.iter().map(|r| r.distractor_count).sum();
        let distractor_rate = if !disambig_results.is_empty() {
            total_distractors as f32 / disambig_results.len() as f32
        } else {
            0.0
        };
        println!("  DISTRACTOR ANALYSIS (disambig + paraphrase + exact)");
        println!(
            "  Queries with distractors in top-5: {}/{}",
            disambig_results
                .iter()
                .filter(|r| r.distractor_count > 0)
                .count(),
            disambig_results.len()
        );
        println!(
            "  Total distractor hits: {} (avg {:.2} per query)",
            total_distractors, distractor_rate
        );
        println!();

        // Hard-fail assertions
        let overall_hit3 = totals.hit3 as f32 / totals.n as f32;
        let overall_mrr = totals.rr / totals.n as f32;
        let overall_hit1 = totals.hit1 as f32 / totals.n as f32;
        assert!(
            overall_hit3 >= 0.75,
            "FAIL: Hit@3 = {:.0}% (threshold 75%)",
            overall_hit3 * 100.0
        );
        assert!(
            overall_mrr >= 0.60,
            "FAIL: MRR = {:.3} (threshold 0.60)",
            overall_mrr
        );
        assert!(
            overall_hit1 >= 0.55,
            "FAIL: Hit@1 = {:.0}% (threshold 55%)",
            overall_hit1 * 100.0
        );
        println!("  All assertions passed:");
        println!("    Hit@1 {:.0}% >= 55%", overall_hit1 * 100.0);
        println!("    Hit@3 {:.0}% >= 75%", overall_hit3 * 100.0);
        println!("    MRR   {:.3} >= 0.60", overall_mrr);
        println!();
    }

    #[test]
    fn test_cosine_similar_vectors() {
        // Normalized vectors pointing roughly the same direction
        let norm = |v: &mut Vec<f32>| {
            let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter_mut().for_each(|x| *x /= n);
        };
        let mut a = vec![1.0, 0.9, 0.1];
        let mut b = vec![0.9, 1.0, 0.2];
        norm(&mut a);
        norm(&mut b);
        let sim = cosine_similarity(&a, &b);
        assert!(sim > 0.95, "Expected high similarity, got {}", sim);
    }
}

fn download_file(url: &str, dest: &Path) -> Result<()> {
    let response = reqwest::blocking::get(url)
        .map_err(|e| SparksError::Internal(format!("Download failed for {}: {}", url, e)))?;

    if !response.status().is_success() {
        return Err(SparksError::Internal(format!(
            "Download failed for {}: HTTP {}",
            url,
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .map_err(|e| SparksError::Internal(format!("Failed to read response body: {}", e)))?;

    std::fs::write(dest, &bytes)
        .map_err(|e| SparksError::Internal(format!("Failed to write {}: {}", dest.display(), e)))?;

    Ok(())
}
