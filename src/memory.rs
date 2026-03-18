use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

use chrono::{NaiveDateTime, Utc};
use hnsw_rs::prelude::{DistCosine, Hnsw};
use rusqlite::Connection;

use crate::embeddings::cosine_similarity;
use crate::error::{SparksError, Result};

/// Minimum cosine similarity to keep a semantic result.
pub const SEMANTIC_THRESHOLD: f32 = 0.25;

/// Aggregate statistics about the active memory store.
#[derive(Debug, Default)]
pub struct MemoryStats {
    pub total_entries: usize,
    pub estimated_duplicates: usize,
    pub oldest_entry: Option<String>,
    pub newest_entry: Option<String>,
    pub avg_age_days: f64,
}

/// Compute the exponential decay multiplier for a memory entry.
///
/// Uses: `score = exp(-ln(2) / half_life_days * age_days)`
///
/// Returns 1.0 when `half_life_days <= 0` (no decay).
pub fn decay_score(created_at_iso: &str, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let age_days = {
        let now = chrono::Utc::now();
        let parsed = chrono::NaiveDateTime::parse_from_str(created_at_iso, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(created_at_iso, "%Y-%m-%dT%H:%M:%SZ")
            })
            .ok()
            .map(|dt| {
                chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc)
            });
        match parsed {
            Some(t) => (now - t).num_seconds().max(0) as f64 / 86400.0,
            None => 0.0,
        }
    };
    let lambda = std::f64::consts::LN_2 / half_life_days;
    (-lambda * age_days).exp()
}

/// Cosine similarity between two arbitrary (non-normalized) vectors.
///
/// Returns 0.0 for empty or mismatched-length slices.
/// Note: memory.rs normally uses `crate::embeddings::cosine_similarity` which
/// assumes pre-normalized vectors. This standalone version works on any vector.
#[allow(dead_code)]
pub fn cosine_similarity_raw(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: String,
    pub category: String,
    pub content: String,
    // Filtering by active is done in SQL (WHERE active = 1); not read in Rust
    #[allow(dead_code, reason = "retained for serde/db compatibility")]
    pub active: bool,
    pub created_at: String,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RetrievalCacheStats {
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub invalidations: u64,
    pub stale_rejections: u64,
    pub hit_ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RetrievalCacheKey {
    normalized_query: String,
    limit: usize,
    embedding_fingerprint: Option<u64>,
}

impl RetrievalCacheKey {
    fn new(query: &str, query_embedding: Option<&[f32]>, limit: usize) -> Self {
        Self {
            normalized_query: normalize_query(query),
            limit,
            embedding_fingerprint: query_embedding.map(fingerprint_embedding),
        }
    }
}

#[derive(Debug, Clone)]
struct RetrievalCacheEntry {
    results: Vec<Memory>,
    generation: u64,
    inserted_at: Instant,
}

#[derive(Debug)]
struct RetrievalLruCache {
    capacity: usize,
    entries: HashMap<RetrievalCacheKey, RetrievalCacheEntry>,
    order: VecDeque<RetrievalCacheKey>,
    hits: u64,
    misses: u64,
    evictions: u64,
    invalidations: u64,
    stale_rejections: u64,
}

impl RetrievalLruCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: HashMap::new(),
            order: VecDeque::new(),
            hits: 0,
            misses: 0,
            evictions: 0,
            invalidations: 0,
            stale_rejections: 0,
        }
    }

    fn get(&mut self, key: &RetrievalCacheKey, generation: u64) -> Option<Vec<Memory>> {
        if self.capacity == 0 {
            self.misses += 1;
            return None;
        }

        let cached = self
            .entries
            .get(key)
            .map(|entry| (entry.generation, entry.results.clone(), entry.inserted_at));
        match cached {
            Some((entry_generation, results, _inserted_at)) if entry_generation == generation => {
                self.hits += 1;
                self.touch(key);
                Some(results)
            }
            Some(_) => {
                self.misses += 1;
                self.stale_rejections += 1;
                self.remove(key);
                None
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    fn put(&mut self, key: RetrievalCacheKey, results: Vec<Memory>, generation: u64) {
        if self.capacity == 0 {
            return;
        }

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.results = results;
            entry.generation = generation;
            entry.inserted_at = Instant::now();
            self.touch(&key);
            return;
        }

        while self.entries.len() >= self.capacity {
            self.evict_lru();
        }

        self.order.push_back(key.clone());
        self.entries.insert(
            key,
            RetrievalCacheEntry {
                results,
                generation,
                inserted_at: Instant::now(),
            },
        );
    }

    fn invalidate_all(&mut self) {
        self.invalidations += 1;
        self.entries.clear();
        self.order.clear();
    }

    #[cfg(test)]
    fn reset_stats(&mut self) {
        self.hits = 0;
        self.misses = 0;
        self.evictions = 0;
        self.invalidations = 0;
        self.stale_rejections = 0;
    }

    #[cfg(test)]
    fn snapshot(&self, generation: u64) -> RetrievalCacheStats {
        let total = self.hits + self.misses;
        let hit_ratio = if total > 0 {
            self.hits as f64 / total as f64
        } else {
            0.0
        };
        let _ = generation;
        RetrievalCacheStats {
            entries: self.entries.len(),
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            invalidations: self.invalidations,
            stale_rejections: self.stale_rejections,
            hit_ratio,
        }
    }

    fn touch(&mut self, key: &RetrievalCacheKey) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.clone());
    }

    fn remove(&mut self, key: &RetrievalCacheKey) {
        self.entries.remove(key);
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
    }

    fn evict_lru(&mut self) {
        if let Some(oldest) = self.order.pop_front() {
            self.entries.remove(&oldest);
            self.evictions += 1;
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HnswIndexConfig {
    pub enabled: bool,
    pub min_index_size: usize,
    pub m: usize,
    pub ef_construction: usize,
    pub ef_search: usize,
}

impl Default for HnswIndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_index_size: 64,
            m: 16,
            ef_construction: 200,
            ef_search: 64,
        }
    }
}

struct HnswSemanticIndex {
    graph: Hnsw<'static, f32, DistCosine>,
    memory_ids: Vec<String>,
}

impl std::fmt::Debug for HnswSemanticIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HnswSemanticIndex")
            .field("points", &self.memory_ids.len())
            .finish()
    }
}

#[derive(Debug)]
struct SemanticIndexState {
    index: Option<HnswSemanticIndex>,
    dirty: bool,
}

impl SemanticIndexState {
    fn new() -> Self {
        Self {
            index: None,
            dirty: true,
        }
    }
}

pub struct MemoryStore {
    conn: Mutex<Connection>,
    embedding_cache: RwLock<HashMap<String, Vec<f32>>>,
    retrieval_cache: Mutex<RetrievalLruCache>,
    semantic_index: RwLock<SemanticIndexState>,
    memory_generation: AtomicU64,
    recency_half_life_days: f32,
    dedup_threshold: f32,
    hnsw: HnswIndexConfig,
}

impl MemoryStore {
    pub fn new_with_hnsw(
        conn: Connection,
        recency_half_life_days: f32,
        dedup_threshold: f32,
        retrieval_cache_capacity: usize,
        hnsw: HnswIndexConfig,
    ) -> Self {
        let store = Self {
            conn: Mutex::new(conn),
            embedding_cache: RwLock::new(HashMap::new()),
            retrieval_cache: Mutex::new(RetrievalLruCache::new(retrieval_cache_capacity)),
            semantic_index: RwLock::new(SemanticIndexState::new()),
            memory_generation: AtomicU64::new(0),
            recency_half_life_days,
            dedup_threshold,
            hnsw,
        };
        if let Err(e) = store.load_embedding_cache() {
            tracing::warn!("Failed to load embedding cache: {}", e);
        }
        store
    }

    /// Safely acquire the database connection lock
    fn conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| SparksError::Internal(format!("Database lock poisoned: {}", e)))
    }

    fn current_memory_generation(&self) -> u64 {
        self.memory_generation.load(Ordering::Relaxed)
    }

    fn invalidate_retrieval_cache(&self) {
        let generation = self.memory_generation.fetch_add(1, Ordering::Relaxed) + 1;
        self.mark_semantic_index_dirty();
        match self.retrieval_cache.lock() {
            Ok(mut cache) => cache.invalidate_all(),
            Err(e) => {
                tracing::warn!(
                    generation,
                    "Retrieval cache lock poisoned during invalidation: {}",
                    e
                );
            }
        }
    }

    fn mark_semantic_index_dirty(&self) {
        match self.semantic_index.write() {
            Ok(mut state) => {
                state.dirty = true;
            }
            Err(e) => {
                tracing::warn!("Semantic index lock poisoned while marking dirty: {}", e);
            }
        }
    }

    fn lookup_retrieval_cache(&self, key: &RetrievalCacheKey) -> Option<Vec<Memory>> {
        let generation = self.current_memory_generation();
        match self.retrieval_cache.lock() {
            Ok(mut cache) => cache.get(key, generation),
            Err(e) => {
                tracing::warn!("Retrieval cache lock poisoned during lookup: {}", e);
                None
            }
        }
    }

    fn fill_retrieval_cache(&self, key: RetrievalCacheKey, results: &[Memory]) {
        let generation = self.current_memory_generation();
        if let Ok(mut cache) = self.retrieval_cache.lock() {
            cache.put(key, results.to_vec(), generation);
        } else {
            tracing::warn!("Retrieval cache lock poisoned during fill");
        }
    }

    #[cfg(test)]
    pub fn retrieval_cache_stats(&self) -> RetrievalCacheStats {
        let generation = self.current_memory_generation();
        match self.retrieval_cache.lock() {
            Ok(cache) => cache.snapshot(generation),
            Err(e) => {
                tracing::warn!("Retrieval cache lock poisoned while reading stats: {}", e);
                let _ = generation;
                RetrievalCacheStats::default()
            }
        }
    }

    #[cfg(test)]
    pub fn reset_retrieval_cache_stats(&self) {
        if let Ok(mut cache) = self.retrieval_cache.lock() {
            cache.reset_stats();
        } else {
            tracing::warn!("Retrieval cache lock poisoned while resetting stats");
        }
    }

    #[cfg(test)]
    pub fn semantic_index_point_count(&self) -> usize {
        match self.semantic_index.read() {
            Ok(state) => state
                .index
                .as_ref()
                .map(|index| index.memory_ids.len())
                .unwrap_or(0),
            Err(e) => {
                tracing::warn!("Semantic index lock poisoned while reading status: {}", e);
                0
            }
        }
    }

    /// Load all active embeddings into the in-memory cache.
    fn load_embedding_cache(&self) -> Result<()> {
        let pairs = {
            let conn = self.conn()?;
            let mut stmt = conn.prepare(
                "SELECT id, embedding FROM memories WHERE active = 1 AND embedding IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |row| {
                let id: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok((id, blob))
            })?;
            let mut pairs = Vec::new();
            for row in rows {
                pairs.push(row?);
            }
            pairs
        };

        let mut cache = self
            .embedding_cache
            .write()
            .map_err(|e| SparksError::Internal(format!("Embedding cache lock poisoned: {}", e)))?;
        for (id, blob) in pairs {
            if let Some(emb) = blob_to_embedding(&blob) {
                cache.insert(id, emb);
            }
        }
        tracing::info!("Loaded {} embeddings into cache", cache.len());
        Ok(())
    }

    /// Find a near-duplicate memory by cosine similarity against the embedding cache.
    fn find_duplicate(&self, embedding: &[f32]) -> Option<(String, f32)> {
        let cache = self.embedding_cache.read().ok()?;
        let mut best_id = None;
        let mut best_sim = 0.0f32;
        for (id, stored_emb) in cache.iter() {
            let sim = cosine_similarity(embedding, stored_emb);
            if sim > best_sim {
                best_sim = sim;
                best_id = Some(id.clone());
            }
        }
        if best_sim >= self.dedup_threshold {
            best_id.map(|id| (id, best_sim))
        } else {
            None
        }
    }

    /// Store a new memory, optionally with a precomputed embedding vector.
    /// If a near-duplicate exists (above dedup_threshold), updates it instead.
    pub fn store(
        &self,
        category: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> Result<String> {
        // Check for dedup
        if let Some(emb) = embedding {
            if let Some((dup_id, sim)) = self.find_duplicate(emb) {
                tracing::info!(
                    "Deduplicated memory: {} (similarity: {:.3})",
                    &dup_id[..8.min(dup_id.len())],
                    sim
                );
                let conn = self.conn()?;
                let rowid: i64 = conn.query_row(
                    "SELECT rowid FROM memories WHERE id = ?1",
                    rusqlite::params![&dup_id],
                    |r| r.get(0),
                )?;
                let blob = embedding_to_blob(emb);
                conn.execute(
                    "UPDATE memories SET category = ?1, content = ?2, embedding = ?3, updated_at = datetime('now') WHERE id = ?4",
                    rusqlite::params![category, content, blob, &dup_id],
                )?;
                // Update FTS
                let _ = conn.execute(
                    "DELETE FROM memories_fts WHERE rowid = ?1",
                    rusqlite::params![rowid],
                );
                let _ = conn.execute(
                    "INSERT INTO memories_fts(rowid, content) VALUES (?1, ?2)",
                    rusqlite::params![rowid, content],
                );
                // Update cache
                if let Ok(mut cache) = self.embedding_cache.write() {
                    cache.insert(dup_id.clone(), emb.to_vec());
                }
                self.invalidate_retrieval_cache();
                return Ok(dup_id);
            }
        }

        // Normal insert
        let id = uuid::Uuid::new_v4().to_string();
        let conn = self.conn()?;
        let blob = embedding.map(embedding_to_blob);
        conn.execute(
            "INSERT INTO memories (id, category, content, embedding) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, category, content, blob],
        )?;
        // Insert into FTS
        let rowid = conn.last_insert_rowid();
        let _ = conn.execute(
            "INSERT INTO memories_fts(rowid, content) VALUES (?1, ?2)",
            rusqlite::params![rowid, content],
        );
        // Update cache
        if let Some(emb) = embedding {
            if let Ok(mut cache) = self.embedding_cache.write() {
                cache.insert(id.clone(), emb.to_vec());
            }
        }
        self.invalidate_retrieval_cache();
        Ok(id)
    }

    /// Search memories by keyword (simple LIKE match with proper escaping)
    pub fn search(&self, query: &str) -> Result<Vec<Memory>> {
        let conn = self.conn()?;
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{}%", escaped);
        let mut stmt = conn.prepare(
            "SELECT id, category, content, active, created_at FROM memories
             WHERE active = 1 AND (content LIKE ?1 ESCAPE '\\' OR category LIKE ?1 ESCAPE '\\')
             ORDER BY created_at DESC LIMIT 10",
        )?;
        let rows = stmt.query_map(rusqlite::params![pattern], |row| {
            Ok(Memory {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                active: row.get::<_, i32>(3)? != 0,
                created_at: row.get(4)?,
            })
        })?;
        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// FTS5 full-text search with BM25 ranking.
    fn search_fts(&self, query: &str) -> Result<Vec<(Memory, f32)>> {
        let keywords = extract_keywords(query);
        if keywords.is_empty() {
            return Ok(vec![]);
        }

        let fts_query = keywords
            .iter()
            .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");

        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT m.id, m.category, m.content, m.active, m.created_at, fts.rank
             FROM memories_fts fts
             JOIN memories m ON m.rowid = fts.rowid
             WHERE memories_fts MATCH ?1 AND m.active = 1
             ORDER BY fts.rank
             LIMIT 20",
        )?;
        let rows = stmt.query_map(rusqlite::params![fts_query], |row| {
            Ok((
                Memory {
                    id: row.get(0)?,
                    category: row.get(1)?,
                    content: row.get(2)?,
                    active: row.get::<_, i32>(3)? != 0,
                    created_at: row.get(4)?,
                },
                row.get::<_, f64>(5)? as f32,
            ))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let (m, rank) = row?;
            // BM25 rank is negative (lower = better). Normalize to 0..1.
            let score = (-rank).clamp(0.0, 5.0) / 5.0;
            results.push((m, score));
        }
        Ok(results)
    }

    fn search_semantic_exact(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(Memory, f32)>> {
        // Phase 1: compute similarities in memory (only hold cache read lock)
        let id_scores = {
            let cache = self.embedding_cache.read().map_err(|e| {
                SparksError::Internal(format!("Embedding cache lock poisoned: {}", e))
            })?;
            let mut id_scores: Vec<(String, f32)> = cache
                .iter()
                .map(|(id, emb)| (id.clone(), cosine_similarity(query_embedding, emb)))
                .collect();
            id_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            id_scores.truncate(limit);
            id_scores
        };

        if id_scores.is_empty() {
            return Ok(vec![]);
        }

        // Phase 2: fetch memory records from DB for top results
        let conn = self.conn()?;
        let mut results = Vec::new();
        for (id, score) in id_scores {
            match conn.query_row(
                "SELECT id, category, content, active, created_at FROM memories WHERE id = ?1 AND active = 1",
                rusqlite::params![id],
                |row| {
                    Ok(Memory {
                        id: row.get(0)?,
                        category: row.get(1)?,
                        content: row.get(2)?,
                        active: row.get::<_, i32>(3)? != 0,
                        created_at: row.get(4)?,
                    })
                },
            ) {
                Ok(m) => results.push((m, score)),
                Err(_) => continue,
            }
        }
        Ok(results)
    }

    fn rebuild_semantic_index(&self) -> Result<bool> {
        if !self.hnsw.enabled {
            return Ok(false);
        }

        let snapshot: Vec<(String, Vec<f32>)> = {
            let cache = self.embedding_cache.read().map_err(|e| {
                SparksError::Internal(format!("Embedding cache lock poisoned: {}", e))
            })?;
            cache
                .iter()
                .map(|(id, emb)| (id.clone(), emb.clone()))
                .collect()
        };

        if snapshot.len() < self.hnsw.min_index_size {
            if let Ok(mut state) = self.semantic_index.write() {
                state.index = None;
                state.dirty = false;
            }
            return Ok(false);
        }

        let max_nb_connection = self.hnsw.m.clamp(2, 256);
        let ef_construction = self.hnsw.ef_construction.max(max_nb_connection);
        let max_elements = snapshot.len().max(self.hnsw.min_index_size) * 2;
        let max_layer = 16usize.min(((snapshot.len() as f32).ln().ceil() as usize).max(1));
        let graph = Hnsw::<f32, DistCosine>::new(
            max_nb_connection,
            max_elements,
            max_layer,
            ef_construction,
            DistCosine {},
        );

        let mut memory_ids = Vec::with_capacity(snapshot.len());
        for (data_id, (memory_id, emb)) in snapshot.into_iter().enumerate() {
            graph.insert((emb.as_slice(), data_id));
            memory_ids.push(memory_id);
        }

        let mut state = self.semantic_index.write().map_err(|e| {
            SparksError::Internal(format!(
                "Semantic index lock poisoned during rebuild: {}",
                e
            ))
        })?;
        state.index = Some(HnswSemanticIndex { graph, memory_ids });
        state.dirty = false;

        Ok(true)
    }

    fn semantic_index_needs_rebuild(&self) -> Option<bool> {
        match self.semantic_index.read() {
            Ok(state) => Some(state.dirty || state.index.is_none()),
            Err(e) => {
                tracing::warn!("Semantic index lock poisoned during read: {}", e);
                None
            }
        }
    }

    fn hnsw_candidate_ids(&self, query_embedding: &[f32], limit: usize) -> Option<Vec<String>> {
        match self.semantic_index.read() {
            Ok(state) => {
                let index = state.index.as_ref()?;
                let ef_search = self.hnsw.ef_search.max(limit);
                Some(
                    index
                        .graph
                        .search(query_embedding, limit, ef_search)
                        .into_iter()
                        .filter_map(|n| index.memory_ids.get(n.d_id).cloned())
                        .collect(),
                )
            }
            Err(e) => {
                tracing::warn!("Semantic index lock poisoned during search: {}", e);
                None
            }
        }
    }

    fn score_hnsw_candidates(
        &self,
        query_embedding: &[f32],
        candidate_ids: &[String],
    ) -> Result<HashMap<String, f32>> {
        let cache = self
            .embedding_cache
            .read()
            .map_err(|e| SparksError::Internal(format!("Embedding cache lock poisoned: {}", e)))?;
        Ok(candidate_ids
            .iter()
            .filter_map(|id| {
                cache
                    .get(id)
                    .map(|emb| (id.clone(), cosine_similarity(query_embedding, emb)))
            })
            .collect())
    }

    fn fetch_memories_with_scores(
        &self,
        candidate_ids: Vec<String>,
        score_map: &HashMap<String, f32>,
        limit: usize,
    ) -> Result<Vec<(Memory, f32)>> {
        let conn = self.conn()?;
        let mut results = Vec::new();
        for id in candidate_ids {
            let Some(score) = score_map.get(&id).copied() else {
                continue;
            };
            match conn.query_row(
                "SELECT id, category, content, active, created_at FROM memories WHERE id = ?1 AND active = 1",
                rusqlite::params![id],
                |row| {
                    Ok(Memory {
                        id: row.get(0)?,
                        category: row.get(1)?,
                        content: row.get(2)?,
                        active: row.get::<_, i32>(3)? != 0,
                        created_at: row.get(4)?,
                    })
                },
            ) {
                Ok(m) => results.push((m, score)),
                Err(_) => continue,
            }
        }
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        Ok(results)
    }

    fn search_semantic_hnsw(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Option<Vec<(Memory, f32)>>> {
        if !self.hnsw.enabled || limit == 0 {
            return Ok(None);
        }
        let Some(needs_rebuild) = self.semantic_index_needs_rebuild() else {
            return Ok(None);
        };
        if needs_rebuild {
            if let Err(e) = self.rebuild_semantic_index() {
                tracing::warn!("Failed to rebuild HNSW semantic index: {}", e);
                return Ok(None);
            }
        }
        let Some(candidate_ids) = self.hnsw_candidate_ids(query_embedding, limit) else {
            return Ok(None);
        };
        if candidate_ids.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let score_map = self.score_hnsw_candidates(query_embedding, &candidate_ids)?;
        let results = self.fetch_memories_with_scores(candidate_ids, &score_map, limit)?;
        Ok(Some(results))
    }

    /// Semantic search: HNSW ANN (with exact fallback) over in-memory embeddings.
    pub fn search_semantic(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(Memory, f32)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if let Some(results) = self.search_semantic_hnsw(query_embedding, limit)? {
            return Ok(results);
        }
        self.search_semantic_exact(query_embedding, limit)
    }

    /// Hybrid search: FTS5 keyword + semantic, merged with time decay.
    pub fn search_hybrid(
        &self,
        query: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let cache_key = RetrievalCacheKey::new(query, query_embedding, limit);
        if let Some(cached) = self.lookup_retrieval_cache(&cache_key) {
            return Ok(cached);
        }

        let mut scored: HashMap<String, (Memory, f32)> = HashMap::new();

        // 1. FTS5 keyword search with BM25 ranking
        for (m, fts_score) in self.search_fts(query)? {
            scored.insert(m.id.clone(), (m, fts_score));
        }

        // 2. Semantic search
        if let Some(emb) = query_embedding {
            for (m, sim) in self.search_semantic(emb, limit * 2)? {
                if sim < SEMANTIC_THRESHOLD {
                    continue;
                }
                scored
                    .entry(m.id.clone())
                    .and_modify(|(_, score)| {
                        *score += sim;
                    })
                    .or_insert((m, sim));
            }
        }

        // 3. Apply time decay
        for (_, (m, score)) in scored.iter_mut() {
            *score *= time_decay_factor(&m.created_at, self.recency_half_life_days);
        }

        // 4. Sort by score descending, take top-K
        let mut results: Vec<(Memory, f32)> = scored.into_values().collect();
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);
        let final_results: Vec<Memory> = results.into_iter().map(|(m, _)| m).collect();
        self.fill_retrieval_cache(cache_key, &final_results);
        Ok(final_results)
    }

    /// Update a single memory's embedding (for backfilling existing records).
    pub fn backfill_embedding(&self, id: &str, embedding: &[f32]) -> Result<()> {
        let conn = self.conn()?;
        let blob = embedding_to_blob(embedding);
        conn.execute(
            "UPDATE memories SET embedding = ?1 WHERE id = ?2",
            rusqlite::params![blob, id],
        )?;
        drop(conn);
        // Update cache
        if let Ok(mut cache) = self.embedding_cache.write() {
            cache.insert(id.to_string(), embedding.to_vec());
        }
        self.invalidate_retrieval_cache();
        Ok(())
    }

    /// Return IDs and content of active memories that have no embedding yet.
    pub fn memories_without_embeddings(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT id, content FROM memories WHERE active = 1 AND embedding IS NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// List all active memories
    pub fn list(&self) -> Result<Vec<Memory>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, category, content, active, created_at FROM memories
             WHERE active = 1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Memory {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                active: row.get::<_, i32>(3)? != 0,
                created_at: row.get(4)?,
            })
        })?;
        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// List active memories in a category, newest first, optionally bounded by recency.
    pub fn list_by_category_recent(
        &self,
        category: &str,
        limit: usize,
        within_days: i64,
    ) -> Result<Vec<Memory>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, category, content, active, created_at FROM memories
             WHERE active = 1
               AND category = ?1
               AND created_at >= datetime('now', ?2)
             ORDER BY created_at DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![
                category,
                format!("-{} days", within_days.max(1)),
                limit as i64
            ],
            |row| {
                Ok(Memory {
                    id: row.get(0)?,
                    category: row.get(1)?,
                    content: row.get(2)?,
                    active: row.get::<_, i32>(3)? != 0,
                    created_at: row.get(4)?,
                })
            },
        )?;
        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// List active memories from any of the provided categories, newest first,
    /// bounded by an exact recency window in hours.
    pub fn list_by_categories_recent_hours(
        &self,
        categories: &[&str],
        limit: usize,
        within_hours: i64,
    ) -> Result<Vec<Memory>> {
        if categories.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn()?;
        let placeholders = std::iter::repeat_n("?", categories.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, category, content, active, created_at FROM memories
             WHERE active = 1
               AND category IN ({})
               AND created_at >= datetime('now', ?{})
             ORDER BY created_at DESC
             LIMIT ?{}",
            placeholders,
            categories.len() + 1,
            categories.len() + 2
        );
        let mut values: Vec<String> = categories.iter().map(|c| (*c).to_string()).collect();
        values.push(format!("-{} hours", within_hours.max(1)));
        values.push(limit.to_string());
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok(Memory {
                id: row.get(0)?,
                category: row.get(1)?,
                content: row.get(2)?,
                active: row.get::<_, i32>(3)? != 0,
                created_at: row.get(4)?,
            })
        })?;
        let mut memories = Vec::new();
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// Retire a memory (soft delete)
    pub fn retire(&self, id: &str) -> Result<bool> {
        let conn = self.conn()?;
        // Get rowid before retiring (for FTS cleanup)
        let rowid: Option<i64> = conn
            .query_row(
                "SELECT rowid FROM memories WHERE id = ?1 AND active = 1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .ok();

        let updated = conn.execute(
            "UPDATE memories SET active = 0, updated_at = datetime('now') WHERE id = ?1",
            rusqlite::params![id],
        )?;

        if updated > 0 {
            // Remove from FTS
            if let Some(rowid) = rowid {
                let _ = conn.execute(
                    "DELETE FROM memories_fts WHERE rowid = ?1",
                    rusqlite::params![rowid],
                );
            }
            drop(conn);
            // Remove from embedding cache
            if let Ok(mut cache) = self.embedding_cache.write() {
                cache.remove(id);
            }
            self.invalidate_retrieval_cache();
        }
        Ok(updated > 0)
    }

    /// Save a conversation turn.
    pub fn save_turn(&self, session_key: &str, role: &str, content: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO conversations (session_key, role, content) VALUES (?1, ?2, ?3)",
            rusqlite::params![session_key, role, content],
        )?;
        Ok(())
    }

    /// Get recent turns for a session (last N messages, returned in chronological order).
    pub fn recent_turns(&self, session_key: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT role, content FROM conversations
             WHERE session_key = ?1
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_key, limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut turns = Vec::new();
        for row in rows {
            turns.push(row?);
        }
        turns.reverse(); // chronological order
        Ok(turns)
    }

    /// Count total turns for a session.
    pub fn turn_count(&self, session_key: &str) -> Result<usize> {
        let conn = self.conn()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM conversations WHERE session_key = ?1",
            rusqlite::params![session_key],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    /// Delete old conversation turns (older than N days).
    pub fn cleanup_conversations(&self, max_age_days: i64) -> Result<usize> {
        let conn = self.conn()?;
        let deleted = conn.execute(
            "DELETE FROM conversations WHERE created_at < datetime('now', ?1)",
            rusqlite::params![format!("-{} days", max_age_days)],
        )?;
        Ok(deleted)
    }

    /// Get all profile key-value pairs for a user.
    pub fn get_user_profile(&self, user_id: &str) -> Result<HashMap<String, String>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT key, value FROM user_profiles WHERE user_id = ?1")?;
        let rows = stmt.query_map(rusqlite::params![user_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut profile = HashMap::new();
        for row in rows {
            let (k, v) = row?;
            profile.insert(k, v);
        }
        Ok(profile)
    }

    // --- Mood state persistence ---

    /// Load the singleton mood state row.
    pub fn load_mood_state(&self) -> Result<(f32, f32, String)> {
        let conn = self.conn()?;
        conn.query_row(
            "SELECT energy, valence, active_modifier FROM mood_state WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, f64>(0)? as f32,
                    row.get::<_, f64>(1)? as f32,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|e| SparksError::Db(e))
    }

    /// Persist mood state.
    pub fn save_mood_state(&self, energy: f32, valence: f32, modifier: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE mood_state SET energy = ?1, valence = ?2, active_modifier = ?3, updated_at = datetime('now') WHERE id = 1",
            rusqlite::params![energy as f64, valence as f64, modifier],
        )?;
        Ok(())
    }

    // --- Scheduled jobs ---

    pub fn create_scheduled_job(
        &self,
        id: &str,
        name: &str,
        schedule_type: &str,
        schedule_data: &str,
        ghost: Option<&str>,
        prompt: &str,
        target: &str,
        next_run: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO scheduled_jobs (id, name, schedule_type, schedule_data, ghost, prompt, target, next_run)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                id,
                name,
                schedule_type,
                schedule_data,
                ghost,
                prompt,
                target,
                next_run
            ],
        )?;
        Ok(())
    }

    pub fn list_scheduled_jobs(&self) -> Result<Vec<crate::scheduler::Job>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, schedule_type, schedule_data, ghost, prompt, target, enabled, next_run, last_run
             FROM scheduled_jobs ORDER BY created_at"
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(JobRow {
                id: row.get(0)?,
                name: row.get(1)?,
                schedule_type: row.get(2)?,
                schedule_data: row.get(3)?,
                ghost: row.get(4)?,
                prompt: row.get(5)?,
                target: row.get(6)?,
                enabled: row.get::<_, i32>(7)? != 0,
                next_run: row.get(8)?,
                last_run: row.get(9)?,
            })
        })?;

        let mut jobs = Vec::new();
        for row in rows {
            let r = row?;
            let schedule = crate::scheduler::Schedule::from_db(&r.schedule_type, &r.schedule_data)
                .unwrap_or(crate::scheduler::Schedule::Interval {
                    every_secs: 3600,
                    jitter: 0.1,
                });
            let next_run = r
                .next_run
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));
            let last_run = r
                .last_run
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));
            jobs.push(crate::scheduler::Job {
                id: r.id,
                name: r.name,
                schedule,
                ghost: r.ghost,
                prompt: r.prompt,
                target: r.target,
                enabled: r.enabled,
                next_run,
                last_run,
            });
        }
        Ok(jobs)
    }

    pub fn due_scheduled_jobs(&self) -> Result<Vec<crate::scheduler::Job>> {
        let conn = self.conn()?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut stmt = conn.prepare(
            "SELECT id, name, schedule_type, schedule_data, ghost, prompt, target, enabled, next_run, last_run
             FROM scheduled_jobs WHERE enabled = 1 AND next_run IS NOT NULL AND next_run <= ?1"
        )?;
        let rows = stmt.query_map(rusqlite::params![now], |row| {
            Ok(JobRow {
                id: row.get(0)?,
                name: row.get(1)?,
                schedule_type: row.get(2)?,
                schedule_data: row.get(3)?,
                ghost: row.get(4)?,
                prompt: row.get(5)?,
                target: row.get(6)?,
                enabled: row.get::<_, i32>(7)? != 0,
                next_run: row.get(8)?,
                last_run: row.get(9)?,
            })
        })?;

        let mut jobs = Vec::new();
        for row in rows {
            let r = row?;
            let schedule = crate::scheduler::Schedule::from_db(&r.schedule_type, &r.schedule_data)
                .unwrap_or(crate::scheduler::Schedule::Interval {
                    every_secs: 3600,
                    jitter: 0.1,
                });
            let next_run = r
                .next_run
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));
            let last_run = r
                .last_run
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));
            jobs.push(crate::scheduler::Job {
                id: r.id,
                name: r.name,
                schedule,
                ghost: r.ghost,
                prompt: r.prompt,
                target: r.target,
                enabled: r.enabled,
                next_run,
                last_run,
            });
        }
        Ok(jobs)
    }

    pub fn update_job_run(
        &self,
        id: &str,
        next_run: Option<&str>,
        last_run: &str,
        disable: bool,
    ) -> Result<()> {
        let conn = self.conn()?;
        if disable {
            conn.execute(
                "UPDATE scheduled_jobs SET next_run = ?1, last_run = ?2, enabled = 0 WHERE id = ?3",
                rusqlite::params![next_run, last_run, id],
            )?;
        } else {
            conn.execute(
                "UPDATE scheduled_jobs SET next_run = ?1, last_run = ?2 WHERE id = ?3",
                rusqlite::params![next_run, last_run, id],
            )?;
        }
        Ok(())
    }

    pub fn delete_scheduled_job(&self, id: &str) -> Result<bool> {
        let conn = self.conn()?;
        let deleted = conn.execute(
            "DELETE FROM scheduled_jobs WHERE id = ?1",
            rusqlite::params![id],
        )?;
        Ok(deleted > 0)
    }

    pub fn delete_scheduled_jobs_by_name(&self, name: &str) -> Result<usize> {
        let conn = self.conn()?;
        let deleted = conn.execute(
            "DELETE FROM scheduled_jobs WHERE name = ?1",
            rusqlite::params![name],
        )?;
        Ok(deleted as usize)
    }

    pub fn toggle_scheduled_job(&self, id: &str, enabled: bool) -> Result<bool> {
        let conn = self.conn()?;
        let updated = conn.execute(
            "UPDATE scheduled_jobs SET enabled = ?1 WHERE id = ?2",
            rusqlite::params![enabled as i32, id],
        )?;
        Ok(updated > 0)
    }

    pub fn cleanup_stale_disabled_oneshots(&self) -> Result<usize> {
        let conn = self.conn()?;
        let deleted = conn.execute(
            "DELETE FROM scheduled_jobs
             WHERE schedule_type = 'oneshot'
               AND enabled = 0
               AND datetime(COALESCE(last_run, created_at)) <= datetime('now', '-24 hours')",
            [],
        )?;
        Ok(deleted as usize)
    }

    // --- Relationship tracking ---

    pub fn record_relationship(&self, user_id: &str, message_length: usize) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO relationship_stats (user_id, total_interactions, last_interaction, avg_message_length)
             VALUES (?1, 1, datetime('now'), ?2)
             ON CONFLICT(user_id) DO UPDATE SET
                total_interactions = total_interactions + 1,
                last_interaction = datetime('now'),
                avg_message_length = (avg_message_length * total_interactions + ?2) / (total_interactions + 1)",
            rusqlite::params![user_id, message_length as f64],
        )?;
        Ok(())
    }

    pub fn get_relationship(&self, user_id: &str) -> Result<Option<UserRelationship>> {
        let conn = self.conn()?;
        match conn.query_row(
            "SELECT total_interactions, warmth_level
             FROM relationship_stats WHERE user_id = ?1",
            rusqlite::params![user_id],
            |row| {
                Ok(UserRelationship {
                    total_interactions: row.get(0)?,
                    warmth_level: row.get::<_, f64>(1)? as f32,
                })
            },
        ) {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SparksError::Db(e)),
        }
    }

    /// Return aggregate statistics about the active memory store.
    pub fn stats(&self) -> Result<MemoryStats> {
        let conn = self.conn()?;

        // Total active entries
        let total_entries: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE active = 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as usize;

        // Oldest and newest created_at
        let oldest_entry: Option<String> = conn
            .query_row(
                "SELECT MIN(created_at) FROM memories WHERE active = 1",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();

        let newest_entry: Option<String> = conn
            .query_row(
                "SELECT MAX(created_at) FROM memories WHERE active = 1",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();

        // Average age in days
        let avg_age_days: f64 = conn
            .query_row(
                "SELECT AVG((julianday('now') - julianday(created_at))) FROM memories WHERE active = 1",
                [],
                |r| r.get::<_, Option<f64>>(0),
            )
            .ok()
            .flatten()
            .unwrap_or(0.0);

        // Estimated duplicates: check pairs in the embedding cache (capped at 1000)
        let estimated_duplicates = {
            let pairs = self.find_duplicates(self.dedup_threshold);
            pairs.len()
        };

        Ok(MemoryStats {
            total_entries,
            estimated_duplicates,
            oldest_entry,
            newest_entry,
            avg_age_days,
        })
    }

    /// Remove active memories whose exponential decay score falls below `min_score`.
    /// Returns the number of entries pruned (or that would be pruned in dry-run mode).
    pub fn prune_decayed(&self, half_life_days: f64, min_score: f64, dry_run: bool) -> Result<usize> {
        if half_life_days <= 0.0 {
            return Ok(0);
        }
        let memories = self.list()?;
        let mut pruned = 0usize;
        for m in &memories {
            let score = decay_score(&m.created_at, half_life_days);
            if score < min_score {
                if !dry_run {
                    self.retire(&m.id)?;
                }
                pruned += 1;
            }
        }
        Ok(pruned)
    }

    /// Return pairs of active memory IDs whose embeddings have cosine similarity >= threshold.
    /// Limited to the first 1000 entries to keep complexity manageable (O(n²)).
    pub fn find_duplicates(&self, threshold: f32) -> Vec<(String, String, f32)> {
        let cache = match self.embedding_cache.read() {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        let entries: Vec<(&String, &Vec<f32>)> = cache.iter().take(1000).collect();
        let mut pairs = Vec::new();
        for i in 0..entries.len() {
            for j in (i + 1)..entries.len() {
                let sim = cosine_similarity(entries[i].1, entries[j].1);
                if sim >= threshold {
                    pairs.push((entries[i].0.clone(), entries[j].0.clone(), sim));
                }
            }
        }
        pairs
    }
}

struct JobRow {
    id: String,
    name: String,
    schedule_type: String,
    schedule_data: String,
    ghost: Option<String>,
    prompt: String,
    target: String,
    enabled: bool,
    next_run: Option<String>,
    last_run: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UserRelationship {
    pub total_interactions: i64,
    pub warmth_level: f32,
}

/// Exponential time-decay factor: 0.5^(days_old / half_life_days).
fn time_decay_factor(created_at: &str, half_life_days: f32) -> f32 {
    let Ok(created) = NaiveDateTime::parse_from_str(created_at, "%Y-%m-%d %H:%M:%S") else {
        return 1.0;
    };
    let now = Utc::now().naive_utc();
    let days_old = (now - created).num_seconds() as f32 / 86400.0;
    if days_old <= 0.0 {
        return 1.0;
    }
    0.5_f32.powf(days_old / half_life_days)
}

fn normalize_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|part| part.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

fn fingerprint_embedding(embedding: &[f32]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    embedding.len().hash(&mut hasher);
    for value in embedding {
        value.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

/// Extract significant keywords from a query string.
/// Lowercases, filters stopwords and very short words (< 2 chars).
fn extract_keywords(query: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "do", "does", "did", "will", "would", "could", "should", "may", "might", "shall", "can",
        "need", "must", "i", "me", "my", "we", "our", "you", "your", "he", "she", "it", "they",
        "them", "his", "her", "its", "this", "that", "these", "those", "what", "which", "who",
        "whom", "where", "when", "how", "why", "and", "or", "but", "if", "then", "so", "than",
        "too", "very", "of", "in", "on", "at", "to", "for", "with", "from", "by", "about", "into",
        "like", "not", "no", "all", "any", "some", "every", "tell", "know", "use", "get", "got",
        "also",
    ];

    query
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 2 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Serialize f32 slice → raw little-endian bytes for SQLite BLOB.
fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Deserialize raw little-endian bytes back to Vec<f32>.
fn blob_to_embedding(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.len() % 4 != 0 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    fn setup_test_db() -> MemoryStore {
        setup_test_db_with_config_and_cache(30.0, 1.0, 256)
        // dedup disabled in most tests
    }

    fn setup_test_db_with_config(half_life: f32, dedup_threshold: f32) -> MemoryStore {
        setup_test_db_with_config_and_cache(half_life, dedup_threshold, 256)
    }

    fn setup_test_db_with_config_and_cache(
        half_life: f32,
        dedup_threshold: f32,
        retrieval_cache_capacity: usize,
    ) -> MemoryStore {
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
            CREATE INDEX IF NOT EXISTS idx_conversations_session ON conversations(session_key, created_at);
            CREATE TABLE IF NOT EXISTS user_profiles (
                user_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (user_id, key)
            );"
        ).unwrap();
        MemoryStore::new_with_hnsw(
            conn,
            half_life,
            dedup_threshold,
            retrieval_cache_capacity,
            HnswIndexConfig::default(),
        )
    }

    fn setup_test_db_with_hnsw(
        half_life: f32,
        dedup_threshold: f32,
        retrieval_cache_capacity: usize,
        hnsw: HnswIndexConfig,
    ) -> MemoryStore {
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
            CREATE INDEX IF NOT EXISTS idx_conversations_session ON conversations(session_key, created_at);
            CREATE TABLE IF NOT EXISTS user_profiles (
                user_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (user_id, key)
            );"
        ).unwrap();
        MemoryStore::new_with_hnsw(
            conn,
            half_life,
            dedup_threshold,
            retrieval_cache_capacity,
            hnsw,
        )
    }

    fn fake_embedding(seed: f32) -> Vec<f32> {
        // Create a simple 4-dim normalized vector for testing
        let mut v = vec![seed, seed * 0.5, 1.0 - seed, 0.1];
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter_mut().for_each(|x| *x /= norm);
        v
    }

    #[test]
    fn test_store_without_embedding() {
        let store = setup_test_db();
        let id = store.store("fact", "Rust is great", None).unwrap();
        assert!(!id.is_empty());

        let memories = store.list().unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].content, "Rust is great");
    }

    #[test]
    fn test_store_with_embedding() {
        let store = setup_test_db();
        let emb = fake_embedding(0.8);
        let id = store.store("fact", "I like Python", Some(&emb)).unwrap();
        assert!(!id.is_empty());

        let memories = store.list().unwrap();
        assert_eq!(memories.len(), 1);
    }

    #[test]
    fn test_semantic_search_returns_ranked_results() {
        let store = setup_test_db();

        // Store three memories with different embeddings
        let emb1 = fake_embedding(0.9); // similar to query
        let emb2 = fake_embedding(0.1); // different from query
        let emb3 = fake_embedding(0.85); // somewhat similar

        store.store("fact", "I prefer Python", Some(&emb1)).unwrap();
        store
            .store("fact", "The weather is nice", Some(&emb2))
            .unwrap();
        store
            .store("fact", "I also like Rust", Some(&emb3))
            .unwrap();

        // Query with embedding close to emb1
        let query = fake_embedding(0.9);
        let results = store.search_semantic(&query, 10).unwrap();

        assert_eq!(results.len(), 3);
        // First result should be the most similar (emb1 = exact match)
        assert_eq!(results[0].0.content, "I prefer Python");
        assert!(
            (results[0].1 - 1.0).abs() < 1e-5,
            "Expected ~1.0 similarity for identical vector"
        );
        // Scores should be descending
        assert!(results[0].1 >= results[1].1);
        assert!(results[1].1 >= results[2].1);
    }

    #[test]
    fn test_semantic_search_skips_memories_without_embeddings() {
        let store = setup_test_db();

        store
            .store("fact", "Has embedding", Some(&fake_embedding(0.5)))
            .unwrap();
        store.store("fact", "No embedding", None).unwrap();

        let query = fake_embedding(0.5);
        let results = store.search_semantic(&query, 10).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.content, "Has embedding");
    }

    #[test]
    fn test_semantic_search_limit() {
        let store = setup_test_db();

        for i in 0..5 {
            let emb = fake_embedding(i as f32 * 0.2);
            store
                .store("fact", &format!("Memory {}", i), Some(&emb))
                .unwrap();
        }

        let query = fake_embedding(0.5);
        let results = store.search_semantic(&query, 2).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_semantic_search_uses_hnsw_index_when_enabled() {
        let store = setup_test_db_with_hnsw(
            30.0,
            1.0,
            256,
            HnswIndexConfig {
                enabled: true,
                min_index_size: 2,
                m: 16,
                ef_construction: 200,
                ef_search: 64,
            },
        );

        let emb1 = fake_embedding(0.9);
        let emb2 = fake_embedding(0.1);
        let emb3 = fake_embedding(0.85);
        store.store("fact", "I prefer Python", Some(&emb1)).unwrap();
        store
            .store("fact", "The weather is nice", Some(&emb2))
            .unwrap();
        store
            .store("fact", "I also like Rust", Some(&emb3))
            .unwrap();

        assert_eq!(store.semantic_index_point_count(), 0);
        let query = fake_embedding(0.9);
        let results = store.search_semantic(&query, 3).unwrap();
        assert_eq!(store.semantic_index_point_count(), 3);
        assert!(!results.is_empty());
        assert_eq!(results[0].0.content, "I prefer Python");
    }

    #[test]
    fn test_semantic_search_falls_back_when_hnsw_disabled() {
        let store = setup_test_db_with_hnsw(
            30.0,
            1.0,
            256,
            HnswIndexConfig {
                enabled: false,
                min_index_size: 1,
                m: 16,
                ef_construction: 200,
                ef_search: 64,
            },
        );

        let emb1 = fake_embedding(0.9);
        let emb2 = fake_embedding(0.1);
        store.store("fact", "I prefer Python", Some(&emb1)).unwrap();
        store
            .store("fact", "The weather is nice", Some(&emb2))
            .unwrap();

        let query = fake_embedding(0.9);
        let results = store.search_semantic(&query, 2).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(store.semantic_index_point_count(), 0);
        assert_eq!(results[0].0.content, "I prefer Python");
    }

    #[test]
    fn test_semantic_hnsw_rebuilds_after_memory_mutation() {
        let store = setup_test_db_with_hnsw(
            30.0,
            1.0,
            256,
            HnswIndexConfig {
                enabled: true,
                min_index_size: 1,
                m: 16,
                ef_construction: 200,
                ef_search: 64,
            },
        );

        let emb1 = fake_embedding(0.2);
        let emb2 = fake_embedding(0.8);
        store.store("fact", "memory one", Some(&emb1)).unwrap();
        let query = fake_embedding(0.2);
        let first = store.search_semantic(&query, 10).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(store.semantic_index_point_count(), 1);

        store.store("fact", "memory two", Some(&emb2)).unwrap();
        let second = store.search_semantic(&query, 10).unwrap();
        assert_eq!(store.semantic_index_point_count(), 2);
        assert_eq!(second.len(), 2);
        assert!(second.iter().any(|(m, _)| m.content == "memory two"));
    }

    #[test]
    fn test_backfill_embedding() {
        let store = setup_test_db();

        // Store without embedding
        let id = store.store("fact", "No embedding yet", None).unwrap();

        // Verify it shows up as missing
        let missing = store.memories_without_embeddings().unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, id);

        // Backfill
        let emb = fake_embedding(0.7);
        store.backfill_embedding(&id, &emb).unwrap();

        // Should no longer be missing
        let missing = store.memories_without_embeddings().unwrap();
        assert!(missing.is_empty());

        // Should now appear in semantic search
        let query = fake_embedding(0.7);
        let results = store.search_semantic(&query, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.content, "No embedding yet");
    }

    #[test]
    fn test_blob_roundtrip() {
        let original = vec![1.0f32, -0.5, 0.0, 3.14159, f32::MIN, f32::MAX];
        let blob = embedding_to_blob(&original);
        let recovered = blob_to_embedding(&blob).unwrap();
        assert_eq!(original, recovered);
    }

    #[test]
    fn test_blob_invalid_length() {
        // 5 bytes is not divisible by 4
        let bad = vec![0u8; 5];
        assert!(blob_to_embedding(&bad).is_none());
    }

    #[test]
    fn test_hybrid_merges_keyword_and_semantic() {
        let store = setup_test_db();

        // Memory 1: has embedding, contains "config.toml"
        let emb1 = fake_embedding(0.3);
        store
            .store("fact", "Edit config.toml for settings", Some(&emb1))
            .unwrap();

        // Memory 2: has embedding, semantically close to query but no keyword match
        let emb2 = fake_embedding(0.9);
        store
            .store(
                "fact",
                "Application preferences are in the settings file",
                Some(&emb2),
            )
            .unwrap();

        // Memory 3: no embedding, but contains keyword
        store
            .store("fact", "config.toml uses TOML format", None)
            .unwrap();

        // Query: "config.toml" with embedding close to emb2
        let query_emb = fake_embedding(0.9);
        let results = store
            .search_hybrid("config.toml", Some(&query_emb), 10)
            .unwrap();

        // Should find all three: keyword matches (1 & 3) + semantic match (2)
        assert_eq!(results.len(), 3);

        // Collect content for easier assertion
        let contents: Vec<&str> = results.iter().map(|m| m.content.as_str()).collect();
        assert!(contents.contains(&"Edit config.toml for settings"));
        assert!(contents.contains(&"config.toml uses TOML format"));
        assert!(contents.contains(&"Application preferences are in the settings file"));
    }

    #[test]
    fn test_hybrid_deduplicates_by_id() {
        let store = setup_test_db();

        // Memory that matches both keyword and semantic
        let emb = fake_embedding(0.5);
        store
            .store("fact", "Rust is a systems language", Some(&emb))
            .unwrap();

        let query_emb = fake_embedding(0.5);
        let results = store.search_hybrid("Rust", Some(&query_emb), 10).unwrap();

        // Should appear only once despite matching both searches
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "Rust is a systems language");
    }

    #[test]
    fn test_hybrid_filters_low_similarity() {
        let store = setup_test_db();

        // Memory with embedding very different from query
        let emb = fake_embedding(0.01);
        store
            .store("fact", "Completely unrelated", Some(&emb))
            .unwrap();

        // Query embedding far from stored
        let query_emb = fake_embedding(0.99);
        let results = store
            .search_hybrid("nonexistent", Some(&query_emb), 10)
            .unwrap();

        // Keyword won't match, semantic similarity should be below threshold
        for m in &results {
            assert_ne!(
                m.content, "Completely unrelated",
                "low-similarity result should be filtered"
            );
        }
    }

    #[test]
    fn test_hybrid_no_embedding_falls_back_to_keyword() {
        let store = setup_test_db();
        store.store("fact", "Rust is great", None).unwrap();
        store.store("fact", "Python is nice", None).unwrap();

        // No query embedding → pure keyword
        let results = store.search_hybrid("Rust", None, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("Rust"));
    }

    #[test]
    fn test_retrieval_cache_hits_repeated_query() {
        let store = setup_test_db_with_config_and_cache(30.0, 1.0, 8);
        let emb = fake_embedding(0.42);
        store
            .store("fact", "Rust cache hit behavior", Some(&emb))
            .unwrap();
        store.reset_retrieval_cache_stats();

        let first = store.search_hybrid("Rust cache", Some(&emb), 10).unwrap();
        let second = store.search_hybrid("Rust cache", Some(&emb), 10).unwrap();
        assert_eq!(first, second);

        let stats = store.retrieval_cache_stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.entries, 1);
    }

    #[test]
    fn test_retrieval_cache_lru_eviction_order() {
        let store = setup_test_db_with_config_and_cache(30.0, 1.0, 2);
        store.store("fact", "alpha token", None).unwrap();
        store.store("fact", "beta token", None).unwrap();
        store.store("fact", "gamma token", None).unwrap();
        store.reset_retrieval_cache_stats();

        let _ = store.search_hybrid("alpha", None, 10).unwrap(); // miss
        let _ = store.search_hybrid("beta", None, 10).unwrap(); // miss
        let _ = store.search_hybrid("alpha", None, 10).unwrap(); // hit; beta now LRU
        let _ = store.search_hybrid("gamma", None, 10).unwrap(); // miss + evict beta
        let _ = store.search_hybrid("beta", None, 10).unwrap(); // miss (beta was evicted)

        let stats = store.retrieval_cache_stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 4);
        assert!(stats.evictions >= 1);
        assert_eq!(stats.entries, 2);
    }

    #[test]
    fn test_retrieval_cache_keys_do_not_collide() {
        let store = setup_test_db_with_config_and_cache(30.0, 1.0, 8);
        store.store("fact", "Rust one", None).unwrap();
        store.store("fact", "Rust two", None).unwrap();
        let emb = fake_embedding(0.6);
        store.reset_retrieval_cache_stats();

        let _ = store.search_hybrid("Rust", None, 1).unwrap(); // miss
        let _ = store.search_hybrid("Rust", None, 1).unwrap(); // hit
        let _ = store.search_hybrid("Rust", None, 2).unwrap(); // miss: different limit
        let _ = store.search_hybrid("Rust", Some(&emb), 2).unwrap(); // miss: embedding fingerprint

        let stats = store.retrieval_cache_stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 3);
    }

    #[test]
    fn test_retrieval_cache_stale_results_blocked_after_write() {
        let store = setup_test_db_with_config_and_cache(30.0, 1.0, 8);
        store.store("fact", "Rust original", None).unwrap();
        store.reset_retrieval_cache_stats();

        let before = store.search_hybrid("Rust", None, 10).unwrap();
        let _ = store.search_hybrid("Rust", None, 10).unwrap(); // hit

        store.store("fact", "Rust after write", None).unwrap(); // invalidates cache

        let after = store.search_hybrid("Rust", None, 10).unwrap();
        let stats = store.retrieval_cache_stats();

        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.invalidations, 1);
        assert!(after.len() >= before.len());
        assert!(after.iter().any(|m| m.content == "Rust after write"));
    }

    #[test]
    fn test_retrieval_cache_generation_mismatch_forces_miss() {
        let store = setup_test_db_with_config_and_cache(30.0, 1.0, 8);
        let emb = fake_embedding(0.5);
        store
            .store("fact", "generation mismatch guard", Some(&emb))
            .unwrap();
        store.reset_retrieval_cache_stats();

        let _ = store
            .search_hybrid("generation mismatch", Some(&emb), 10)
            .unwrap(); // miss + fill
        store.memory_generation.fetch_add(1, Ordering::Relaxed); // simulate out-of-band bump
        let _ = store
            .search_hybrid("generation mismatch", Some(&emb), 10)
            .unwrap(); // stale reject + miss

        let stats = store.retrieval_cache_stats();
        assert_eq!(stats.misses, 2);
        assert_eq!(stats.stale_rejections, 1);
    }

    #[test]
    fn test_retrieval_cache_invalidation_on_backfill_and_retire() {
        let store = setup_test_db_with_config_and_cache(30.0, 1.0, 8);
        let id = store
            .store("fact", "cache invalidation target", None)
            .unwrap();
        store.reset_retrieval_cache_stats();

        let _ = store
            .search_hybrid("invalidation target", None, 10)
            .unwrap();
        let emb = fake_embedding(0.33);
        store.backfill_embedding(&id, &emb).unwrap();
        let after_backfill = store.retrieval_cache_stats();
        assert_eq!(after_backfill.invalidations, 1);
        assert_eq!(after_backfill.entries, 0);

        let _ = store
            .search_hybrid("invalidation target", None, 10)
            .unwrap();
        store.retire(&id).unwrap();
        let after_retire = store.retrieval_cache_stats();
        assert_eq!(after_retire.invalidations, 2);
        assert_eq!(after_retire.entries, 0);
    }

    #[test]
    fn test_retrieval_cache_invalidation_on_dedup_update() {
        let store = setup_test_db_with_config_and_cache(30.0, 0.95, 8);
        let emb1 = fake_embedding(0.5);
        let emb2 = fake_embedding(0.51);
        store.store("fact", "first", Some(&emb1)).unwrap();
        store.reset_retrieval_cache_stats();

        let _ = store.search_hybrid("first", Some(&emb1), 10).unwrap();
        let _ = store.store("fact", "first updated", Some(&emb2)).unwrap(); // dedup path
        let stats = store.retrieval_cache_stats();

        assert_eq!(stats.invalidations, 1);
        assert_eq!(stats.entries, 0);
    }

    #[test]
    fn test_keyword_search_still_works() {
        let store = setup_test_db();
        store
            .store("fact", "Rust is a systems programming language", None)
            .unwrap();
        store.store("fact", "Python is interpreted", None).unwrap();

        let results = store.search("Rust").unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("Rust"));
    }

    #[test]
    fn test_dedup_merges_similar_memories() {
        let store = setup_test_db_with_config(30.0, 0.95);
        let emb1 = fake_embedding(0.5);
        let id1 = store.store("fact", "I like Python", Some(&emb1)).unwrap();

        // Very similar embedding should deduplicate
        let emb2 = fake_embedding(0.51);
        let id2 = store
            .store("fact", "I like Python a lot", Some(&emb2))
            .unwrap();
        assert_eq!(id1, id2);

        let memories = store.list().unwrap();
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].content, "I like Python a lot");
    }

    #[test]
    fn test_dedup_allows_different_memories() {
        let store = setup_test_db_with_config(30.0, 0.95);
        let emb1 = fake_embedding(0.1);
        let id1 = store.store("fact", "I like Python", Some(&emb1)).unwrap();

        // Very different embedding should not deduplicate
        let emb2 = fake_embedding(0.9);
        let id2 = store.store("fact", "I prefer Rust", Some(&emb2)).unwrap();
        assert_ne!(id1, id2);

        let memories = store.list().unwrap();
        assert_eq!(memories.len(), 2);
    }

    #[test]
    fn test_conversation_turns() {
        let store = setup_test_db();
        store.save_turn("cli:local:local", "user", "Hello").unwrap();
        store
            .save_turn("cli:local:local", "assistant", "Hi there!")
            .unwrap();
        store
            .save_turn("cli:local:local", "user", "How are you?")
            .unwrap();

        let turns = store.recent_turns("cli:local:local", 10).unwrap();
        assert_eq!(turns.len(), 3);
        // Chronological order
        assert_eq!(turns[0], ("user".to_string(), "Hello".to_string()));
        assert_eq!(turns[1], ("assistant".to_string(), "Hi there!".to_string()));
        assert_eq!(turns[2], ("user".to_string(), "How are you?".to_string()));
    }

    #[test]
    fn test_conversation_turns_limit() {
        let store = setup_test_db();
        store.save_turn("test:1:1", "user", "First").unwrap();
        store.save_turn("test:1:1", "assistant", "Second").unwrap();
        store.save_turn("test:1:1", "user", "Third").unwrap();
        store.save_turn("test:1:1", "assistant", "Fourth").unwrap();

        let turns = store.recent_turns("test:1:1", 2).unwrap();
        assert_eq!(turns.len(), 2);
        // Should be the most recent 2, in chronological order
        assert_eq!(turns[0].1, "Third");
        assert_eq!(turns[1].1, "Fourth");
    }

    #[test]
    fn test_conversation_session_isolation() {
        let store = setup_test_db();
        store
            .save_turn("cli:user1:chat1", "user", "Hello from user1")
            .unwrap();
        store
            .save_turn("cli:user2:chat2", "user", "Hello from user2")
            .unwrap();

        let turns1 = store.recent_turns("cli:user1:chat1", 10).unwrap();
        assert_eq!(turns1.len(), 1);
        assert_eq!(turns1[0].1, "Hello from user1");

        let turns2 = store.recent_turns("cli:user2:chat2", 10).unwrap();
        assert_eq!(turns2.len(), 1);
        assert_eq!(turns2[0].1, "Hello from user2");
    }

    #[test]
    fn test_time_decay_factor_recent() {
        // A memory created now should have decay factor ~1.0
        let now = Utc::now()
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let factor = time_decay_factor(&now, 30.0);
        assert!(
            (factor - 1.0).abs() < 0.01,
            "Recent memory should have factor ~1.0, got {}",
            factor
        );
    }

    #[test]
    fn test_time_decay_factor_old() {
        // A memory from 30 days ago should have factor ~0.5
        let old = (Utc::now().naive_utc() - chrono::Duration::days(30))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let factor = time_decay_factor(&old, 30.0);
        assert!(
            (factor - 0.5).abs() < 0.01,
            "30-day-old memory should have factor ~0.5, got {}",
            factor
        );
    }

    #[test]
    fn test_time_decay_factor_invalid() {
        let factor = time_decay_factor("not-a-date", 30.0);
        assert_eq!(factor, 1.0, "Invalid date should return 1.0 (no penalty)");
    }

    #[test]
    fn test_list_by_category_recent_respects_window_and_limit() {
        let store = setup_test_db();
        store.store("self_heal_outcome", "recent 1", None).unwrap();
        store.store("self_heal_outcome", "recent 2", None).unwrap();
        let old_id = store.store("self_heal_outcome", "old", None).unwrap();
        // Force one record to be outside recency window.
        {
            let conn = store.conn().unwrap();
            conn.execute(
                "UPDATE memories
                 SET created_at = datetime('now', '-40 days')
                 WHERE id = ?1",
                rusqlite::params![old_id],
            )
            .unwrap();
        }

        let rows = store
            .list_by_category_recent("self_heal_outcome", 1, 30)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_ne!(rows[0].content, "old");
    }

    #[test]
    fn test_list_by_categories_recent_hours_filters_categories_and_window() {
        let store = setup_test_db();
        let recent_refactor = store
            .store("refactoring_failed", "recent refactor fail", None)
            .unwrap();
        let old_code = store
            .store("code_change_failed", "old code fail", None)
            .unwrap();
        let recent_other = store.store("other", "recent other", None).unwrap();

        {
            let conn = store.conn().unwrap();
            conn.execute(
                "UPDATE memories
                 SET created_at = datetime('now', '-60 hours')
                 WHERE id = ?1",
                rusqlite::params![old_code],
            )
            .unwrap();
        }

        let rows = store
            .list_by_categories_recent_hours(&["code_change_failed", "refactoring_failed"], 10, 48)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, recent_refactor);
        assert_ne!(rows[0].id, recent_other);
    }

    #[derive(Debug, Clone, Serialize)]
    struct HotPathBenchRun {
        cache_capacity: usize,
        query_count: usize,
        hit_ratio: f64,
        hits: u64,
        misses: u64,
        evictions: u64,
        invalidations: u64,
        stale_incidents: usize,
        p50_latency_us: u64,
        p95_latency_us: u64,
    }

    #[derive(Debug, Clone, Serialize)]
    struct HotPathBenchDelta {
        hit_ratio_delta: f64,
        p50_improvement_pct: f64,
        p95_improvement_pct: f64,
    }

    #[derive(Debug, Clone, Serialize)]
    struct HotPathBenchReport {
        timestamp_utc: String,
        baseline: HotPathBenchRun,
        cached: HotPathBenchRun,
        delta: HotPathBenchDelta,
    }

    #[test]
    #[ignore = "benchmark"]
    fn bench_memory_hot_path_lru_cache() {
        let baseline = run_hot_path_workload(0);
        let cached = run_hot_path_workload(256);

        let report = HotPathBenchReport {
            timestamp_utc: Utc::now().format("%Y%m%dT%H%M%SZ").to_string(),
            delta: HotPathBenchDelta {
                hit_ratio_delta: cached.hit_ratio - baseline.hit_ratio,
                p50_improvement_pct: pct_improvement(
                    baseline.p50_latency_us,
                    cached.p50_latency_us,
                ),
                p95_improvement_pct: pct_improvement(
                    baseline.p95_latency_us,
                    cached.p95_latency_us,
                ),
            },
            baseline,
            cached,
        };

        write_hot_path_report(&report);

        println!("\n{}", render_hot_path_markdown(&report));

        assert_eq!(report.cached.stale_incidents, 0);
        assert!(report.cached.hit_ratio > 0.5);
        assert!(report.cached.p50_latency_us < report.baseline.p50_latency_us);
    }

    fn run_hot_path_workload(cache_capacity: usize) -> HotPathBenchRun {
        let store = setup_test_db_with_config_and_cache(30.0, 1.0, cache_capacity);
        let mut queries: Vec<(String, Vec<f32>)> = Vec::new();

        for i in 0..48 {
            let seed = 0.05 + (i as f32 / 48.0) * 0.9;
            let emb = fake_embedding(seed);
            let content = format!("topic{} cache benchmark memory payload {}", i, i);
            store.store("fact", &content, Some(&emb)).unwrap();
            if i < 24 {
                queries.push((format!("topic{}", i), emb));
            }
        }

        let sentinel_query = "stale sentinel";
        let mut sentinel_seed = 0.72f32;
        let mut sentinel_emb = fake_embedding(sentinel_seed);
        let mut sentinel_id = store
            .store("fact", "stale sentinel generation 0", Some(&sentinel_emb))
            .unwrap();
        let mut retired_ids: Vec<String> = Vec::new();

        for i in 0..240 {
            let (q, emb) = &queries[i % queries.len()];
            let _ = store.search_hybrid(q, Some(emb), 10).unwrap();
        }
        store.reset_retrieval_cache_stats();

        let iterations = 3000usize;
        let mut latencies_us: Vec<u64> = Vec::with_capacity(iterations);
        let mut stale_incidents = 0usize;

        for i in 0..iterations {
            let (query, emb): (&str, &[f32]) = if i % 5 == 0 {
                (sentinel_query, &sentinel_emb)
            } else {
                let (q, e) = &queries[i % queries.len()];
                (q.as_str(), e.as_slice())
            };

            let start = Instant::now();
            let results = store.search_hybrid(query, Some(emb), 10).unwrap();
            latencies_us.push(start.elapsed().as_micros() as u64);

            if retired_ids
                .iter()
                .any(|retired| results.iter().any(|m| &m.id == retired))
            {
                stale_incidents += 1;
            }

            if i > 0 && i % 300 == 0 {
                store.retire(&sentinel_id).unwrap();
                retired_ids.push(sentinel_id.clone());
                sentinel_seed += 0.04;
                if sentinel_seed > 0.95 {
                    sentinel_seed = 0.41;
                }
                sentinel_emb = fake_embedding(sentinel_seed);
                sentinel_id = store
                    .store(
                        "fact",
                        &format!("stale sentinel generation {}", i / 300),
                        Some(&sentinel_emb),
                    )
                    .unwrap();
            }
        }

        let stats = store.retrieval_cache_stats();
        HotPathBenchRun {
            cache_capacity,
            query_count: iterations,
            hit_ratio: stats.hit_ratio,
            hits: stats.hits,
            misses: stats.misses,
            evictions: stats.evictions,
            invalidations: stats.invalidations,
            stale_incidents,
            p50_latency_us: percentile(&latencies_us, 50.0),
            p95_latency_us: percentile(&latencies_us, 95.0),
        }
    }

    fn percentile(samples: &[u64], percentile: f64) -> u64 {
        if samples.is_empty() {
            return 0;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let idx = ((percentile / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[idx]
    }

    fn pct_improvement(before: u64, after: u64) -> f64 {
        if before == 0 {
            return 0.0;
        }
        ((before as f64 - after as f64) / before as f64) * 100.0
    }

    fn write_hot_path_report(report: &HotPathBenchReport) {
        let out_dir = Path::new("eval/results");
        fs::create_dir_all(out_dir).unwrap();

        let stem = format!("memory-hot-path-lru-bench-{}", report.timestamp_utc);
        let json = serde_json::to_string_pretty(report).unwrap();
        let md = render_hot_path_markdown(report);

        fs::write(out_dir.join(format!("{}.json", stem)), &json).unwrap();
        fs::write(out_dir.join(format!("{}.md", stem)), &md).unwrap();
        fs::write(out_dir.join("memory-hot-path-lru-bench-latest.json"), json).unwrap();
        fs::write(out_dir.join("memory-hot-path-lru-bench-latest.md"), md).unwrap();
    }

    fn render_hot_path_markdown(report: &HotPathBenchReport) -> String {
        format!(
            "# Memory Hot-Path LRU Benchmark ({})\n\n| Scenario | Capacity | Hit Ratio | p50 (us) | p95 (us) | Hits | Misses | Evictions | Invalidations | Stale Incidents |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n| Baseline | {} | {:.3} | {} | {} | {} | {} | {} | {} | {} |\n| Cached | {} | {:.3} | {} | {} | {} | {} | {} | {} | {} |\n\n## Delta\n- Hit ratio delta: {:.3}\n- p50 latency improvement: {:.2}%\n- p95 latency improvement: {:.2}%\n",
            report.timestamp_utc,
            report.baseline.cache_capacity,
            report.baseline.hit_ratio,
            report.baseline.p50_latency_us,
            report.baseline.p95_latency_us,
            report.baseline.hits,
            report.baseline.misses,
            report.baseline.evictions,
            report.baseline.invalidations,
            report.baseline.stale_incidents,
            report.cached.cache_capacity,
            report.cached.hit_ratio,
            report.cached.p50_latency_us,
            report.cached.p95_latency_us,
            report.cached.hits,
            report.cached.misses,
            report.cached.evictions,
            report.cached.invalidations,
            report.cached.stale_incidents,
            report.delta.hit_ratio_delta,
            report.delta.p50_improvement_pct,
            report.delta.p95_improvement_pct,
        )
    }

    // --- decay_score and cosine_similarity_raw unit tests ---

    #[test]
    fn decay_score_no_decay() {
        assert_eq!(decay_score("2026-01-01 00:00:00", 0.0), 1.0);
    }

    #[test]
    fn decay_score_at_half_life() {
        // At exactly half_life_days, score should be ~0.5
        let half_life = 30.0;
        let date_30_days_ago = (chrono::Utc::now() - chrono::Duration::days(30))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let score = decay_score(&date_30_days_ago, half_life);
        assert!(
            (score - 0.5).abs() < 0.01,
            "Expected ~0.5, got {}",
            score
        );
    }

    #[test]
    fn decay_score_recent_near_one() {
        let now = chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let score = decay_score(&now, 30.0);
        assert!(
            score > 0.99,
            "Recent entry should have score near 1.0, got {}",
            score
        );
    }

    #[test]
    fn cosine_similarity_raw_identical() {
        let v = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity_raw(&v, &v) - 1.0).abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_raw_orthogonal() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert!(cosine_similarity_raw(&a, &b).abs() < 0.001);
    }

    #[test]
    fn cosine_similarity_raw_empty() {
        assert_eq!(cosine_similarity_raw(&[], &[]), 0.0);
    }
}
