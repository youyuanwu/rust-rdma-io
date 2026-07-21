# Collection protocol & recording convention

The single authoritative source for **how benchmark results are structured,
recorded, compared, and collected** in this repo. Every scoreboard, coverage
matrix, and regime detail page follows it, so each `<env>/` tree links here rather
than restating it. Read this before adding results, adding an environment, or
reading the [scoreboard](azure-mana-rocev2/scoreboard.md).

Shared context lives one level up: [strategy](strategy.md) (what/why compared) ·
[methodology](methodology.md) (how runs execute) · [metrics](metrics.md) (metric
definitions) · [scenarios](scenarios/) (echo / grpc / h1).

---

## 1. The canonical comparison-table schema

Every cross-transport comparison uses one fixed schema, so results are uniform and
directly scannable. There are two forms of the *same* schema: the **full** form on
detail pages and a fixed **projection** on the scoreboard.

### 1.1 Detail-page table (full schema)

One table per dated result block, comparing the transports head-to-head:

| transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
|---|---:|---:|---:|---:|---:|---:|---|
| send-recv | … | … | … | … | … | … | CPU-eff× · p99× · tput% |
| read-ring | … | … | … | … | … | … | … |
| credit-ring | … | … | … | … | … | … | … |
| tcp *(or tcp1)* | … | … | … | … | … | — | baseline |

- **Rows:** the three RDMA transports (`send-recv`, `read-ring`, `credit-ring`),
  then the kernel baseline (`tcp` for echo/grpc, `tcp1` for h1) last.
- **Metric columns** (see [metrics.md](metrics.md) for definitions): peak
  throughput (req/s), CPU per operation (µs), cores busy at peak, p50 latency, p99
  latency, peak RSS.
- **`vs baseline`** carries the derived ratios for each RDMA row (§1.3).
- A metric that was **not recorded** is `n/r` — never blank, never guessed.

### 1.2 Scoreboard table (fixed projection)

The [scoreboard](azure-mana-rocev2/scoreboard.md) uses a fixed **projection** of
the full schema — the headline subset — applied identically everywhere:

| regime | transport | throughput | CPU/op | cores@peak | p99 | CPU-eff× | p99× | tput% |
|---|---|---:|---:|---:|---:|---:|---:|---:|

Same columns, same order, on every scoreboard table (no per-page drift). Detail
pages add `p50` and `peak RSS`; the scoreboard omits them for scannability.

### 1.3 Derived RDMA-vs-baseline ratios

Computed per RDMA row against the baseline row **in the same block/config**:

- **CPU-eff×** = baseline CPU/op ÷ RDMA CPU/op (how many times more CPU-efficient).
- **p99×** = RDMA p99 ÷ baseline p99 (<1 means RDMA has the lower/better tail).
- **tput%** = RDMA throughput ÷ baseline throughput × 100.

Rounding: `×` to one decimal, `%` to whole (or one decimal when <10). If the
baseline value is missing, the ratio is `n/r`. Ratios are **derived**, not source
data — they are excluded from the migration fidelity corpus (recomputed instead).

### 1.4 Reformatting rule (no fabricated rows)

The canonical table **reformats a block's existing headline comparison in place**.
Do **not** add a second duplicate table, and do **not** merge values measured at
*different* configurations into one row — v1 often measured peak throughput and
CPU/op at different configs. When columns come from different configs, annotate the
config (footnote) rather than inventing a combined row. Sweep tables and deeper
analysis stay as detail-page prose beneath the canonical table.

---

## 2. Regime kinds

Every regime is classified as one of two kinds; this fixes how it is presented and
how its coverage cells are marked.

- **Transport-comparison regime** — compares the RDMA transports against the kernel
  baseline. Populates all applicable transport rows. This is the RDMA-vs-TCP story
  the scoreboard tells.
- **Completion-topology regime** — a **read-ring completion-mode study**
  (busy-poll / arm-park / thread-per-core) against the baseline. The other RDMA
  transports have **no such mode**, so their rows are `— N/A (no such mode)`; the
  read-ring row is annotated with the mode. The mode-comparison tables
  (busy vs park vs thread-per-core) live in the detail page's analysis.

### Classification (azure-mana-rocev2)

| Scenario | Regime | Kind |
|---|---|---|
| echo | message-rate-64b | transport-comparison |
| echo | large-payload-8kib | transport-comparison |
| echo | busy-poll (`echo-busy`) | completion-topology |
| echo | thread-per-core-park (`echo-park`) | completion-topology |
| grpc | throughput-64b | transport-comparison |
| grpc | payload | transport-comparison |
| grpc | cpu-memory | transport-comparison |
| h1 | throughput-64b | transport-comparison |
| h1 | large-payload-8kib | transport-comparison |
| h1 | thread-per-core (`rh1-busy`/`rh1-park`) | completion-topology |

---

## 3. Scoreboard value-selection policy

Each scoreboard number is drawn deterministically so a reader can trace it:

- From the regime's **newest dated result block**.
- Each transport at its **characteristic peak** (the config the detail page
  presents as that transport's headline — e.g. read-ring at its depth optimum).
- The **config is annotated** on the regime via a **best-config footnote** where a
  single number could mislead (e.g. read-ring's depth knee), kept outside the fixed
  metric columns so the schema stays fixed.
- Cells not measured show `⏳ pending` or `— N/A` per the coverage rules (§5).

---

## 4. Result files are append-only, newest on top

Each regime detail page (`<env>/<scenario>/<regime>.md`) is a stack of **dated
result blocks, newest first**. A new run is **prepended** above the previous
blocks; older blocks are retained (git holds superseded edits, the doc holds the
run-to-run history). Layout:

```
# <Regime title>
<one-line intro linking the shared scenario doc>

## Results
### <YYYY-MM-DD> — <label>          (newest first)
- **Date:** 2026-07-17
- **Environment:** [<env-slug>](../README.md)
- **Commit:** `192daa7`             (or `unknown` — never guessed)
- **Command:** `just bench-echo --transport read-ring …`   (or `TODO — not recorded`)
- *(optional)* duration/warmup/threads · reboot-clean NIC · raw JSON `build/…/archive/`

<the canonical comparison table for this block, then its analysis, stay together>

### Undated — historical baseline    (last block, only if the baseline date is unrecoverable)
- **Date:** unknown
- **Environment:** … **Commit:** … **Command:** …
```

### Provenance rules

- **Mandatory provenance (per block):** `**Date:**`, `**Environment:**`,
  `**Commit:**`, `**Command:**`. Unrecoverable values are marked `unknown` /
  `TODO — not recorded`, **never guessed**. Optional fields (durations, reboot
  state, raw-JSON path) are included only when actually recorded.
- **Exact command:** the real recorded `**Command:**` carries the full command
  line (no illustrative `…`; the `…` above is only in this template).
- **Newest first;** an `### Undated — historical baseline` block sorts **last**.
- **Per-block attribution:** each run's canonical table and the prose analysing it
  stay **together** in that run's block. Only environment-invariant explanation
  belongs in the shared docs ([strategy](strategy.md) · [methodology](methodology.md)
  · [metrics](metrics.md) · [scenarios](scenarios/)).
- **Heading levels don't skip:** dated blocks are `###` under `## Results` (or under
  a preserved deep-linked `##` heading); any per-run analytical subsections nest as
  `####`/`#####` beneath their block.

---

## 5. Coverage matrix

Each environment publishes a **coverage matrix** (scenario × regime × transport) so
collection is systematic and gaps are visible. Every cell has an explicit state:

| Marker | Meaning |
|---|---|
| `✅` | **collected** — measured and recorded on the detail page |
| `⏳` | **pending** — collectible but not yet run (future data expected); give a short reason |
| `— N/A` | **not applicable** — the transport has no such mode for that regime (completion-topology regimes); give the reason "no such mode" |

No cell is ever blank. Completion-topology regimes' send-recv/credit-ring cells are
`— N/A (no such mode)`. A transport-comparison regime that simply hasn't been run
for a transport yet is `⏳ pending` (e.g. echo `large-payload-8kib` for
send-recv/credit-ring).

---

## 6. Workflows

### 6.1 Add a new run of an existing regime

1. **Prepend** a new `### <YYYY-MM-DD> — <label>` block to the matching
   `<env>/<scenario>/<regime>.md`, above the previous blocks.
2. Fill the mandatory provenance header (§4) — exact `**Command:**`, real
   `**Commit:**`, or honest `unknown`/`TODO — not recorded`.
3. Add this run's **canonical comparison table** (§1.1) and its analysis, together
   in the block.
4. If the run fills a previously-`⏳ pending` cell, flip that cell to `✅` in the
   environment's coverage matrix, and refresh the scoreboard row if this is now the
   newest block (§3).

### 6.2 Add a new environment

1. Choose a durable slug: **`<cloud>-<nic>[-<variant>]`**, lowercase, era-neutral —
   e.g. `azure-mana-rocev2`. Put SKU / driver / kernel specifics in the env README,
   not the slug. **Do not rename existing environment slugs** (inbound links depend
   on them).
2. Create `docs/bench/<slug>/README.md` — copy an existing environment README as the
   template: environment-definition table + a link to its scoreboard + a link back
   to this protocol.
3. Create `docs/bench/<slug>/scoreboard.md` (per-scenario canonical tables + coverage
   matrix + the cross-environment caveat) — copy the existing scoreboard as template.
4. Add a row to the Environments table in [README.md](README.md).
5. Reuse the shared docs unchanged; add regime files under
   `docs/bench/<slug>/<scenario>/` as data is collected.

### 6.3 Reproduce a result

Open the regime detail page, find the dated block, and re-run its exact
`**Command:**` on the environment named in `**Environment:**` at the recorded
`**Commit:**`. If any of those is `unknown` / `TODO — not recorded`, the result is
not exactly reproducible — record the real values on your fresh run.

---

## 7. Worked example (dry-run)

Say you re-ran the echo 64 B message-rate regime and want to record it. Using only
this protocol + the coverage matrix:

1. Open `azure-mana-rocev2/echo/message-rate-64b.md`. Prepend, above the newest
   block:

   ```
   ### 2026-08-01 — regression re-validation
   - **Date:** 2026-08-01
   - **Environment:** [azure-mana-rocev2](../README.md)
   - **Commit:** `abc1234`
   - **Command:** `just bench-echo --duration 10 --warmup 3 --threads 64 --in-flight 64`

   | transport | throughput | CPU/op | cores@peak | p50 | p99 | peak RSS | vs baseline |
   |---|---:|---:|---:|---:|---:|---:|---|
   | send-recv | 4.15M | 1.25 µs | 5.2 | 340 µs | 1500 µs | 24.4 MB | 4.3× · 1.1× · 61% |
   | read-ring | 4.80M | 1.20 µs | 5.9 | 150 µs | 1740 µs | 34.8 MB | 4.5× · 1.3× · 70% |
   | credit-ring | 0.98M | 3.18 µs | 3.1 | 4180 µs | 4730 µs | 34.3 MB | 1.7× · 3.5× · 14% |
   | tcp | 6.84M | 5.40 µs | 35.5 | 270 µs | 1340 µs | 17.9 MB | baseline |

   <one-paragraph analysis of this run stays here>
   ```

   (Ratios follow §1.3: e.g. read-ring CPU-eff× = 5.40 ÷ 1.20 = 4.5; p99× =
   1740 ÷ 1340 = 1.3; tput% = 4.80 ÷ 6.84 × 100 = 70.)

2. This regime's coverage cells were already `✅` — no change. Had this been a
   first run of a previously-`⏳ pending` cell, flip it to `✅`.

3. Because this is now the newest block, refresh the scoreboard's
   echo/message-rate-64b rows from it (§3), keeping the fixed projection columns.

Nothing else needs to change.
