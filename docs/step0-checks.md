# Step 0: smoke check of Ocellus on a new host

`scripts/step0_smoke.py` (Python 3 standard library only) checks, on a
host's first allocation, that the running daemon sees the host, that every
collector of the CPU family samples every package, that the family's PMU
configs program and count, and that IMC, RAPL and DMA counts match known
volumes.

## Running it

As root on the host, with the daemon (Rhea's unit `rhea-telemetry-ocellus`)
on `127.0.0.1:8080`:

```sh
python3 -I scripts/step0_smoke.py --out /tmp/step0-$(hostname)
```

It writes `report.json` (per check: `name`, `status` pass/fail/skip,
`measured`, `expected`, `note`, and `expected_skip` for skips) and
`summary.txt` to `--out` (default `/tmp/ocellus-step0-<host>-<UTC time>`).
It exits 0 if every check passed or was an expected skip, 1 otherwise, and 2
if it refused to start: it starts only if `GET /pmu-config` is `{}`. On the
host it only sets PMU configs and runs two bounded `dd` loads, about a
minute in all. It deletes every config it set, also on an error, Ctrl-C or
SIGTERM; `bundle.delete` records the DELETE and the `GET` after it.

`--base-url`, `--unit`, `--proc-root`, `--sys-root`, `--journal-file`
(instead of `journalctl`) and `--no-load` make it testable;
`python3 -I scripts/test_step0_smoke.py` runs it against a fake daemon and
fake `/proc` and `/sys`. `--hold-seconds` (30), `--idle-seconds` (10),
`--imc-gib` (8), `--dma-mib` (4096) and `--disk` (default: the largest
non-removable disk in `lsblk`, NVMe first) size the checks.

## The checks

1. **`identity`**: model, stepping and sockets from `/proc/cpuinfo`, the
   kernel, `ocellus_collector_info`, `processor_info`, `ocellus_info`, and
   the unit's MainPID and binary. 0x2D SNB, 0x3E IVB, 0x3F HSX, 0x4F BDX,
   0x55 SKX (CLX from stepping 5), 0x6A ICX, 0x8F SPR, 0xCF EMR; on any other
   model nothing else runs. It fails if `processor_info` names another model.
2. **`collectors.*`**: each collector of the family gains samples over two
   intervals, is not stopped and exports every package; the SNB HA, which has
   no default group, exports no series. RDT: L3 occupancy
   everywhere; MBM on BDX and later exactly when `/proc/cpuinfo` has
   `cqm_mbm_total` or `cqm_mbm_local`, never on HSX. `collectors.journal`
   lists the `skipping ... collector` lines of the unit's current
   invocation and passes if each is expected (below).
3. **`bundle.*`**: the family's bundle (below) is PUT, all box types with
   `"aggregate": "box"`, and held 30 s. `bundle.put` and `bundle.event_info`
   record the request, the `config_id`s and the `ocellus_uncore_event_info`
   labels. `bundle.series`: every series of every package and unit that
   event_info names exists and never decreases, the config ids stay, clock
   series tick, and every group's running seconds are > 0.
   `bundle.precession`: in every CHA sample
   `ocellus_uncore_schedule_first_group{box_type="cha"}` is the cycle (window
   end / interval) mod the group count.
4. **`imc.default_write_bytes`**: `dd if=/dev/zero of=/dev/null bs=1G count=8
   iflag=fullblock`, pinned to the last CPU of package 0, under the default
   groups: package 0's `*imc_write_bytes_per_second` integrated over the
   load, minus its level over the 10 s before. Each read zero-fills dd's
   1 GiB buffer, larger than any LLC (EMR: 320 MB), so nearly every line
   reaches DRAM, and writing to /dev/null touches no memory: DRAM writes are
   the bytes dd copied, less at most one LLC, plus up to 1 GiB of page
   zeroing on first touch. **`imc.channels`** runs the load again under the
   bundle: `cas_count wr` x 64 summed over package 0's channels must be in
   the band, and no channel may stay below 5% of an even share (with
   sub-NUMA clustering, only the load node's channels must). A silent
   channel is dead or unpopulated; see `dmidecode -t memory`.
5. **`rapl.<domain>.<package>`**: Ocellus's package and DRAM energy over the
   idle window and the load against `/sys/class/powercap/intel-rapl:*`,
   read at each new RAPL sample and scaled to the samples' span, within 5%.
   A ratio within 5% of 2^n, n != 0, is reported as a unit bug.
6. **`dma`**: `dd if=<disk> of=/dev/null bs=1M count=4096 iflag=direct` under
   the bundle. O_DIRECT skips the page cache, so the device DMA-writes each
   byte once, in full lines, into dd's buffer. SKX and later: `data_req_of_cpu
   mem_write` (4 B units, as in Intel's IO bandwidth metrics) of the stack
   with the largest delta, which must be on the disk's package (from its PCI
   path's `local_cpulist`). SNB to BDX: CBo PCIe-write TOR inserts x 64,
   with and without the IO thread id, and on HSX/BDX IRP `coherent_ops
   pcitom` per set and summed; any of them in the band on the disk's package
   passes, and the others show which opcode, TID and IRP split the host
   uses. Counts are net of their rate during the hold, per running second.
7. **Family notes**: the open hardware questions below, untested, in the
   report and summary.

The band of checks 4 and 6 is 0.5-2x: it catches a missing x64, a wrong
package or a dead channel, not a calibration error.

## Bundles

One temporal CHA/CBo config, fixed configs for the rest; every group passes
the validators of `src/pmu_config/spec`.

| Family | CHA/CBo groups | Other box types |
| --- | --- | --- |
| SNB, IVB | PCIItoM, PCIRdCur: `tor_occupancy`, `tor_inserts` (kind opcode, tid 0x1e); LLC lookup (SNB `data_read`, IVB `any`) on I,S,E,M,F; PCIItoM without tid | `imc` cas_count rd, wr, DCLK |
| HSX, BDX | ItoM, PCIRdCur: occupancy and inserts with tid 0x3e, inserts without; PCIItoM occupancy and inserts, LLC lookup `any` | `imc` as SNB; `ha` clockticks, snoop_resp rspi, rsps, rspifwd; `irp` clockticks, coherent_ops pcitom, both sets |
| SKX, CLX | ItoM, RdCur: io occupancy, inserts all and miss; LLC lookup `any`, all states | `imc` as SNB; `iio` data_req_of_cpu mem_write, mem_read, clockticks; `irp` clockticks, coherent_ops pcitom |
| ICX | opcode ITOM, PCIRDCUR: as SKX; LLC lookup `data_read` | `imc` cas_count rd, wr, clockticks, fixed; `iio`, `irp` as SKX |
| SPR, EMR | as ICX without LLC lookup (experimental only) | `imc` as ICX; `iio` as SKX; `irp` clockticks, faf_inserts |

The PCIe-write opcodes follow Intel PCM (`src/pcm-pcie.h`): PCIItoM (0x19c)
on Jaketown and Ivytown, where PCM also adds PCIWiLF and the NS writes; ItoM
(0x1c8) on Haswell-EP and Broadwell-EP, where it adds RFO. Whether BDX uses
ItoM or PCIItoM is BDX item 2.

## Expected skips

| Family | Collectors it lacks (journal skips) |
| --- | --- |
| SNB | IIO, IRP, RDT |
| IVB | IIO, RDT |
| HSX, BDX | IIO (HSX also has no MBM) |
| SKX, CLX, ICX, SPR, EMR | HA |

Any family may skip interconnect where no links are found and any
`rapl.*` domain powercap lacks; `--no-load` skips `dma`, `imc.channels` and
`imc.default_write_bytes`. Ocellus has an SNB IRP collector
(`src/metrics/irp/snb.rs`, device 0x3c40, no source cited): if it samples,
`collectors.irp` fails until that device is explained (card c59).

## Open questions per family

- **HSX, BDX**: [`docs/bdx-step0-checks.md`](bdx-step0-checks.md), in
  particular item 10 (IRP counter offsets and the IRP0/IRP1 split, card c23).
- **SNB, IVB**: card c55 (PCIe TID 0x1e on IVB, tid with opc and opc with
  nid, SNB iMC rst bits, IVB box ctl bits 17:16, SNB IMC package mapping by
  bus order, IVB LLC_LOOKUP state bit 23, which only the perfmon JSON has).
  Neither has a public XSNP or snoop-response event.
- **SKX, CLX**: per-CPU MBM needs an IMC or CHA cross-check (README, RDT).
- **ICX**: the default CHA groups count IA ITOM and IA/IO WbMtoI, which
  Intel lists only as experimental on ICX.
- **SPR, EMR**: card c54 (default events with experimental-only or no public
  encodings).
