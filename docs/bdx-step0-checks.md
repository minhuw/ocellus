# Broadwell-EP step 0: checks before trusting BDX PMU configs

The Broadwell-EP (and Haswell-EP) encodings in `src/pmu_config/spec/hsx` come
from Intel's Xeon E5 v4 uncore guide 334291 (V4), `broadwellx_uncore.json`
and the Linux `hswep` CBo driver. The items below rest on inference or on
sources that disagree, so the first run on a BDX host settles them before any
other BDX measurement is interpreted. Each item names the cell, what to read,
and the acceptance rule.

Cells use the DDIO recipe's names: S_LLC (A pinned), S_L2M (B pinned),
S_L2S (D pinned, next lifecycle), the two-sharer cell (two-core handoff), and
a stale core-valid cell. L is the DMA lines per packet; line 0 stays M in the
NF core's L2 at reuse, so every cell has about one core snoop per packet from
it.

1. **IIO thread ID.** V4's note says PCIe requests carry TID 0x1E, its derived
   events use 0x3F, and the HSX guide, PCM and Linux perf use 0x3E. Count
   TOR_INSERTS.OPCODE ItoM (0x1C8) in S_mem with `tid_en` off, then with TID
   0x3E, 0x1E and 0x3F, and once more with a core `rep stosb` loop and no
   traffic. Accept the TID whose count equals the `tid_en`-off count under
   NIC traffic and reads about 0 for the core loop.
2. **ItoM vs PCIItoM.** Count TOR_INSERTS.OPCODE for ItoM 0x1C8 (with the
   accepted TID) and PCIItoM 0x19C side by side. Accept the opcode that counts
   about L_full per packet; the other must read about 0.
3. **IO lookups in LLC_LOOKUP.ANY.** V4 says ANY covers the IRQ and IPQ, and
   PCIe requests may enter through the PRQ. In S_LLC, LLC_LOOKUP.ANY at state
   M must reach about 1 per DMA line; if it stays near the idle level, IO
   lookups need another umask.
4. **Core-snoop proxies.** TxR_INSERTS.IV_CACHE (0x02/0x08), AK_CORE
   (0x02/0x20), BL_CORE (0x02/0x40) and MISC.RSPI_WAS_FSE (0x39/0x01), summed
   over all CBos and also per CBo (`aggregate: box`) to see which box counts
   the `*_CORE` subevents.
   - S_L2M: IV_CACHE and AK_CORE about 1 per DMA line, BL_CORE about 1 per
     line, RSPI_WAS_FSE about 0.
   - S_L2S: IV_CACHE and AK_CORE about 1 per line, RSPI_WAS_FSE about 1,
     BL_CORE about 0.
   - S_LLC: each proxy about 1 per packet (1/L per ItoM: 0.043 at L = 23),
     from line 0. A reading of 0 means the proxy misses snoops.
   - BL_CORE must count once per 64 B line, not twice.
5. **Two sharers.** In the two-core handoff cell the line has two core-valid
   bits, so an IO ItoM snoops two cores. Record IV_CACHE, AK_CORE and
   RSPI_WAS_FSE per ItoM: whether they count per transaction (about 1) or per
   snooped core (about 2) decides the BDX L2(S) signature.
6. **Stale core-valid bits.** BDX has no clean-writeback opcode, so a clean
   L2 drop leaves the core-valid bit set. In the stale-CV cell, IV_CACHE
   should be about 1 per line with BL_CORE and RSPI_WAS_FSE about 0; record
   MISC.CVZERO_PREFETCH_VICTIM/MISS (0x39/0x10, 0x20) next to it.
7. **Shared CBo filters.** V4 allows one filtered event per box; the kernel
   allows events whose filter fields agree. Run one group with
   TOR_OCCUPANCY.OPCODE, TOR_INSERTS.OPCODE and TOR_INSERTS.MISS_OPCODE on the
   same opcode, and compare each with the same event alone in its own group.
   Accept sharing if they agree within 1%.
8. **TID plus opcode.** The HSX guide says one filter criterion at a time;
   Ocellus, PCM and Linux perf combine TID and opcode. Item 1 covers it: the
   TID-filtered ItoM count must equal the unfiltered count under NIC traffic.
9. **BIOS snoop mode.** Record it (early snoop, home snoop, or directory with
   OSB) before reading the two-socket miss signature, then check that in S_mem
   HA REQUESTS.INVITOE_LOCAL (0x01/0x10) and HA SNOOP_RESP.RSPI (0x21/0x01)
   are about 1 per IO miss, and whether CAS RD rises by 1 per miss
   (a directory read).
10. **IRP counters.** Ocellus reads the HSX/BDX IRP counters at a0/a8/b0/b8,
    as Linux has since commit 41a134a5830a; Intel's guides list a0/b0/b8/c0.
    With Ocellus stopped, put clockticks (event 0x00) on all four IRP
    counters through perf: four equal counts confirm the offsets. Then put
    one DMA traffic event (0x13/0x10 PCIItoM or 0x16/0x02 inbound writes) on
    all four counters under NIC receive: equal counts mean one IRP set and
    the current four-counter use is right; two different pairs mean counters
    0-1 and 2-3 belong to separate sets, which the collector must sum.
11. **IMC channel discovery.** Ocellus pairs each IMC channel's PCI address
    with a device id (D20:F0 = 0x6FB0, as in the kernel's table); the encoding
    report read 0x6FB4 for D20:F0 from V4. Check that the IMC collector starts
    and lists every populated channel, and that ocellus_uncore_count_total
    for box_type="imc" uses the expected channel ids (index in the channel
    table, D20:F0 = 0 to D24:F1 = 7).
