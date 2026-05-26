---
name: docmancer
description: Query the Ocellus local uncore documentation RAG with Docmancer. Use when working on Intel uncore PMU events, Ocellus metrics, Grafana dashboards, CPU generation support, Linux uncore perf/sysfs behavior, Intel perfmon JSON, or any question that needs source-grounded lookup from the local `.resources/docmancer` corpus.
---

# Docmancer

Use the project-local Docmancer index before relying on memory for Intel uncore details.

## Setup

If `.resources/docmancer/venv/bin/docmancer` or `.resources/docmancer/docmancer.yaml` is missing, rebuild the local corpus and index:

```bash
scripts/setup-uncore-rag.sh
```

To also install/update the user-level agent skill copy:

```bash
scripts/setup-uncore-rag.sh --install-codex-skill
```

The generated resources are intentionally untracked under `.resources/`.

## Query

Run queries from the repo root with the project-local config:

```bash
.resources/docmancer/venv/bin/docmancer \
  --config .resources/docmancer/docmancer.yaml \
  query 'CAPID6 CAPID7 CHA count Skylake Xeon' \
  --limit 8 \
  --budget 3000 \
  --explain
```

Useful checks:

```bash
.resources/docmancer/venv/bin/docmancer --config .resources/docmancer/docmancer.yaml inspect
.resources/docmancer/venv/bin/docmancer --config .resources/docmancer/docmancer.yaml list
```

## Source Trust

Prefer sources in this order:

1. Intel uncore programming/reference manuals and SDM material.
2. Linux kernel uncore PMU driver code and sysfs ABI documentation.
3. Intel perfmon public JSON event files.
4. Intel perfmon experimental JSON event files.
5. Local machine validation with `perf`, MSR reads, or Ocellus exporter output.

Treat `*_uncore_experimental.json` entries as hints. Do not enable exporter metrics or dashboards from experimental JSON alone; verify against a manual, Linux support, or hardware.

## Corpus Shape

The script builds a focused corpus at `.resources/docmancer/corpus` from:

- Intel perfmon uncore JSON and `mapfile.csv`.
- Intel uncore manuals/packages for Ocellus-supported generations.
- Linux `arch/x86/events/intel/uncore*`, perf PMU event tables, and event_source sysfs ABI docs.
- Normalized per-event Markdown records under `normalized-events/` that repeat platform, source, and trust metadata beside each event.

Text-shadow files under `.resources/docmancer/corpus/text-shadows/` are copies with `.txt` suffixes so Docmancer can index CSV, C, header, and extensionless ABI content. Event JSON tables are not indexed raw; use the generated `normalized-events/` Markdown records for event lookup.
