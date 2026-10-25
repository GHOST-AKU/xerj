# Autoindex / search release performance gate

`scripts/perf/autoindex_release_gate.sh` blocks a build that repeats the
rc.99 field failures: #2421 (state dir 1.6x source), #2422 (454 indices,
13–23 s searches), #2423 (0.2 MB/s, OOM), #2424 (planning memory, long
names), #2427 (node over its own memory cap) and #2428 (client EAGAIN abort).
It runs the user-facing path end to end, `xerj autoindex <folder>` with
default flags, against a deterministic synthetic corpus on a throwaway node
with a fixed memory cap. It then measures uncached search on all three
surfaces and fails on any budget miss.

## Run it locally

```bash
(cd engine && cargo build --release --locked -p xerj-server)
scripts/perf/autoindex_release_gate.sh --bin engine/target/release/xerj            # 1 GB tier
scripts/perf/autoindex_release_gate.sh --bin engine/target/release/xerj --tier smoke
scripts/perf/autoindex_release_gate.sh --bin engine/target/release/xerj \
    --baseline ~/xerj-prev/xerj                                             # +regression checks
scripts/perf/autoindex_release_gate.sh --bin engine/target/release/xerj \
    --engine-baseline es-baseline.json                                      # +ES/Lucene floor
```

| flag | meaning |
|---|---|
| `--tier smoke\|1gb\|5gb` | budget tier in `scripts/perf/budgets.toml` (sets corpus size, memory cap, timeout) |
| `--size-mb N` | override corpus size (budgets stay those of the tier) |
| `--seed N` | corpus seed (default 2421) |
| `--work DIR` | scratch: corpus + per-run data/state (default `$RUNNER_TEMP` or `/tmp`/xerj-perf-gate) |
| `--corpus-dir DIR` | reuse or put the corpus here (reused if its manifest matches size + seed) |
| `--out DIR` | results (`candidate.json`, `baseline.json`, `results.json`, `summary.md`, log tails) |
| `--baseline BIN` | also run the previous release binary on the same corpus; fail on >20% regression |
| `--engine-baseline FILE` | ES/Lucene floor JSON (from `es_baseline.py`); enables `[ratio.*]` budgets |
| `--autoindex-args "..."` | extra autoindex flags (default: none, the user-facing path) |
| `--keep` | keep run data/state dirs |
| `MIN_FREE_GB` env | disk reserve the guard keeps free (default 8) |

Exit 0 means every budget was met. Exit 1 means at least one was missed, and the markdown table is
printed and written to `summary.md` (and appended to `$GITHUB_STEP_SUMMARY`
in Actions). Exit 2 means a usage or environment error, e.g. not enough disk. Requires
Python ≥ 3.11 (stdlib only). On a 4-core machine a passing build finishes the
1 GB tier in under 20 minutes: corpus about 25 s, autoindex ≤ 15 min by budget, restart
plus 60 queries about 1 min.

## What a run does

1. `gen_corpus.py` writes a byte-for-byte deterministic corpus plus
   `<corpus>.queries.json` (20 queries per surface, 60 distinct strings
   drawn from phrases planted in the corpus, so each should hit) and
   `<corpus>.manifest.json`. The mix per GB is code (rs/go/py/ts, about 24%),
   markdown (12%), HTML with tables (8%), 3 large plus about 800 small
   distinct-schema CSVs, 24 JSONL, 16 logs, 4 big JSON flight-trace files with numeric
   arrays (15%), 1500 multi-array JSON/YAML, 8000 tiny files and deep Go-module
   style paths up to about 500 chars.
2. `gate.py run` starts `xerj --insecure --data-dir … --port … --disable-feedback`
   with `XERJ_MAX_PROCESS_MEMORY_MB=<tier cap>` and `XERJ_DISABLE_FEEDBACK=true`,
   then runs `xerj autoindex <corpus> --approve proceed --json` under the tier
   timeout. It samples node and client process-tree RSS and threads from `/proc`
   every 0.5 s.
3. It reads `_cat/indices/<prefix>-*`, stops the node, scans the node log for
   parent-breaker trips (`engaging the parent memory circuit breaker`), fatal
   patterns (EAGAIN / os error 11, OOM, SIGABRT/panic, EMFILE) and retry/429
   lines, and measures the state dir and data dir sizes.
4. It restarts the node (timed) so the page cache and query caches are cold,
   then runs 20 queries through `xerj search --json`, 20 through raw
   `POST /<prefix>-*/_search?request_cache=false` and 20 through `xerj mcp`
   (`tools/call xerj_search`). No query string repeats.
5. `gate.py eval` compares against `budgets.toml`.

## Budgets

See `scripts/perf/budgets.toml` for the numbers and the reasoning behind each one.

| check | smoke (200 MB) | 1gb | 5gb (release) | why |
|---|---|---|---|---|
| autoindex completes (exit 0/3, node alive, no OOM/abort/EAGAIN, within timeout) | 6 min | 15 min | 30 min | #2423, #2427, #2428 |
| throughput MB/s ≥ | 1.5 | 3.0 | 4.0 | #2423 asks 5 GB in 20 min ≈ 4.3 MB/s; rc.99 did about 0.2 |
| node peak RSS ≤ memory cap (MB) | 2048 | 3072 | 4096 | #2427: 3.9 GB under a 3 GB cap |
| client peak RSS (MB) ≤ | 768 | 1024 | 1536 | #2424, #2428 |
| state dir / source ≤ | 1.2 | 1.2 | 1.2 | #2421: 8.4 GB state for 5 GB |
| indices per prefix ≤ | 40 | 40 | 40 | #2422: 454; #2430 caps a run at 32 |
| uncached search p95 / max (ms) | 1000 / 2000 | 1000 / 2000 | 1000 / 2000 | #2422: 13–23 s |
| queries with hits ≥ | 90% | 90% | 90% | catches "fast because it found nothing" |
| parent-breaker trips | 0 | 0 | 0 | #2427 |
| bulk retries / 429s ≤ | 20 | 20 | 50 | #2427 |
| node restart (s) ≤ | 60 | 60 | 120 | |
| regression vs `--baseline` binary | 20% | 20% | 20% | throughput, p95, node/client RSS, state ratio, index bytes |
| index bytes vs ES ≤ | 2.5x | 2.0x | 2.0x | Lucene floor (`[ratio.*]`) |
| ingest MB/s vs ES ≥ | 0.3x | 0.5x | 0.5x | Lucene floor |
| search p95 vs ES ≤ | — | 1.5x | 1.5x | Lucene floor |

### Lucene / Elasticsearch floor

Absolute budgets drift with runner hardware, so index size and indexing speed are
also held against Elasticsearch on the same corpus and the same machine.
`scripts/perf/es_baseline.py` bulk-loads the corpus into a throwaway ES. It writes
one doc per line for jsonl/csv/log and one doc per file otherwise, using default
analyzers, 1 shard and 0 replicas. It then refreshes and flushes, runs the same 60
queries uncached, and writes:

```json
{"engine": "elasticsearch-9.x (lucene 10.x)", "corpus_mb": 1024, "seed": 2421,
 "index_bytes": 0, "ingest_mb_s": 0.0, "search_p95_ms": 0.0}
```

Pass that file as `--engine-baseline` and the `[ratio.<tier>]` keys apply:
`xerj_index_bytes ≤ k·es_index_bytes`, `xerj_ingest_MBps ≥ k·es_ingest_MBps`,
and `xerj_p95 ≤ k·es_p95`. In CI the ES service container runs on the same runner,
so the floor scales with the runner automatically. The `k` values are provisional
until the ES/Lucene measurements on the reference corpus land (perf-floor work).
Tighten them there.

```bash
python3 scripts/perf/gen_corpus.py --out /tmp/c1g --size-mb 1024 --seed 2421
docker run -d --name es -p 9200:9200 -e discovery.type=single-node \
  -e xpack.security.enabled=false -e ES_JAVA_OPTS="-Xms2g -Xmx2g" \
  docker.elastic.co/elasticsearch/elasticsearch:9.5.5
python3 scripts/perf/es_baseline.py --es http://127.0.0.1:9200 --corpus /tmp/c1g \
  --queries /tmp/c1g.queries.json --out es-baseline.json
```

## CI

The workflow (`.github/workflows/perf-gate.yml`) is maintained by hand. See the
tracking issue for the ready-to-paste YAML. It runs:

* on PRs touching autoindex, engine or server paths: the 1 GB tier;
* nightly: the 1 GB tier with `--baseline` set to the latest release binary, plus the ES floor;
* on `v*` tags, as a job `release.yml` depends on (`needs: [build, perf-gate]`): the 5 GB tier.

Results are uploaded as the `perf-gate-*` artifact (`results.json` holds every
raw measurement, budget and verdict) and summarized in the job summary and as
a PR comment.

## Changing a budget

Edit `scripts/perf/budgets.toml` in the same PR as the change that needs it,
with a comment explaining why. Loosening a budget needs a linked issue.
