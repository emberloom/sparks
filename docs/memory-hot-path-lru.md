# Memory Hot-Path LRU Architecture (LNY-43)

## Goal

Close the repeated-query latency gap in memory retrieval without sacrificing correctness.

## Retrieval Pipeline (Quality Path)

`MemoryStore::search_hybrid` computes final memory ranking in this order:

1. FTS leg (`search_fts`): lexical precision with BM25 ranking.
2. Semantic leg (`search_semantic`): vector similarity over active embeddings.
3. Merge + dedup by memory ID.
4. Recency decay (`0.5^(age_days / half_life_days)`).
5. Top-K truncation.

Quality improves over time because:
- recency decay continuously prioritizes fresher evidence;
- dedup collapses near-duplicates on write;
- semantic retrieval (ANN slot; currently in-process cosine scan) captures meaning beyond keywords;
- FTS retains strong exact/phrase recall;
- hybrid merge combines both recall modes before recency weighting.

## Cache Boundary

The LRU cache sits at the `search_hybrid` boundary and stores **final ranked `Vec<Memory>` only**.

Cache key:
- normalized query text (trimmed/lowercased/collapsed whitespace),
- `limit`,
- query embedding fingerprint (or none).

Cache entry metadata:
- `generation` at fill time,
- insertion timestamp,
- final result payload.

Not cached:
- raw FTS rows,
- raw semantic scores,
- write paths,
- conversation history APIs.

## Deterministic Stale Guardrail

`MemoryStore` now tracks monotonic `memory_generation` and a bounded LRU.

Invalidation contract:
- bump generation + invalidate cache on:
  - `store` insert path,
  - `store` dedup-update path,
  - `backfill_embedding`,
  - `retire`.
- read path rejects entries whose stored generation != current generation.

This makes stale reuse deterministic: stale entries are never served after a mutating memory/index operation.

## Bounded Memory

Capacity is count-bounded (`memory.retrieval_cache_capacity`, default `256`, `0` disables cache).

Memory overhead is upper-bounded by:
- cache metadata (`O(capacity)` keys + LRU order),
- cloned result payloads for at most `capacity` query keys.

## Benchmark Harness

Run:

```bash
cargo test bench_memory_hot_path_lru_cache -- --ignored --nocapture
```

Artifacts written to:
- `eval/results/memory-hot-path-lru-bench-<timestamp>.json`
- `eval/results/memory-hot-path-lru-bench-<timestamp>.md`
- `eval/results/memory-hot-path-lru-bench-latest.json`
- `eval/results/memory-hot-path-lru-bench-latest.md`

## Latest Benchmark Evidence

Run timestamp: `20260303T101326Z` (March 3, 2026).

- Baseline (`capacity=0`): hit ratio `0.000`, p50 `294us`, p95 `382us`, stale incidents `0`.
- Cached (`capacity=256`): hit ratio `0.925`, p50 `2us`, p95 `284us`, stale incidents `0`.
- Delta: hit ratio `+0.925`, p50 `+99.32%`, p95 `+25.65%`.

Evidence files:
- `eval/results/memory-hot-path-lru-bench-20260303T101326Z.json`
- `eval/results/memory-hot-path-lru-bench-20260303T101326Z.md`
- `eval/results/memory-hot-path-lru-bench-latest.json`
- `eval/results/memory-hot-path-lru-bench-latest.md`

## Measurable Claims (AC Mapping)

- AC1: cached run should materially improve repeated-query latency with bounded capacity.
  - target: p50 improvement >= 35%, p95 improvement >= 20% versus capacity `0`.
- AC2: stale protection is deterministic.
  - target: stale incidents == `0` under interleaved write/read workload.
- AC3: report includes hit ratio + p50/p95 + stale incidents for baseline and cached runs.
- AC4: claims are tied to generated benchmark artifacts in `eval/results/`.
