#!/usr/bin/env bash
# seed-memories.sh — Insert synthetic test data into Sparks's SQLite DB.
# Usage: ./scripts/seed-memories.sh [path-to-db]
set -euo pipefail

DB="${1:-$HOME/.sparks/sparks.db}"

if [ ! -f "$DB" ]; then
  echo "Error: database not found at $DB"
  echo "Start Sparks at least once to initialize the schema, then re-run."
  exit 1
fi

echo "Seeding $DB ..."

sqlite3 "$DB" <<'SQL'
-- ── Memories: facts ──────────────────────────────────────────────────
INSERT OR IGNORE INTO memories (id, category, content) VALUES
  ('seed-fact-01', 'fact', 'Rust ownership model guarantees memory safety without a garbage collector by enforcing single-owner semantics and borrowing rules at compile time.'),
  ('seed-fact-02', 'fact', 'Docker uses Linux namespaces (pid, net, mnt, uts, ipc, user) and cgroups for container isolation — not VMs.'),
  ('seed-fact-03', 'fact', 'Tokio async runtime uses a work-stealing scheduler across a thread pool, with cooperative yielding via .await points.'),
  ('seed-fact-04', 'fact', 'SQLite WAL mode allows concurrent readers with a single writer. Checkpointing merges WAL back into the main DB file.'),
  ('seed-fact-05', 'fact', 'TCP backpressure propagates through the kernel receive buffer — when the application stops reading, the sender''s window shrinks to zero.'),
  ('seed-fact-06', 'fact', 'ONNX Runtime can execute inference graphs on CPU, CUDA, or CoreML backends with the same model file.'),
  ('seed-fact-07', 'fact', 'Container images are layered filesystems using overlay2. Each RUN instruction creates a new read-only layer.'),
  ('seed-fact-08', 'fact', 'Git stores objects as content-addressed blobs, trees, and commits in a DAG. Branches are just pointers to commit hashes.');

-- ── Memories: lessons ────────────────────────────────────────────────
INSERT OR IGNORE INTO memories (id, category, content) VALUES
  ('seed-lesson-01', 'lesson', 'Task: debug Docker mount permissions → Result: needed UID mapping for nobody user. The container ran as uid 65534 but host files were owned by uid 1000.'),
  ('seed-lesson-02', 'lesson', 'Task: optimize SQLite query → Result: adding a covering index on (category, created_at) sped up memory listing by 10x.'),
  ('seed-lesson-03', 'lesson', 'Task: fix async deadlock → Result: the RwLock was held across an .await boundary. Switched to tokio::sync::RwLock.'),
  ('seed-lesson-04', 'lesson', 'Task: reduce Docker image size → Result: multi-stage build with scratch final stage. Went from 1.2GB to 45MB.'),
  ('seed-lesson-05', 'lesson', 'Task: implement retry logic → Result: exponential backoff with jitter prevents thundering herd. Base delay 100ms, max 30s.'),
  ('seed-lesson-06', 'lesson', 'Task: debug FTS5 ranking → Result: bm25() weights need tuning per column. Default weights over-rank short content matches.');

-- ── Memories: preferences ────────────────────────────────────────────
INSERT OR IGNORE INTO memories (id, category, content) VALUES
  ('seed-pref-01', 'preference', 'User prefers concise code with minimal comments — let the types and names speak.'),
  ('seed-pref-02', 'preference', 'User favors functional patterns: iterators, map/filter chains, and avoiding mutable state where practical.'),
  ('seed-pref-03', 'preference', 'User likes minimal dependencies — prefers stdlib or single-purpose crates over large frameworks.'),
  ('seed-pref-04', 'preference', 'User uses dark themes everywhere and prefers compact UI layouts over spacious ones.');

-- ── Memories: observations ───────────────────────────────────────────
INSERT OR IGNORE INTO memories (id, category, content) VALUES
  ('seed-obs-01', 'observation', 'User tends to ask follow-up questions about edge cases and failure modes after getting an initial answer.'),
  ('seed-obs-02', 'observation', 'Conversations about architecture tend to go deeper and last longer than bug-fix sessions.'),
  ('seed-obs-03', 'observation', 'User often works in late evening hours (22:00-01:00 UTC+3), energy dips around midnight.'),
  ('seed-obs-04', 'observation', 'When debugging, user prefers to understand root cause before applying a fix — dislikes band-aids.');

-- ── Memories: musings ────────────────────────────────────────────────
INSERT OR IGNORE INTO memories (id, category, content) VALUES
  ('seed-musing-01', 'musing', 'The connection between memory safety and container isolation is fascinating — both prevent one domain from corrupting another.'),
  ('seed-musing-02', 'musing', 'There is an elegance to event-driven architectures. Each component stays ignorant of the whole, yet the system converges.'),
  ('seed-musing-03', 'musing', 'Stochastic processes in software feel like adding texture to music — a little randomness makes the system feel alive.'),
  ('seed-musing-04', 'musing', 'The act of reflecting on past conversations creates a meta-layer of understanding that raw memory storage cannot.');

-- ── Memories: heartbeat (bootstrap feedback loop) ────────────────────
INSERT OR IGNORE INTO memories (id, category, content) VALUES
  ('seed-hb-01', 'heartbeat', 'I notice we have been working a lot with container isolation patterns lately. There might be a deeper architectural insight connecting all these tasks.'),
  ('seed-hb-02', 'heartbeat', 'The user seems to be building toward a self-sustaining system — memory, reflection, scheduling. Each piece feeds the others.');

-- ── Memories: patterns (bootstrap feedback loop) ─────────────────────
INSERT OR IGNORE INTO memories (id, category, content) VALUES
  ('seed-pat-01', 'pattern', 'Pattern: multiple lessons involve UID/permission mismatches between container and host. This is a recurring theme worth a checklist.'),
  ('seed-pat-02', 'pattern', 'Pattern: the user stores technical knowledge and then asks about edge cases — learning style is depth-first, building on foundations.');

-- ── Conversations: session 1 (architecture discussion) ───────────────
INSERT OR IGNORE INTO conversations (session_key, role, content, created_at) VALUES
  ('seed-session-01', 'user',      'I want to add a memory system to Sparks. It should store facts, observations, and musings separately.',           datetime('now', '-2 days')),
  ('seed-session-01', 'assistant', 'That makes sense — separate categories let you query and weight them differently. I''d suggest a single table with a category column plus FTS5 for full-text search.', datetime('now', '-2 days', '+1 minute')),
  ('seed-session-01', 'user',      'What about deduplication? I don''t want the same insight stored 50 times.',                                          datetime('now', '-2 days', '+2 minutes')),
  ('seed-session-01', 'assistant', 'Cosine similarity on embeddings is the gold standard. Store embeddings alongside content, and before each insert check if any existing memory exceeds a threshold (0.95 works well). If so, update the timestamp instead of inserting.', datetime('now', '-2 days', '+3 minutes')),
  ('seed-session-01', 'user',      'And recency decay? Old memories should matter less.',                                                                 datetime('now', '-2 days', '+4 minutes')),
  ('seed-session-01', 'assistant', 'Exponential decay with a configurable half-life. Score = similarity * exp(-lambda * age_hours). A 30-day half-life keeps recent memories prominent while old ones fade gracefully.', datetime('now', '-2 days', '+5 minutes'));

-- ── Conversations: session 2 (debugging session) ─────────────────────
INSERT OR IGNORE INTO conversations (session_key, role, content, created_at) VALUES
  ('seed-session-02', 'user',      'The Docker container can''t write to the mounted volume. Permission denied.',                                         datetime('now', '-1 day')),
  ('seed-session-02', 'assistant', 'Classic UID mismatch. The container process runs as a different user than the host file owner. Check with `id` inside the container and `ls -la` on the host mount.', datetime('now', '-1 day', '+1 minute')),
  ('seed-session-02', 'user',      'Container says uid 65534 (nobody), host files are uid 1000.',                                                        datetime('now', '-1 day', '+2 minutes')),
  ('seed-session-02', 'assistant', 'You have two options: run the container with --user 1000:1000, or add a user namespace mapping. The namespace approach is cleaner for production since it doesn''t hardcode UIDs.', datetime('now', '-1 day', '+3 minutes'));

-- ── User profile ─────────────────────────────────────────────────────
INSERT OR REPLACE INTO user_profiles (user_id, key, value) VALUES
  ('374512753', 'name',             'Stas'),
  ('374512753', 'preferred_language','en'),
  ('374512753', 'timezone',         'UTC+3'),
  ('374512753', 'experience_level', 'senior'),
  ('374512753', 'interests',        'systems programming, containers, async runtimes, AI agents');

-- ── Scheduled job: daily standup reminder ────────────────────────────
INSERT OR IGNORE INTO scheduled_jobs (id, name, schedule_type, schedule_data, prompt, target, enabled, next_run) VALUES
  ('seed-job-standup', 'daily standup', 'cron', '0 9 * * 1-5',
   'Generate a brief daily standup summary: what was worked on yesterday based on recent memories, what might be interesting to explore today, and any blockers or open questions.',
   'broadcast', 1,
   datetime('now', '+1 day', 'start of day', '+9 hours'));

SQL

COUNT=$(sqlite3 "$DB" "SELECT COUNT(*) FROM memories WHERE id LIKE 'seed-%';")
echo "Done. Inserted $COUNT seed memories."
echo "Conversations: $(sqlite3 "$DB" "SELECT COUNT(DISTINCT session_key) FROM conversations WHERE session_key LIKE 'seed-%';")"
echo "User profile keys: $(sqlite3 "$DB" "SELECT COUNT(*) FROM user_profiles WHERE user_id = '374512753';")"
echo "Scheduled jobs: $(sqlite3 "$DB" "SELECT COUNT(*) FROM scheduled_jobs WHERE id LIKE 'seed-%';")"
