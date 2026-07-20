# Recording convention

How benchmark results are recorded in this repo. This is the single canonical source
for the result-block format and the "adding data" workflow — every environment tree
follows it, so each `<env>/README.md` links here rather than restating it.

## Result files are append-only, newest on top

Each regime file (`<env>/<scenario>/<regime>.md`) is a stack of **dated result
blocks, newest first**. A new run is **prepended** above the previous blocks; older
blocks are retained (git holds superseded edits, the doc holds the run-to-run
history). Layout:

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

<this run's tables AND its analysis stay together, in this block>

### Undated — historical baseline    (last block, only if the baseline date is unrecoverable)
- **Date:** unknown
- **Environment:** … **Commit:** … **Command:** …
```

## Rules

- **Mandatory provenance (per block):** `**Date:**`, `**Environment:**`,
  `**Commit:**`, `**Command:**`. Unrecoverable values are marked `unknown` /
  `TODO — not recorded`, **never guessed**. Optional fields (durations, reboot state,
  raw-JSON path) are included only when actually recorded.
- **Exact command:** the real recorded `**Command:**` carries the full command line
  (no illustrative `…`; the `…` above is only in this template).
- **Newest first;** an `### Undated — historical baseline` block sorts **last**.
- **Per-block attribution:** each run's tables and the prose analysing them stay
  **together** in that run's block. Only environment-invariant explanation belongs in
  the shared docs ([strategy](strategy.md) · [methodology](methodology.md) ·
  [metrics](metrics.md) · [scenarios](scenarios/)).
- **Heading levels don't skip:** dated blocks are `###` under `## Results` (or under a
  preserved deep-linked `##` heading); any per-run analytical subsections nest as
  `####`/`#####` beneath their block.

## Adding a new run of an existing regime

Prepend a new dated block to the matching `<env>/<scenario>/<regime>.md` and update
that block with the run's tables and analysis. Nothing else needs to change.

## Adding a new environment

1. Choose a durable slug: **`<cloud>-<nic>[-<variant>]`**, lowercase, era-neutral —
   e.g. `azure-mana-rocev2`. Put SKU / driver / kernel specifics in the env README,
   not the slug. **Do not rename existing environment slugs** (inbound links depend on
   them).
2. Create `docs/bench/<slug>/README.md` — copy an existing environment README as the
   template: environment definition table + a results index grouped by scenario + a
   link back to this recording convention.
3. Add a row to the Environments table in [README.md](README.md).
4. Reuse the shared docs unchanged; add regime files under
   `docs/bench/<slug>/<scenario>/` as data is collected.
