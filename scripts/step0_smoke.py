#!/usr/bin/env python3
"""Step-0 smoke check of the Ocellus daemon on a host's first allocation.

Runs as root against the running daemon. On the host it only sets PMU
configs, which it always deletes again, and runs short bounded loads; it
writes only to its output directory. docs/step0-checks.md describes the
checks and the expected skips of each family.
"""

import argparse
import glob
import json
import math
import os
import re
import signal
import socket
import subprocess
import sys
import time
import types
import urllib.error
import urllib.request

BAND = (0.5, 2.0)  # measured / expected bytes of a load
RAPL_TOLERANCE = 0.05  # |Ocellus / kernel energy - 1|
MODELS = {0x2D: "SNB", 0x3E: "IVB", 0x3F: "HSX", 0x4F: "BDX", 0x55: "SKX",
          0x6A: "ICX", 0x8F: "SPR", 0xCF: "EMR"}
ARCH = {"SNB": "snb", "IVB": "snb", "HSX": "hsx", "BDX": "hsx", "SKX": "skx",
        "CLX": "skx", "ICX": "icx", "SPR": "spr", "EMR": "spr"}
CLOCKS = {"clockticks", "dclk", "hclockticks", "fixed_clockticks"}
SKIP_RE = re.compile(r"ocellus: skipping (\S+) collector: (.*)")
NOTES = {
    "snb": [
        "Board card c55: check on hardware PCIe TID 0x1e on IVB (only the SNB "
        "guide documents it); whether tid with opc, or opc with nid, counts "
        "despite the guides' one-criterion note; the SNB iMC rst bits; IVB box "
        "ctl bits 17:16; the SNB IMC package mapping when UBOX node ids fall "
        "back to bus order.",
        "Board card c55: IVB LLC_LOOKUP state is FILTER0[22:17] in Intel's "
        "guide and Linux but [23:17] in the perfmon JSON; filter0.raw may "
        "set bit 23 until a hardware check settles it.",
        "Sandy Bridge-EP and Ivy Bridge-EP have no public XSNP or "
        "snoop-response event: no count shows the core snoops IO causes.",
    ],
    "hsx": [
        "docs/bdx-step0-checks.md items 1-11 (IO TID, ItoM vs PCIItoM, IO LLC "
        "lookups, snoop proxies, shared filters, snoop mode, IMC channel ids).",
        "Item 10 (board card c23): IRP counters at a0/a8/b0/b8 (Linux) or "
        "a0/b0/b8/c0 (Intel), and whether IRP0 and IRP1 split the box's "
        "traffic. Until it has run, never sum an IRP config over both sets.",
    ],
    "skx": ["README 'RDT memory bandwidth': per-CPU MBM is valid only with an "
            "IMC or CHA cross-check (erratum SKX99)."],
    "icx": ["The default CHA groups count TOR opcodes IA ITOM and IA/IO "
            "WbMtoI, which Intel lists only as experimental on ICX "
            "(src/metrics/cha/icx.rs:397-409); configs take them as raw "
            "opcodes only."],
    "spr": [
        "Board card c54: default events with experimental-only or no public "
        "encodings (IRP TotalIrpOccupancy, CHA LLC_LOOKUP, LLC_VICTIMS, "
        "SF_EVICTION, TOR IA_ITOM and other hit/miss splits, IMC 0x10/0x03, "
        "0x20/0x03, 0x03/0x33, IIO 0x84 with mask 0xff).",
        "The bundle has no LLC lookup: SPR/EMR list LLC_LOOKUP only as "
        "experimental.",
    ],
}


def ev(alias, tid_en=False, **params):
    return {"alias": alias, **params, **({"tid_en": True} if tid_en else {})}


CLOCK = ev("clockticks")
STATES = ["I", "S", "E", "M", "F"]
IMC = [{"events": [ev("cas_count", op="rd"), ev("cas_count", op="wr")], "fixed": True}]
IIO = [{"events": [ev("data_req_of_cpu", op="mem_write"),
                   ev("data_req_of_cpu", op="mem_read"), CLOCK]}]


def bundle(family):
    """The family's step-0 PUT /pmu-config request."""
    arch = ARCH[family]
    if arch == "snb":
        def tor(opcode, tid):
            group = {"filter1": {"opcodes": [opcode]}, "events": [
                ev("tor_occupancy", tid, kind="opcode", home="any"),
                ev("tor_inserts", tid, kind="opcode", home="any"), CLOCK]}
            return {"filter0": {"tid": "0x1e"}, **group} if tid else group
        lookup = "data_read" if family == "SNB" else "any"
        pmus = {"cha": [tor("PCIItoM", True), tor("PCIRdCur", True),
                        {"filter0": {"states": STATES},
                         "events": [ev("llc_lookup", op=lookup), CLOCK]},
                        tor("PCIItoM", False)],
                "imc": IMC}
    elif arch == "hsx":
        def tor(opcode):
            return {"filter0": {"tid": "0x3e"}, "filter1": {"opcodes": [opcode]}, "events": [
                ev("tor_occupancy", True, kind="opcode", home="any"),
                ev("tor_inserts", True, kind="opcode", home="any"),
                ev("tor_inserts", kind="opcode", home="any"), CLOCK]}
        pmus = {"cha": [tor("ItoM"), tor("PCIRdCur"),
                        {"filter0": {"states": STATES}, "filter1": {"opcodes": ["PCIItoM"]},
                         "events": [ev("tor_occupancy", kind="opcode", home="any"),
                                    ev("tor_inserts", kind="opcode", home="any"),
                                    ev("llc_lookup", op="any"), CLOCK]}],
                "imc": IMC,
                "ha": [{"events": [CLOCK, ev("snoop_resp", op="rspi"),
                                   ev("snoop_resp", op="rsps"), ev("snoop_resp", op="rspifwd")]}],
                "irp": [{"events": [CLOCK, ev("coherent_ops", op="pcitom")]}]}
    elif arch == "skx":
        def tor(opcode):
            return {"filter1": {"opcodes": [opcode]}, "events": [
                ev("tor_occupancy", source="io", result="all"),
                ev("tor_inserts", source="io", result="all"),
                ev("tor_inserts", source="io", result="miss"), CLOCK]}
        pmus = {"cha": [tor("ItoM"), tor("RdCur"),
                        {"filter0": {"states": STATES + ["SF_S", "SF_E", "SF_H"]},
                         "events": [ev("llc_lookup", op="any"), CLOCK]}],
                "imc": IMC, "iio": IIO,
                "irp": [{"events": [CLOCK, ev("coherent_ops", op="pcitom")]}]}
    else:
        def tor(opcode):
            return {"events": [ev("tor_occupancy", source="io", result="all", opcode=opcode),
                               ev("tor_inserts", source="io", result="all", opcode=opcode),
                               ev("tor_inserts", source="io", result="miss", opcode=opcode),
                               CLOCK]}
        lookup = [{"events": [ev("llc_lookup", op="data_read"), CLOCK]}] if arch == "icx" else []
        irp = ev("coherent_ops", op="pcitom") if arch == "icx" else ev("faf_inserts")
        pmus = {"cha": [tor("ITOM"), tor("PCIRDCUR")] + lookup,
                "imc": [{"events": IMC[0]["events"] + [CLOCK], "fixed": True}],
                "iio": IIO, "irp": [{"events": [CLOCK, irp]}]}
    return {"pmus": {box: {**({"grouping": "temporal"} if box == "cha" else {}),
                           "aggregate": "box", "groups": groups}
                     for box, groups in pmus.items()}}


def dma_candidates(arch):
    """(label, box, group, slot, bytes per count, per unit): bundle series
    that count the DMA writes of a disk read."""
    if arch in ("skx", "icx", "spr"):
        return [("IIO DATA_REQ_OF_CPU.MEM_WRITE x 4 B per stack", "iio", "0", "0", 4, True)]
    if arch == "hsx":
        return [("CBo TOR_INSERTS ItoM, tid 0x3e", "cha", "0", "1", 64, False),
                ("CBo TOR_INSERTS ItoM, any tid", "cha", "0", "2", 64, False),
                ("CBo TOR_INSERTS PCIItoM, any tid", "cha", "2", "1", 64, False),
                ("IRP COHERENT_OPS.PCITOM per set", "irp", "0", "1", 64, True),
                ("IRP COHERENT_OPS.PCITOM, both sets", "irp", "0", "1", 64, False)]
    return [("CBo TOR_INSERTS PCIItoM, tid 0x1e", "cha", "0", "1", 64, False),
            ("CBo TOR_INSERTS PCIItoM, any tid", "cha", "3", "1", 64, False)]


def expected_collectors(family):
    arch = ARCH[family]
    names = {"cha", "imc", "pcu", "rapl"}
    names |= {"iio"} if arch in ("skx", "icx", "spr") else set()
    names |= {"irp"} if family != "SNB" else set()
    names |= {"ha"} if arch == "hsx" else set()
    names |= {"rdt"} if arch != "snb" else set()
    return names


def in_band(measured, expected):
    return expected > 0 and BAND[0] <= measured / expected <= BAND[1]


def unit_factor(ratio):
    """n != 0 if ratio is within RAPL_TOLERANCE of 2^n, else None."""
    if ratio <= 0:
        return None
    n = round(math.log2(ratio))
    return n if n and abs(ratio / 2 ** n - 1) <= RAPL_TOLERANCE else None


def integrate(samples, t0, t1):
    """Per-package bytes over (t0, t1] from the rate samples (start, end,
    {package: B/s}) that end there, scaled up for missed samples, and the
    fraction of the span the samples cover."""
    tolerance = 0.25 * min((end - start for start, end, _ in samples), default=0)
    inside = [s for s in samples if t0 + tolerance < s[1] <= t1 + tolerance]
    covered = sum(end - start for start, end, _ in inside)
    if t1 <= t0 or covered <= 0:
        return {}, 0.0
    totals = {}
    for start, end, rates in inside:
        for package, rate in rates.items():
            totals[package] = totals.get(package, 0.0) + rate * (end - start)
    return {p: v * (t1 - t0) / covered for p, v in totals.items()}, covered / (t1 - t0)


def excess(idle, load, key):
    """Counts of series key=(box, package, unit, group, slot) between the
    snapshots load=(s0, s1) beyond its rate between idle=(h0, h1), each rate
    per running second of the group, over the load's wall time."""
    box, package, _, group, _ = key
    run = (box, package, group)

    def rate(a, b):
        running = b["running"][run] - a["running"][run]
        if running <= 0:
            raise RuntimeError(f"group {run} did not run")
        return (b["counts"][key] - a["counts"][key]) / running
    wall = load[1]["ends"][box] - load[0]["ends"][box]
    return (rate(*load) - rate(*idle)) * wall


# --- the daemon and the host ---------------------------------------------

LABEL_RE = re.compile(r'([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"')
SAMPLE_RE = re.compile(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{(.*)\})?\s+(\S+)")


def parse_metrics(text):
    metrics = {}
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        match = SAMPLE_RE.match(line)
        if not match:
            raise ValueError(f"unparsable /metrics line {line!r}")
        labels = {k: re.sub(r"\\(.)", r"\1", v) for k, v in LABEL_RE.findall(match.group(2) or "")}
        metrics.setdefault(match.group(1), []).append((labels, float(match.group(3))))
    return metrics


def series(metrics, name, **match):
    return [(l, v) for l, v in metrics.get(name, []) if all(l.get(k) == w for k, w in match.items())]


def value(metrics, name, **match):
    found = series(metrics, name, **match)
    return found[0][1] if found else None


def per_package(metrics, name):
    totals = {}
    for labels, v in series(metrics, name):
        totals[labels["package"]] = totals.get(labels["package"], 0.0) + v
    return totals


class Daemon:
    def __init__(self, base_url):
        self.base = base_url.rstrip("/")
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def request(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        headers = {"Content-Type": "application/json"} if data else {}
        request = urllib.request.Request(self.base + path, data, headers, method=method)
        try:
            with self.opener.open(request, timeout=60) as response:
                return response.status, response.read().decode()
        except urllib.error.HTTPError as error:
            return error.code, error.read().decode()

    def metrics(self):
        status, text = self.request("GET", "/metrics")
        if status != 200:
            raise RuntimeError(f"GET /metrics: {status}")
        return parse_metrics(text)

    def config(self, method, body=None):
        status, text = self.request(method, "/pmu-config", body)
        try:
            return status, json.loads(text)
        except ValueError:
            return status, text


def host_path(root, path):
    return os.path.join(root, path.lstrip("/"))


def read(root, path):
    with open(host_path(root, path)) as file:
        return file.read().strip()


def read_cpuinfo(proc_root):
    cpus, cpu = [], {}
    for line in read(proc_root, "/proc/cpuinfo").splitlines() + [""]:
        if not line.strip():
            cpus, cpu = (cpus + [cpu] if cpu else cpus), {}
            continue
        key, _, val = line.partition(":")
        cpu[key.strip()] = val.strip()
    return cpus


def read_powercap(sys_root):
    """Kernel RAPL energy by (package, domain): [(zone, uJ, wrap range)]."""
    zones = {}
    for path in sorted(glob.glob(host_path(sys_root, "/sys/class/powercap/intel-rapl:*"))):
        parts = os.path.basename(path).split(":")
        top = re.match(r"package-(\d+)", read(sys_root, f"/sys/class/powercap/{':'.join(parts[:2])}/name"))
        domain = "package" if len(parts) == 2 else read("/", f"{path}/name")
        if top and domain in ("package", "dram"):
            zones.setdefault((top.group(1), domain), []).append(
                (parts, int(read("/", f"{path}/energy_uj")), int(read("/", f"{path}/max_energy_range_uj"))))
    return zones


def cpu_list(text):
    cpus = []
    for part in filter(None, text.split(",")):
        low, _, high = part.partition("-")
        cpus += range(int(low), int(high or low) + 1)
    return cpus


def run_command(argv, timeout=60):
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError) as error:
        return None, str(error)
    return (result.stdout if result.returncode == 0 else None), result.stderr.strip()


class Report:
    def __init__(self, args):
        self.args, self.checks, self.notes, self.identity = args, [], [], {}
        self.started = time.time()

    def add(self, name, status, measured=None, expected=None, note="", expected_skip=False):
        entry = {"name": name, "status": status, "measured": measured, "expected": expected, "note": note}
        if status == "skip":
            entry["expected_skip"] = expected_skip
        self.checks.append(entry)
        return status == "pass"

    def finish(self, code=None):
        ok = all(c["status"] == "pass" or c.get("expected_skip") for c in self.checks)
        code = code if code is not None else (0 if ok and self.checks else 1)
        report = {"host": socket.gethostname(), "started": self.started, "finished": time.time(),
                  "args": vars(self.args), "identity": self.identity, "checks": self.checks,
                  "family_notes": self.notes, "exit_code": code}
        lines = [f"Ocellus step 0 on {report['host']}: " + ", ".join(
            f"{k} {self.identity[k]}" for k in ("family", "model", "stepping", "sockets", "kernel")
            if k in self.identity)]
        for c in self.checks:
            skip = (" (expected)" if c.get("expected_skip") else " (UNEXPECTED)") if c["status"] == "skip" else ""
            lines.append(f"{c['status'].upper():4} {c['name']}{skip}{': ' + c['note'] if c['note'] else ''}")
        lines += ["Open hardware questions (not tested):"] * bool(self.notes) + [f"  - {n}" for n in self.notes]
        lines.append(f"Result: {'PASS' if code == 0 else 'FAIL'} (exit {code})")
        os.makedirs(self.args.out, exist_ok=True)
        with open(os.path.join(self.args.out, "report.json"), "w") as file:
            json.dump(report, file, indent=1, sort_keys=True)
        with open(os.path.join(self.args.out, "summary.txt"), "w") as file:
            file.write("\n".join(lines) + "\n")
        print("\n".join(lines))
        return code


# --- the checks ----------------------------------------------------------

def check_identity(args, report, metrics):
    cpus = read_cpuinfo(args.proc_root)
    host = types.SimpleNamespace()
    host.cpu_package = {int(c["processor"]): c.get("physical id", "0") for c in cpus}
    host.packages = sorted(set(host.cpu_package.values()), key=int)
    host.flags = set(cpus[0].get("flags", "").split())
    model, stepping = int(cpus[0]["model"]), int(cpus[0]["stepping"])
    intel = cpus[0].get("vendor_id") == "GenuineIntel" and cpus[0].get("cpu family") == "6"
    host.family = MODELS.get(model) if intel else None
    if host.family == "SKX" and stepping >= 5:
        host.family = "CLX"
    # Windows run between whole multiples of the interval, to within a few ms.
    windows = sorted(v - (value(metrics, "ocellus_sample_window_start_timestamp_seconds", **l) or v)
                     for l, v in series(metrics, "ocellus_sample_window_end_timestamp_seconds"))
    windows = [w for w in windows if w > 0] or [1.0]
    host.interval = round(windows[len(windows) // 2], 2)
    host.poll = min(0.25, host.interval / 4)
    unit_out, unit_error = run_command(["systemctl", "show", args.unit, "-p", "MainPID", "-p", "InvocationID"])
    host.unit = dict(line.partition("=")[::2] for line in (unit_out or "").splitlines())
    if host.unit.get("MainPID", "0") != "0":
        try:
            host.unit["binary"] = os.readlink(host_path(args.proc_root, f"/proc/{host.unit['MainPID']}/exe"))
        except OSError as error:
            host.unit["binary_error"] = str(error)
    processor = (series(metrics, "processor_info") or [({}, 0)])[0][0]
    report.identity = {
        "family": host.family, "model": f"{model:#04x}", "stepping": stepping,
        "sockets": len(host.packages), "cpus": len(cpus), "kernel": read(args.proc_root, "/proc/sys/kernel/osrelease"),
        "brand": cpus[0].get("model name"), "measure_interval_seconds": host.interval,
        "collector_info": (series(metrics, "ocellus_collector_info") or [({}, 0)])[0][0],
        "processor_info": processor,
        "ocellus_version": (series(metrics, "ocellus_info") or [({}, 0)])[0][0].get("version"),
        "unit": host.unit or {"error": unit_error},
    }
    report.notes = NOTES[ARCH[host.family]] if host.family else []
    same_model = processor.get("model") == str(model)
    report.add("identity", "pass" if host.family and same_model else "fail", report.identity,
               {"model": sorted(f"{m:#04x}" for m in MODELS), "processor_info.model": str(model)},
               "" if host.family else "not a supported Intel server model")
    return host if host.family else None


def collector_packages(metrics, name):
    prefixes = (f"ocellus_{name}_", f"ocellus_haswell_{name}_")
    return {l["package"] for metric, rows in metrics.items() if metric.startswith(prefixes)
            for l, _ in rows if "package" in l}


def check_collectors(args, report, host, m1, m2):
    expected = expected_collectors(host.family)
    running = {l["collector"] for l, _ in series(m2, "ocellus_samples_total")} - {"tsc"}
    for name in sorted(expected | running):
        before = value(m1, "ocellus_samples_total", collector=name)
        after = value(m2, "ocellus_samples_total", collector=name)
        packages = collector_packages(m2, name)
        measured = {"samples": [before, after], "packages": sorted(packages, key=int),
                    "stopped": value(m2, "ocellus_collector_stopped", collector=name) == 1}
        healthy = (before is not None and after is not None and after > before
                   and not measured["stopped"] and packages >= set(host.packages))
        wanted = name in expected or name == "interconnect"
        report.add(f"collectors.{name}", "pass" if healthy and wanted else "fail", measured,
                   {"packages": host.packages} if wanted else "absent",
                   "" if wanted else f"not expected on {host.family}")
    if "rdt" in expected:
        occupancy = collector_packages({k: v for k, v in m2.items() if k == "ocellus_rdt_l3_occupancy_bytes"}, "rdt")
        mbm = collector_packages({k: v for k, v in m2.items() if "memory_bandwidth" in k}, "rdt")
        want_mbm = host.family != "HSX" and bool({"cqm_mbm_total", "cqm_mbm_local"} & host.flags)
        ok = occupancy >= set(host.packages) and (mbm >= set(host.packages) if want_mbm else not mbm)
        report.add("collectors.rdt_features", "pass" if ok else "fail",
                   {"occupancy_packages": sorted(occupancy), "mbm_packages": sorted(mbm)},
                   {"occupancy": host.packages, "mbm": host.packages if want_mbm else []})
    if args.journal_file:
        with open(args.journal_file) as file:
            journal, error = file.read(), ""
    else:
        match = ([f"_SYSTEMD_INVOCATION_ID={host.unit['InvocationID']}"] if host.unit.get("InvocationID")
                 else ["-u", args.unit, "-b"])
        journal, error = run_command(["journalctl", "--no-pager", "-o", "cat", *match])
    if journal is None:
        return report.add("collectors.journal", "skip", note=f"journal unreadable: {error}")
    skips = []
    for line in journal.splitlines():
        match = SKIP_RE.search(line)
        if match:
            name, reason = match.group(1).lower(), match.group(2)
            ok = name not in expected and (name != "interconnect" or "links" in reason)
            skips.append({"collector": name, "reason": reason, "expected": ok})
    report.add("collectors.journal", "pass" if all(s["expected"] for s in skips) else "fail",
               skips, "skips of collectors the family lacks, or of interconnect without links")


def snapshot(metrics):
    return {"counts": {(l["box_type"], l["package"], l["unit"], l["group"], l["slot"]): v
                       for l, v in series(metrics, "ocellus_uncore_count_total")},
            "running": {(l["box_type"], l["package"], l["group"]): v
                        for l, v in series(metrics, "ocellus_uncore_running_seconds_total")},
            "ends": {l["collector"]: v for l, v in series(metrics, "ocellus_sample_window_end_timestamp_seconds")},
            "starts": {l["collector"]: v for l, v in series(metrics, "ocellus_sample_window_start_timestamp_seconds")}}


def wait_for(daemon, host, done, on_scrape=None, timeout=60):
    """Scrapes /metrics every poll period until done(metrics)."""
    deadline = time.time() + timeout
    while True:
        metrics = daemon.metrics()
        if on_scrape:
            on_scrape(metrics)
        if done(metrics):
            return metrics
        if time.time() > deadline:
            raise RuntimeError("timed out waiting for the daemon's samples")
        time.sleep(host.poll)


def start_load(argv, cpu):
    affinity = (lambda: os.sched_setaffinity(0, {cpu})) if cpu is not None else None
    return subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
                            preexec_fn=affinity, env={"LC_ALL": "C", "PATH": os.environ.get("PATH", "")})


def run_load(daemon, host, argv, collectors, on_scrape=None, cpu=None):
    """Runs a bounded load while scraping; returns its bytes and the metrics
    from just before it and from once every collector has sampled past it."""
    before = daemon.metrics()
    if on_scrape:
        on_scrape(before)
    process = start_load(argv, cpu)
    try:
        wait_for(daemon, host, lambda m: process.poll() is not None, on_scrape, timeout=300)
    finally:
        if process.poll() is None:
            process.kill()
    ended = time.time()
    stderr = process.stderr.read()
    copied = re.search(r"^(\d+) bytes", stderr, re.M)
    if process.returncode != 0 or not copied:
        raise RuntimeError(f"{' '.join(argv)} failed: {stderr.strip()}")
    after = wait_for(daemon, host, lambda m: all(
        (value(m, "ocellus_sample_window_start_timestamp_seconds", collector=c) or 0) >= ended
        for c in collectors), on_scrape)
    return int(copied.group(1)), before, after


def pick_disk(args, host):
    """The disk to read from, its size and its package (None if unknown)."""
    if args.disk:
        name, size = os.path.basename(os.path.realpath(args.disk)), None
    else:
        out, error = run_command(["lsblk", "-J", "-b", "-d", "-o", "NAME,TYPE,SIZE,TRAN,RM"])
        if out is None:
            raise RuntimeError(f"lsblk failed: {error}")
        disks = [d for d in json.loads(out)["blockdevices"] if d["type"] == "disk"
                 and str(d.get("rm")).lower() not in ("1", "true")
                 and not re.match(r"(zram|ram|loop|nbd|rbd)", d["name"])]
        if not disks:
            return None, None, None
        best = max(disks, key=lambda d: (d.get("tran") == "nvme", int(d["size"])))
        name, size = best["name"], int(best["size"])
    path = os.path.realpath(host_path(args.sys_root, f"/sys/block/{name}"))
    functions = re.findall(r"[0-9a-f]{4}:[0-9a-f]{2}:[0-9a-f]{2}\.[0-9a-f]", path)
    package = None
    if functions:
        try:
            cpus = cpu_list(read(args.sys_root, f"/sys/bus/pci/devices/{functions[-1]}/local_cpulist"))
            package = host.cpu_package.get(cpus[0]) if cpus else None
        except OSError:
            pass
    return f"/dev/{name}", size, package


def run_bundle(args, report, daemon, host, state):
    request = bundle(host.family)
    boxes = sorted(request["pmus"])
    state["configured"] = True
    status, reply = daemon.config("PUT", request)
    ids = {b: reply[b]["config_id"] for b in boxes} if status == 200 and isinstance(reply, dict) \
        and set(reply) == set(boxes) else None
    if not report.add("bundle.put", "pass" if ids else "fail",
                      {"status": status, "reply": reply, "request": request},
                      {"status": 200, "box_types": boxes}):
        return
    first = wait_for(daemon, host, lambda m: all(value(m, "ocellus_uncore_config_id", box_type=b) == ids[b]
                                                 for b in boxes))
    info = [l for l, _ in series(first, "ocellus_uncore_event_info") if int(l["config_id"]) in ids.values()]
    expected_keys = {(l["box_type"], l["package"], str(u), l["group"], l["slot"])
                     for l in info for u in cpu_list(l["units"])}
    groups = len(request["pmus"]["cha"]["groups"])
    seen = {"prev": snapshot(first), "decreases": [], "precession": [], "ids": [], "cha_end": None}

    def on_scrape(metrics):
        now = snapshot(metrics)
        seen["decreases"] += [k for k, v in now["counts"].items() if v < seen["prev"]["counts"].get(k, 0)]
        seen["ids"] += [b for b in boxes if value(metrics, "ocellus_uncore_config_id", box_type=b) != ids[b]]
        end = now["ends"].get("cha")
        if end is not None and end != seen["cha_end"]:
            seen["cha_end"] = end
            cycle = round(end / host.interval)
            first_group = value(metrics, "ocellus_uncore_schedule_first_group", box_type="cha")
            seen["precession"].append((end, first_group, cycle % groups))
        seen["prev"] = now

    hold_end = time.time() + args.hold_seconds
    last = wait_for(daemon, host, lambda m: time.time() >= hold_end, on_scrape, args.hold_seconds + 60)
    h0, h1 = snapshot(first), snapshot(last)
    report.add("bundle.event_info", "pass" if {l["package"] for l in info} >= set(host.packages) else "fail",
               {"config_ids": ids, "event_info": info}, {"packages": host.packages})
    missing = sorted(expected_keys - set(h1["counts"]))
    idle_clocks = sorted(k for k in expected_keys - set(missing) if k in h0["counts"]
                         and h1["counts"][k] <= h0["counts"][k]
                         and any(l["alias"] in CLOCKS and (l["box_type"], l["package"], l["group"], l["slot"])
                                 == (k[0], k[1], k[3], k[4]) for l in info))
    unchanged = sum(1 for k in expected_keys - set(missing) if h1["counts"][k] == h0["counts"].get(k))
    not_running = sorted(k for k in {(l["box_type"], l["package"], l["group"]) for l in info}
                         if h1["running"].get(k, 0) <= 0)
    ok = not (missing or seen["decreases"] or seen["ids"] or idle_clocks or not_running)
    report.add("bundle.series", "pass" if ok else "fail",
               {"series": len(expected_keys), "missing": missing[:20], "decreasing": seen["decreases"][:20],
                "config_id_changes": seen["ids"][:20], "clocks_not_ticking": idle_clocks[:20],
                "groups_not_running": not_running, "series_without_counts": unchanged,
                "hold_seconds": h1["ends"]["cha"] - h0["ends"]["cha"]},
               "every series present, non-decreasing; clock series ticking; running seconds > 0")
    wrong = [p for p in seen["precession"] if p[1] != p[2]]
    report.add("bundle.precession", "pass" if len(seen["precession"]) >= 3 and not wrong else "fail",
               {"samples": len(seen["precession"]), "mismatches": wrong[:20]},
               f"first_group == cycle mod {groups}")
    if args.no_load:
        report.add("dma", "skip", note="--no-load", expected_skip=True)
        report.add("imc.channels", "skip", note="--no-load", expected_skip=True)
        return
    disk, size, disk_package = pick_disk(args, host)
    candidates = dma_candidates(ARCH[host.family])
    if disk is None:
        report.add("dma", "skip", note="no local disk found; pass --disk")
    else:
        mib = min(args.dma_mib, size >> 20) if size else args.dma_mib
        dma_boxes = sorted({c[1] for c in candidates})
        nbytes, before, after = run_load(daemon, host, ["dd", f"if={disk}", "of=/dev/null", "bs=1M",
                                                        f"count={mib}", "iflag=direct"], dma_boxes)
        s0, s1 = snapshot(before), snapshot(after)
        results = []
        for label, box, group, slot, scale, per_unit in candidates:
            totals = {}
            for key in h1["counts"]:
                if key[0] == box and key[3] == group and key[4] == slot and key in s1["counts"]:
                    where = (key[1], key[2]) if per_unit else (key[1], "all")
                    totals[where] = totals.get(where, 0.0) + excess((h0, h1), (s0, s1), key) * scale
            if not totals:
                raise RuntimeError(f"no {box} series for {label}")
            where, measured = max(totals.items(), key=lambda t: t[1])
            results.append({"series": label, "package": where[0], "unit": where[1], "bytes": measured,
                            "ratio": measured / nbytes, "on_disk_package": disk_package in (None, where[0])})
        ok = any(in_band(r["bytes"], nbytes) and r["on_disk_package"] for r in results)
        report.add("dma", "pass" if ok else "fail", results,
                   {"bytes": nbytes, "band": BAND, "disk": disk, "disk_package": disk_package},
                   "largest-delta unit of each series; any series in band on the disk's package passes")
    cpu = max(c for c, p in host.cpu_package.items() if p == host.packages[0])
    nbytes, before, after = run_load(daemon, host, imc_load(args), ["imc"], cpu=cpu)
    s0, s1 = snapshot(before), snapshot(after)
    channels = {k[2]: excess((h0, h1), (s0, s1), k) * 64 for k in h1["counts"]
                if k[0] == "imc" and k[1] == host.packages[0] and k[3:] == ("0", "1") and k in s1["counts"]}
    if not channels:
        raise RuntimeError(f"no imc cas_count wr series on package {host.packages[0]}")
    total = sum(channels.values())
    silent = sorted(u for u, b in channels.items() if b < 0.05 * total / len(channels))
    nodes = numa_nodes_per_package(args, host)
    allowed = len(channels) - math.ceil(len(channels) / nodes)
    report.add("imc.channels", "pass" if in_band(total, nbytes) and len(silent) <= allowed else "fail",
               {"write_bytes_by_channel": channels, "total": total, "silent": silent, "numa_nodes_per_package": nodes},
               {"bytes": nbytes, "band": BAND, "silent_at_most": allowed},
               f"bundle cas_count wr x 64 on package {host.packages[0]}, cpu {cpu}")


def imc_load(args):
    return ["dd", "if=/dev/zero", "of=/dev/null", "bs=1G", f"count={args.imc_gib}", "iflag=fullblock"]


def numa_nodes_per_package(args, host):
    nodes = {}
    for path in glob.glob(host_path(args.sys_root, "/sys/devices/system/node/node[0-9]*/cpulist")):
        cpus = cpu_list(read("/", path))
        if cpus:
            nodes[host.cpu_package[cpus[0]]] = nodes.get(host.cpu_package[cpus[0]], 0) + 1
    return max(nodes.values(), default=1)


def run_default_loads(args, report, daemon, host):
    """IMC write bytes against a zero-fill load and RAPL against powercap,
    with the daemon's default groups."""
    name = "ocellus_haswell_imc_write_bytes_per_second" if ARCH[host.family] == "hsx" \
        else "ocellus_imc_write_bytes_per_second"
    imc, rapl = [], []

    def on_scrape(metrics):
        start = value(metrics, "ocellus_sample_window_start_timestamp_seconds", collector="imc")
        end = value(metrics, "ocellus_sample_window_end_timestamp_seconds", collector="imc")
        if series(metrics, name) and (not imc or imc[-1][1] != end):
            imc.append((start, end, per_package(metrics, name)))
        end = value(metrics, "ocellus_sample_window_end_timestamp_seconds", collector="rapl")
        if end is not None and (not rapl or rapl[-1][0] != end):
            energy = {(l["package"], l["domain"]): 0.0 for l, _ in series(metrics, "ocellus_rapl_energy_joules_total")}
            for l, v in series(metrics, "ocellus_rapl_energy_joules_total"):
                energy[(l["package"], l["domain"])] += v
            rapl.append((end, energy, read_powercap(args.sys_root), time.time()))

    wait_for(daemon, host, lambda m: len(imc) >= 2, on_scrape)
    del imc[0]  # may have started under the bundle
    idle_end = time.time() + args.idle_seconds
    wait_for(daemon, host, lambda m: time.time() >= idle_end, on_scrape, args.idle_seconds + 60)
    if args.no_load:
        report.add("imc.default_write_bytes", "skip", note="--no-load", expected_skip=True)
    else:
        idle = list(imc)
        cpu = max(c for c, p in host.cpu_package.items() if p == host.packages[0])
        nbytes, _, after = run_load(daemon, host, imc_load(args), ["imc"], on_scrape, cpu)
        t0, t1 = idle[-1][1], value(after, "ocellus_sample_window_start_timestamp_seconds", collector="imc")
        idle_bytes, _ = integrate(idle, idle[0][0], idle[-1][1])
        load_bytes, coverage = integrate(imc, t0, t1)
        extra = {p: b - idle_bytes.get(p, 0.0) / (idle[-1][1] - idle[0][0]) * (t1 - t0) for p, b in load_bytes.items()}
        package = host.packages[0]
        ok = coverage >= 0.5 and in_band(extra.get(package, 0.0), nbytes)
        report.add("imc.default_write_bytes", "pass" if ok else "fail",
                   {"bytes_by_package": extra, "coverage": coverage, "span_seconds": t1 - t0, "cpu": cpu},
                   {"package": package, "bytes": nbytes, "band": BAND},
                   f"{name} on package {package}, minus its idle level")
    if len(rapl) < 3:
        return report.add("rapl", "fail", note=f"{len(rapl)} RAPL samples")
    first, last = rapl[0], rapl[-1]
    for key in sorted(set(first[1]) | set(first[2])):
        check = f"rapl.{key[1]}.{key[0]}"
        if key not in first[2]:
            report.add(check, "skip", note="no powercap domain", expected_skip=True)
            continue
        if key not in first[1]:
            report.add(check, "fail", note="Ocellus exports no energy for a powercap domain")
            continue
        kernel_uj = 0
        for a, b in zip(rapl, rapl[1:]):
            for (zone, before, wrap), (_, now, _) in zip(a[2][key], b[2][key]):
                kernel_uj += now - before if now >= before else now + wrap - before
        kernel = kernel_uj / 1e6 * (last[0] - first[0]) / (last[3] - first[3])
        ocellus = last[1][key] - first[1][key]
        ratio = ocellus / kernel if kernel > 0 else float("inf")
        factor = unit_factor(ratio)
        report.add(check, "pass" if abs(ratio - 1) <= RAPL_TOLERANCE else "fail",
                   {"ocellus_joules": ocellus, "kernel_joules": kernel, "ratio": ratio,
                    "seconds": last[0] - first[0]}, {"ratio": [1 - RAPL_TOLERANCE, 1 + RAPL_TOLERANCE]},
                   f"unit bug: off by 2^{factor}" if factor else "")


def interrupt(signum, frame):
    raise KeyboardInterrupt


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base-url", default="http://127.0.0.1:8080")
    parser.add_argument("--unit", default="rhea-telemetry-ocellus")
    parser.add_argument("--out", default=f"/tmp/ocellus-step0-{socket.gethostname()}-"
                        f"{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}")
    parser.add_argument("--proc-root", default="/")
    parser.add_argument("--sys-root", default="/")
    parser.add_argument("--journal-file", help="read the daemon's journal from this file")
    parser.add_argument("--no-load", action="store_true", help="skip the IMC and DMA loads")
    parser.add_argument("--hold-seconds", type=float, default=30)
    parser.add_argument("--idle-seconds", type=float, default=10)
    parser.add_argument("--imc-gib", type=int, default=8)
    parser.add_argument("--dma-mib", type=int, default=4096)
    parser.add_argument("--disk", help="block device for the DMA read (default: from lsblk)")
    args = parser.parse_args()
    signal.signal(signal.SIGTERM, interrupt)
    report, daemon, state = Report(args), Daemon(args.base_url), {"configured": False}
    try:
        try:
            status, current = daemon.config("GET")
        except OSError as error:
            status, current = None, str(error)
        if status != 200 or current != {}:
            report.add("preflight", "fail", {"status": status, "pmu_config": current}, {},
                       "GET /pmu-config is not {}: not starting")
            return report.finish(2)
        m1 = daemon.metrics()
        host = check_identity(args, report, m1)
        if host:
            time.sleep(2 * host.interval + 0.1)
            check_collectors(args, report, host, m1, daemon.metrics())
            run_bundle(args, report, daemon, host, state)
            if state["configured"]:
                state["configured"] = False
                delete(report, daemon)
            run_default_loads(args, report, daemon, host)
    except KeyboardInterrupt:
        report.add("aborted", "fail", note="interrupted")
    except Exception as error:
        report.add("aborted", "fail", note=f"{type(error).__name__}: {error}")
    finally:
        if state["configured"]:
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            delete(report, daemon)
    return report.finish()


def delete(report, daemon):
    try:
        deleted = daemon.config("DELETE")
        after = daemon.config("GET")
    except OSError as error:
        deleted = after = (None, str(error))
    report.add("bundle.delete", "pass" if deleted == (200, {}) and after == (200, {}) else "fail",
               {"delete": deleted, "get": after}, {"delete": [200, {}], "get": [200, {}]})


if __name__ == "__main__":
    sys.exit(main())
