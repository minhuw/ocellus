#!/usr/bin/env python3
"""Fixture test of step0_smoke.py: a fake daemon and fake /proc and /sys.

Run with `python3 -I scripts/test_step0_smoke.py`.
"""

import hashlib
import http.server
import importlib.util
import io
import json
import math
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
import types
import unittest

SCRIPT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "step0_smoke.py")
INTERVAL = 0.25
PACKAGES = ("0", "1")
UNITS = {"cha": 4, "imc": 2, "iio": 3, "irp": 2}
POWER = {"package": 100.0, "dram": 10.0}
JOURNAL = """ocellus: starting CHA collector
ocellus: starting IIO collector
ocellus: starting IMC collector
ocellus: skipping HA collector: HA collection is not supported for SkylakeXeon
ocellus: starting IRP collector
ocellus: starting RAPL collector
ocellus: starting PCU collector
ocellus: skipping interconnect collector: failed to discover any Skylake-SP UPI links
ocellus: starting RDT collector
"""

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("step0_smoke", SCRIPT)
step0 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(step0)


class FakeOcellus:
    """A 2-socket Cascade Lake daemon whose counts and energy follow the clock."""

    def __init__(self, root, config=None, dram_factor=1.0, imc_factor=1.0):
        self.root, self.dram_factor, self.imc_factor = root, dram_factor, imc_factor
        self.config, self.started, self.calls, self.loads = dict(config or {}), {}, [], []
        self.t0 = time.time()
        self.lock = threading.Lock()
        for box in self.config:
            self.started[box] = self.t0

    def energy(self, package, domain, at):
        return POWER[domain] * (at - self.t0) * (1 + int(package))

    def moved(self, kind, at):
        """Bytes the loads of a kind ("imc" or "dma") have moved by time at."""
        return sum(n * min(1.0, max(0.0, (at - s) / (e - s))) for k, s, e, n in self.loads if k == kind)

    def start_load(self, argv, cpu):
        """Stands in for step0_smoke.start_load: a 0.6 s load of dd's size."""
        size = {"bs=1G": 1 << 30, "bs=1M": 1 << 20}[next(a for a in argv if a.startswith("bs="))]
        nbytes = size * int(next(a for a in argv if a.startswith("count="))[6:])
        start = time.time()
        self.loads.append(("dma" if "iflag=direct" in argv else "imc", start, start + 0.6, nbytes))
        return types.SimpleNamespace(poll=lambda: 0 if time.time() >= start + 0.6 else None,
                                     returncode=0, kill=lambda: None,
                                     stderr=io.StringIO(f"{nbytes} bytes copied, 0.6 s\n"))

    def summary(self):
        return {box: {"config_id": self.config_id(box), "start_timestamp_seconds": self.started[box]}
                for box in self.config}

    def config_id(self, box):
        digest = hashlib.sha256(json.dumps(self.config[box], sort_keys=True).encode()).hexdigest()
        return int(digest, 16) % (1 << 53)

    def metrics(self):
        now = time.time()
        for package in PACKAGES:
            for zone, domain in ((f"intel-rapl:{package}", "package"), (f"intel-rapl:{package}:0", "dram")):
                with open(os.path.join(self.root, "sys/class/powercap", zone, "energy_uj"), "w") as file:
                    file.write(f"{int(self.energy(package, domain, now) * 1e6) % (1 << 32)}\n")
        cycle = math.floor(now / INTERVAL)
        start, end = (cycle - 1) * INTERVAL, cycle * INTERVAL
        lines = ['ocellus_info{version="0.12.0"} 1',
                 'ocellus_collector_info{cha_supported="true",ha_supported="false",iio_supported="true"} 1',
                 'processor_info{brand="Fixture Xeon",family="6",model="85",vendor="GenuineIntel"} 1']
        for collector in ("cha", "iio", "irp", "imc", "pcu", "rapl", "rdt", "tsc"):
            lines += [f'ocellus_samples_total{{collector="{collector}"}} {cycle - math.floor(self.t0 / INTERVAL)}',
                      f'ocellus_sample_window_start_timestamp_seconds{{collector="{collector}"}} {start}',
                      f'ocellus_sample_window_end_timestamp_seconds{{collector="{collector}"}} {end}']
        for package in PACKAGES:
            scope = f'die="unknown",die_group="unknown",package="{package}"'
            defaults = {"cha": f"ocellus_cha_frequency_hz{{{scope}}} 2e9",
                        "imc": f"ocellus_imc_write_bytes_per_second{{{scope}}} "
                               f"{1e8 + (package == '0') * self.imc_factor * (self.moved('imc', end) - self.moved('imc', start)) / INTERVAL}",
                        "iio": f'ocellus_iio_pcie_write_bytes_per_second{{{scope},port="0",stack="pcie0"}} 0',
                        "irp": f'ocellus_irp_frequency_hz{{{scope},stack="pcie0"}} 1e9'}
            lines += [line for box, line in defaults.items() if box not in self.config]
            lines += [f'ocellus_pcu_frequency_hz{{package="{package}"}} 2e9',
                      f'ocellus_rdt_l3_occupancy_bytes{{core="0",cpu="{package}",{scope}}} 1e6',
                      f'ocellus_rdt_memory_bandwidth_bytes_per_second{{core="0",cpu="{package}",{scope},traffic="local"}} 1e6']
            for domain, factor in (("package", 1.0), ("dram", self.dram_factor)):
                lines.append(f'ocellus_rapl_energy_joules_total{{{scope},domain="{domain}"}} '
                             f"{factor * self.energy(package, domain, end)}")
        for box, request in self.config.items():
            groups, config_id = request["groups"], self.config_id(box)
            temporal = request.get("grouping") == "temporal"
            elapsed = max(0.0, end - self.started[box])
            lines += [f'ocellus_uncore_config_id{{box_type="{box}"}} {config_id}',
                      f'ocellus_uncore_config_start_timestamp_seconds{{box_type="{box}"}} {self.started[box]}']
            if temporal:
                lines.append(f'ocellus_uncore_schedule_first_group{{box_type="{box}"}} {cycle % len(groups)}')
            for package in PACKAGES:
                for index, group in enumerate(groups):
                    running = elapsed / len(groups) if temporal else elapsed
                    labels = f'box_type="{box}",package="{package}",group="{index}"'
                    lines.append(f"ocellus_uncore_running_seconds_total{{{labels}}} {running}")
                    slots = [(str(s), e["alias"]) for s, e in enumerate(group["events"]) if e]
                    slots += [("fixed", "dclk")] if group.get("fixed") else []
                    for number, (slot, alias) in enumerate(slots):
                        lines.append(f'ocellus_uncore_event_info{{config_id="{config_id}",{labels},slot="{slot}",'
                                     f'units="0-{UNITS[box] - 1}",alias="{alias}"}} 1')
                        for unit in range(UNITS[box]):
                            count = 1000 * running * (number + 1)
                            if (box, package, index, slot) == ("imc", "0", 0, "1"):
                                count += self.moved("imc", end) / 64 / UNITS["imc"]
                            if (box, package, unit, index, slot) == ("iio", "1", 1, 0, "0"):
                                count += self.moved("dma", end) / 4
                            lines.append(f'ocellus_uncore_count_total{{{labels},unit="{unit}",slot="{slot}"}} '
                                         f"{int(count)}")
        return "\n".join(lines) + "\n"

    def serve(self):
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, body, kind="application/json"):
                data = (body if isinstance(body, str) else json.dumps(body)).encode()
                self.send_response(status)
                self.send_header("Content-Type", kind)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                with fake.lock:
                    if self.path == "/metrics":
                        return self.reply(200, fake.metrics(), "text/plain")
                    fake.calls.append(("GET", self.path))
                    self.reply(200, fake.summary())

            def do_PUT(self):
                request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                with fake.lock:
                    fake.calls.append(("PUT", self.path))
                    fake.config = dict(request["pmus"])
                    fake.started = {box: time.time() for box in fake.config}
                    self.reply(200, fake.summary())

            def do_DELETE(self):
                with fake.lock:
                    fake.calls.append(("DELETE", self.path))
                    fake.config, fake.started = {}, {}
                    self.reply(200, {})

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return f"http://127.0.0.1:{self.server.server_address[1]}"


def write(root, path, text):
    os.makedirs(os.path.dirname(os.path.join(root, path)), exist_ok=True)
    with open(os.path.join(root, path), "w") as file:
        file.write(text)


def host_files(root):
    cpus = [f"processor\t: {cpu}\nvendor_id\t: GenuineIntel\ncpu family\t: 6\nmodel\t\t: 85\n"
            f"model name\t: Fixture Xeon\nstepping\t: 7\nphysical id\t: {cpu // 2}\n"
            f"flags\t\t: fpu cqm_llc cqm_occup_llc cqm_mbm_total cqm_mbm_local\n" for cpu in range(4)]
    write(root, "proc/cpuinfo", "\n".join(cpus) + "\n")
    write(root, "proc/sys/kernel/osrelease", "6.8.0-fixture\n")
    for package in PACKAGES:
        for zone, name in ((f"intel-rapl:{package}", f"package-{package}"), (f"intel-rapl:{package}:0", "dram")):
            write(root, f"sys/class/powercap/{zone}/name", name + "\n")
            write(root, f"sys/class/powercap/{zone}/energy_uj", "0\n")
            write(root, f"sys/class/powercap/{zone}/max_energy_range_uj", f"{1 << 32}\n")
    write(root, "journal.txt", JOURNAL)
    device = "sys/devices/pci0000:5d/0000:5d:00.0/0000:5e:00.0"
    write(root, f"{device}/nvme/nvme0/fakedisk/size", "0\n")
    os.makedirs(os.path.join(root, "sys/block"))
    os.symlink(os.path.join(root, f"{device}/nvme/nvme0/fakedisk"), os.path.join(root, "sys/block/fakedisk"))
    write(root, "sys/bus/pci/devices/0000:5e:00.0/local_cpulist", "2-3\n")
    write(root, "sys/devices/system/node/node0/cpulist", "0-1\n")
    write(root, "sys/devices/system/node/node1/cpulist", "2-3\n")


class StepZeroTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        host_files(self.root)

    def tearDown(self):
        self.fake.server.shutdown()
        self.fake.server.server_close()
        self.tmp.cleanup()

    def arguments(self, hold=2.5, **fake):
        self.fake = FakeOcellus(self.root, **fake)
        return ["--base-url", self.fake.serve(), "--unit", "step0-fixture-absent.service",
                "--proc-root", self.root, "--sys-root", self.root,
                "--journal-file", os.path.join(self.root, "journal.txt"), "--out", os.path.join(self.root, "out"),
                "--hold-seconds", str(hold), "--idle-seconds", "1.5"]

    def start(self, hold=2.5, **fake):
        argv = [sys.executable, "-I", SCRIPT, "--no-load"] + self.arguments(hold, **fake)
        return subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

    def run_with_loads(self, **fake):
        """Runs the script in this process, with the fake daemon's loads."""
        argv, start_load, handler = sys.argv, step0.start_load, signal.getsignal(signal.SIGTERM)
        sys.argv = [SCRIPT, "--disk", "/dev/fakedisk", "--imc-gib", "1", "--dma-mib", "512"] + self.arguments(**fake)
        step0.start_load = self.fake.start_load
        try:
            code = step0.main()
        finally:
            sys.argv, step0.start_load = argv, start_load
            signal.signal(signal.SIGTERM, handler)
        with open(os.path.join(self.root, "out", "report.json")) as file:
            return code, {c["name"]: c for c in json.load(file)["checks"]}

    def finish(self, process):
        output, _ = process.communicate(timeout=60)
        print(output, end="")
        with open(os.path.join(self.root, "out", "report.json")) as file:
            report = json.load(file)
        return process.returncode, {c["name"]: c for c in report["checks"]}, report

    def test_passes_and_deletes_its_config(self):
        code, checks, report = self.finish(self.start())
        self.assertEqual(code, 0)
        self.assertEqual(report["identity"]["family"], "CLX")
        self.assertEqual(checks["collectors.journal"]["status"], "pass")
        self.assertEqual(checks["bundle.series"]["status"], "pass")
        self.assertGreaterEqual(checks["bundle.precession"]["measured"]["samples"], 3)
        self.assertEqual(checks["rapl.dram.1"]["status"], "pass")
        self.assertTrue(checks["dma"]["expected_skip"])
        self.assertEqual([c for c in self.fake.calls if c[0] != "GET"], [("PUT", "/pmu-config"), ("DELETE", "/pmu-config")])
        self.assertEqual(self.fake.config, {})

    def test_measures_the_loads(self):
        code, checks = self.run_with_loads()
        self.assertEqual(code, 0)
        dma = checks["dma"]["measured"][0]
        self.assertEqual((dma["package"], dma["unit"]), ("1", "1"))
        self.assertAlmostEqual(dma["ratio"], 1.0, delta=0.01)
        self.assertEqual(checks["imc.channels"]["measured"]["silent"], [])
        self.assertAlmostEqual(checks["imc.channels"]["measured"]["total"] / (1 << 30), 1.0, delta=0.01)
        extra = checks["imc.default_write_bytes"]["measured"]["bytes_by_package"]
        self.assertAlmostEqual(extra["0"] / (1 << 30), 1.0, delta=0.05)
        self.assertAlmostEqual(extra["1"] / (1 << 30), 0.0, delta=0.05)

    def test_catches_imc_bytes_without_the_x64(self):
        code, checks = self.run_with_loads(imc_factor=1 / 64)
        self.assertEqual(code, 1)
        self.assertEqual(checks["imc.default_write_bytes"]["status"], "fail")
        self.assertEqual(checks["imc.channels"]["status"], "pass")

    def test_refuses_to_start_under_another_config(self):
        code, checks, _ = self.finish(self.start(config={"cha": {"groups": []}}))
        self.assertEqual(code, 2)
        self.assertEqual(list(checks), ["preflight"])
        self.assertEqual([c for c in self.fake.calls if c[0] != "GET"], [])

    def test_reports_a_dram_unit_bug(self):
        code, checks, _ = self.finish(self.start(dram_factor=2.0))
        self.assertEqual(code, 1)
        self.assertEqual(checks["rapl.dram.0"]["status"], "fail")
        self.assertIn("2^1", checks["rapl.dram.0"]["note"])
        self.assertEqual(checks["rapl.package.0"]["status"], "pass")

    def test_deletes_its_config_when_interrupted(self):
        process = self.start(hold=60)
        deadline = time.time() + 20
        while ("PUT", "/pmu-config") not in self.fake.calls and time.time() < deadline:
            time.sleep(0.1)
        process.send_signal(signal.SIGTERM)
        code, checks, _ = self.finish(process)
        self.assertEqual(code, 1)
        self.assertEqual(checks["aborted"]["note"], "interrupted")
        self.assertEqual(checks["bundle.delete"]["status"], "pass")
        self.assertEqual(self.fake.calls[-2:], [("DELETE", "/pmu-config"), ("GET", "/pmu-config")])
        self.assertEqual(self.fake.config, {})


class AnalysisTest(unittest.TestCase):
    def test_integrate_scales_up_for_a_missed_sample(self):
        samples = [(0, 1, {"0": 10.0}), (1, 2, {"0": 30.0}), (3, 4, {"0": 30.0})]
        totals, coverage = step0.integrate(samples, 1, 4)
        self.assertAlmostEqual(totals["0"], 90.0)
        self.assertAlmostEqual(coverage, 2 / 3)

    def test_unit_factor(self):
        self.assertEqual(step0.unit_factor(2.03), 1)
        self.assertEqual(step0.unit_factor(1 / 64), -6)
        self.assertIsNone(step0.unit_factor(1.01))
        self.assertIsNone(step0.unit_factor(1.5))

    def test_excess_scales_a_temporal_group_to_wall_time(self):
        key = ("cha", "0", "0", "1", "1")
        snap = lambda count, running, end: {"counts": {key: count}, "running": {("cha", "0", "1"): running},
                                            "ends": {"cha": end}}
        idle = (snap(0, 0, 0), snap(100, 10, 30))  # 10 counts per running second
        load = (snap(100, 10, 30), snap(400, 15, 45))  # 60 per running second over 15 s
        self.assertAlmostEqual(step0.excess(idle, load, key), (60 - 10) * 15)

    def test_every_family_has_a_temporal_cha_bundle(self):
        for family in step0.ARCH:
            pmus = step0.bundle(family)["pmus"]
            self.assertEqual(pmus["cha"]["grouping"], "temporal")
            for label, box, group, slot, _, _ in step0.dma_candidates(step0.ARCH[family]):
                self.assertIsNotNone(pmus[box]["groups"][int(group)]["events"][int(slot)], label)


if __name__ == "__main__":
    unittest.main(verbosity=2)
