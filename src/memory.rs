use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use chrono::{NaiveDateTime, Utc};
use rusqlite::Connection;

use crate::embeddings::cosine_similarity;
use crate::error::{AthenaError, Result};

/// Minimum cosine similarity to keep a semantic result.
pub const SEMANTIC_THRESHOLD: f32 = 0.25;

pub struct Memory {
    pub id: String,
    pub category: String,
    pub content: String,
    // Filtering by active is done in SQL (WHERE active = 1); not read in Rust
    #[allow(dead_code)]
    pub active: bool,
    pub created_at: String,
}

pub struct MemoryStore {
    conn: Mutex<Connection>,
    embedding_cache: RwLock<HashMap<String, Vec<f32>>>,
    recency_half_life_days: f32,
    dedup_threshold: f32,
}

impl MemoryStore {
    pub fn new(conn: Connection, recency_half_life_days: f32, dedup_threshold: f32) -> Self {
        let store = Self {
            conn: Mutex::new(conn),
            embedding_cache: RwLock::new(HashMap::new()),
            recency_half_life_days,
            dedup_threshold,
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
            .map_err(|e| AthenaError::Internal(format!("Database lock poisoned: {}", e)))
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
            .map_err(|e| AthenaError::Internal(format!("Embedding cache lock poisoned: {}", e)))?;
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

    /// Semantic search: cosine similarity via in-memory embedding cache.
    pub fn search_semantic(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(Memory, f32)>> {
        // Phase 1: compute similarities in memory (only hold cache read lock)
        let id_scores = {
            let cache = self.embedding_cache.read().map_err(|e| {
                AthenaError::Internal(format!("Embedding cache lock poisoned: {}", e))
            })?;
            let mut id_scores: Vec<(String, f32)> = cache
                .iter()
                .map(|(id, emb)| (id.clone(), cosine_similarity(query_embedding, emb)))
                .collect();
            id_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            id_scores.truncate(limit);
            id_scores
        }; // cache read lock dropped

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

    /// Hybrid search: FTS5 keyword + semantic, merged with time decay.
    pub fn search_hybrid(
        &self,
        query: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
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

        Ok(results.into_iter().map(|(m, _)| m).collect())
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
        .map_err(|e| AthenaError::Db(e))
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
            Err(e) => Err(AthenaError::Db(e)),
        }
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

    fn setup_test_db() -> MemoryStore {
        setup_test_db_with_config(30.0, 1.0) // dedup disabled in most tests
    }

    fn setup_test_db_with_config(half_life: f32, dedup_threshold: f32) -> MemoryStore {
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
        MemoryStore::new(conn, half_life, dedup_threshold)
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
}
