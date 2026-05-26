#!/usr/bin/env python3
"""Generate normalized Markdown event records for the Ocellus uncore RAG corpus."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


PLATFORMS = {
    "SNB": {
        "name": "Sandy Bridge Xeon",
        "aliases": ["SNB", "Sandy Bridge", "Sandy Bridge-EP", "Intel Xeon E5-2600"],
    },
    "IVT": {
        "name": "Ivy Town Xeon",
        "aliases": ["IVT", "Ivy Town", "Ivy Bridge-EP", "Intel Xeon E5-2600 v2"],
    },
    "HSX": {
        "name": "Haswell Xeon",
        "aliases": ["HSX", "Haswell", "Haswell-EP", "Intel Xeon E5 v3", "Intel Xeon E7 v3"],
    },
    "BDX": {
        "name": "Broadwell Xeon",
        "aliases": ["BDX", "Broadwell", "Broadwell-EP", "Intel Xeon E5 v4", "Intel Xeon E7 v4"],
    },
    "BDW-DE": {
        "name": "Broadwell-DE Xeon",
        "aliases": ["BDW-DE", "Broadwell-DE", "Intel Xeon D"],
    },
    "SKX": {
        "name": "Skylake Xeon",
        "aliases": ["SKX", "Skylake Xeon", "Skylake-SP", "Intel Xeon Scalable"],
    },
    "CLX": {
        "name": "Cascade Lake Xeon",
        "aliases": ["CLX", "Cascade Lake", "Cascade Lake-SP", "2nd Gen Intel Xeon Scalable"],
    },
    "ICX": {
        "name": "Ice Lake Xeon",
        "aliases": ["ICX", "Ice Lake Xeon", "Ice Lake-SP", "3rd Gen Intel Xeon Scalable"],
    },
    "SPR": {
        "name": "Sapphire Rapids Xeon",
        "aliases": ["SPR", "Sapphire Rapids", "sapphirerapids", "4th Gen Intel Xeon Scalable"],
    },
    "EMR": {
        "name": "Emerald Rapids Xeon",
        "aliases": ["EMR", "Emerald Rapids", "emeraldrapids", "5th Gen Intel Xeon Scalable"],
    },
}

LINUX_ARCH_TO_PLATFORM = {
    "sandybridge": "SNB",
    "ivytown": "IVT",
    "haswellx": "HSX",
    "broadwellde": "BDW-DE",
    "broadwellx": "BDX",
    "skylakex": "SKX",
    "cascadelakex": "CLX",
    "icelakex": "ICX",
    "sapphirerapids": "SPR",
    "emeraldrapids": "EMR",
}


def slug(value: str) -> str:
    return re.sub(r"[^a-zA-Z0-9._-]+", "-", value).strip("-").lower()


def load_events(path: Path) -> list[dict[str, Any]]:
    data = json.loads(path.read_text(encoding="utf-8"))
    if isinstance(data, dict):
        events = data.get("Events", [])
    elif isinstance(data, list):
        events = data
    else:
        events = []
    return [event for event in events if isinstance(event, dict)]


def scalar(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, (str, int, float)):
        return str(value)
    return json.dumps(value, sort_keys=True)


def truthy_metadata(value: Any) -> bool:
    if value is None:
        return False
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return value != 0
    if isinstance(value, str):
        return value.strip().lower() in {"1", "true", "yes", "y"}
    return False


def event_line(event: dict[str, Any], keys: list[str]) -> str:
    parts = []
    for key in keys:
        value = scalar(event.get(key))
        if value:
            parts.append(f"{key}={value}")
    return "; ".join(parts) if parts else "none"


def describe_platform(code: str) -> tuple[str, list[str]]:
    meta = PLATFORMS.get(code, {"name": code, "aliases": [code]})
    return meta["name"], list(dict.fromkeys([code, *meta["aliases"]]))


def write_event_file(
    out_path: Path,
    *,
    title: str,
    platform_code: str,
    source_kind: str,
    source_file: str,
    trust_rank: str,
    trust_note: str,
    events: list[dict[str, Any]],
) -> None:
    platform_name, aliases = describe_platform(platform_code)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("w", encoding="utf-8") as out:
        out.write(f"# {title}\n\n")
        out.write(f"Platform code: {platform_code}\n\n")
        out.write(f"Platform name: {platform_name}\n\n")
        out.write(f"Platform aliases: {'; '.join(aliases)}\n\n")
        out.write(f"Source kind: {source_kind}\n\n")
        out.write(f"Source file: {source_file}\n\n")
        out.write(f"Trust rank: {trust_rank}\n\n")
        out.write(f"Trust note: {trust_note}\n\n")
        out.write("Each event section repeats platform and source metadata for retrieval.\n\n")

        for event in events:
            name = scalar(event.get("EventName")) or scalar(event.get("MetricName")) or "unnamed-event"
            unit = scalar(event.get("Unit")) or "unknown-unit"
            brief = scalar(event.get("BriefDescription"))
            public = scalar(event.get("PublicDescription"))
            experimental = scalar(event.get("Experimental"))
            deprecated = scalar(event.get("Deprecated"))
            is_experimental = truthy_metadata(event.get("Experimental")) or "experimental" in source_kind.lower()
            effective_trust_rank = "4" if is_experimental else trust_rank
            effective_trust_note = (
                "Experimental event entry. Treat as a hint and verify against a manual, Linux driver support, or hardware before relying on it."
                if is_experimental
                else trust_note
            )
            support_status = "experimental hint" if is_experimental else "non-experimental source entry"

            out.write(f"## {platform_code} {platform_name} {unit} {name}\n\n")
            out.write(f"Platform code: {platform_code}\n\n")
            out.write(f"Platform name: {platform_name}\n\n")
            out.write(f"Platform aliases: {'; '.join(aliases)}\n\n")
            out.write(f"Source kind: {source_kind}\n\n")
            out.write(f"Source file: {source_file}\n\n")
            out.write(f"Source trust rank: {trust_rank}\n\n")
            out.write(f"Event trust rank: {effective_trust_rank}\n\n")
            out.write(f"Event trust note: {effective_trust_note}\n\n")
            out.write(f"Event support status: {support_status}\n\n")
            out.write(f"Unit: {unit}\n\n")
            out.write(f"Event name: {name}\n\n")
            out.write(
                "Event encoding: "
                + event_line(
                    event,
                    [
                        "EventCode",
                        "UMask",
                        "UMaskExt",
                        "FCMask",
                        "PortMask",
                        "ExtSel",
                        "Filter",
                        "FILTER_VALUE",
                    ],
                )
                + "\n\n"
            )
            out.write(f"Counters: {event_line(event, ['Counter', 'CounterType', 'PerPkg'])}\n\n")
            if experimental:
                out.write(f"Experimental: {experimental}\n\n")
            if deprecated:
                out.write(f"Deprecated: {deprecated}\n\n")
            if brief:
                out.write(f"Brief description: {brief}\n\n")
            if public and public != brief:
                out.write(f"Public description: {public}\n\n")
            out.write(
                "Search tags: "
                + " ".join([platform_code, platform_name, *aliases, source_kind, unit, name, source_file])
                + "\n\n"
            )


def write_platform_index(out_path: Path) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("w", encoding="utf-8") as out:
        out.write("# Normalized Uncore Event Metadata\n\n")
        out.write(
            "This directory contains generated Markdown event records. Each event repeats "
            "platform, source, and trust metadata to improve platform-specific retrieval.\n\n"
        )
        out.write("## Trust Order\n\n")
        out.write("1. Intel uncore programming/reference manuals and SDM material.\n")
        out.write("2. Linux kernel uncore PMU driver code and sysfs ABI documentation.\n")
        out.write("3. Intel perfmon public JSON event files.\n")
        out.write("4. Intel perfmon experimental JSON event files.\n")
        out.write("5. Local machine validation with `perf`, MSR reads, or Ocellus exporter output.\n\n")
        out.write("## Platform Aliases\n\n")
        for code, meta in PLATFORMS.items():
            out.write(f"- {code}: {meta['name']}; aliases: {'; '.join(meta['aliases'])}\n")


def generate_intel_perfmon(corpus_dir: Path, out_dir: Path) -> int:
    root = corpus_dir / "intel-perfmon"
    count = 0
    for path in sorted(root.glob("*/events/*uncore*.json")):
        platform_code = path.parts[-3]
        if platform_code not in PLATFORMS:
            continue
        events = load_events(path)
        if not events:
            continue
        experimental = "experimental" in path.name
        source_kind = "Intel perfmon experimental JSON" if experimental else "Intel perfmon public JSON"
        trust_rank = "4" if experimental else "3"
        trust_note = (
            "Experimental perfmon event. Treat as a hint and verify against a manual, Linux support, or hardware."
            if experimental
            else "Public Intel perfmon event JSON. Prefer manuals and Linux driver support when there is disagreement."
        )
        rel = path.relative_to(root).as_posix()
        platform_name, _ = describe_platform(platform_code)
        out_path = out_dir / "intel-perfmon" / f"{platform_code.lower()}-{slug(path.stem)}.md"
        write_event_file(
            out_path,
            title=f"Normalized Intel perfmon events for {platform_code} {platform_name}: {path.name}",
            platform_code=platform_code,
            source_kind=source_kind,
            source_file=rel,
            trust_rank=trust_rank,
            trust_note=trust_note,
            events=events,
        )
        count += len(events)
    return count


def generate_linux_pmu(linux_dir: Path, out_dir: Path) -> int:
    root = linux_dir / "tools/perf/pmu-events/arch/x86"
    count = 0
    for arch, platform_code in LINUX_ARCH_TO_PLATFORM.items():
        arch_dir = root / arch
        if not arch_dir.exists():
            continue
        for path in sorted(arch_dir.glob("*.json")):
            if not (
                "uncore" in path.name
                or path.name.endswith("metrics.json")
                or path.name in {"counter.json", "metricgroups.json"}
            ):
                continue
            events = load_events(path)
            if not events:
                continue
            rel = path.relative_to(root).as_posix()
            platform_name, _ = describe_platform(platform_code)
            out_path = out_dir / "linux-pmu-events" / f"{arch}-{slug(path.stem)}.md"
            write_event_file(
                out_path,
                title=f"Normalized Linux perf PMU events for {platform_code} {platform_name}: {path.name}",
                platform_code=platform_code,
                source_kind="Linux perf PMU event table",
                source_file=rel,
                trust_rank="2",
                trust_note="Linux source-tree perf PMU event table. Verify raw programming support against Linux uncore driver code or Intel manuals when needed.",
                events=events,
            )
            count += len(events)
    return count


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus-dir", required=True, type=Path)
    parser.add_argument("--linux-dir", required=True, type=Path)
    args = parser.parse_args()

    out_dir = args.corpus_dir / "normalized-events"
    write_platform_index(out_dir / "README.md")
    intel_count = generate_intel_perfmon(args.corpus_dir, out_dir)
    linux_count = generate_linux_pmu(args.linux_dir, out_dir)
    print(f"generated normalized event metadata: intel_events={intel_count} linux_events={linux_count}")


if __name__ == "__main__":
    main()
