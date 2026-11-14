# ocellus

`ocellus` is a Rust hardware telemetry exporter for Intel server processors.

## Usage

`ocellus` currently reads x86 MSRs, so load the `msr` kernel module and run with root or `CAP_SYS_RAWIO`.

Local JSONL mode is the default:

```sh
sudo modprobe msr
sudo cargo run -- --output ocellus-metrics.jsonl --measure-interval-ms 1000
```

Prometheus daemon mode:

```sh
sudo modprobe msr
sudo cargo run -- --daemon --listen 0.0.0.0:8080 --measure-interval-ms 1000
```

Run directly from the GitHub flake:

```sh
sudo modprobe msr
sudo nix run github:minhuw/ocellus -- --daemon --listen 0.0.0.0:8080 --measure-interval-ms 1000
```

Alternatively, build with Nix first and run the built binary with elevated
privileges:

```sh
nix build github:minhuw/ocellus
sudo ./result/bin/ocellus --daemon --listen 0.0.0.0:8080 --measure-interval-ms 1000
```

## PMU configs

On Skylake-SP, Cascade Lake, Haswell-EP and Broadwell-EP, daemon mode lets a
client set uncore event groups at runtime. While a box type (`cha`, `iio`,
`irp`, `imc`, `ha`) has a config, its collector counts the config's groups
instead of its default ones, and `/metrics` exports raw counts cumulative since
the config was set in place of the box type's default `ocellus_<box type>_*`
(Haswell-EP/Broadwell-EP: `ocellus_haswell_<box type>_*`) metrics.

```sh
curl -X PUT localhost:8080/pmu-config -H 'Content-Type: application/json' -d '{
  "pmus": {"cha": {"groups": [{
    "filter1": {"opcodes": ["ItoM"]},
    "events": [{"alias": "tor_inserts", "source": "io", "result": "miss"}]
  }]}}
}'
```

Haswell-EP and Broadwell-EP take configs for `cha` (the CBo boxes), `imc`
and `ha` (the home agents). Their TOR events filter by `kind` and `home`
rather than by source: requests from IO are told apart by thread id, which a
group sets in `filter0.tid` and which applies only to the events that set
`"tid_en": true`. Several of these encodings come from Intel sources that
disagree, so run the checks in `docs/bdx-step0-checks.md` on a new
Broadwell-EP host before interpreting its counts.

- `PUT /pmu-config` sets the configs of the box types it names and returns
  every other box type to its default groups. Once every affected collector
  has switched it replies `200` with
  `{"cha": {"config_id": N, "start_timestamp_seconds": T}}` for the named box
  types. An invalid request gets `400`, a named box type without a running
  collector `409`, and a collector that fails to switch `500`; errors are
  `{"error": "..."}`, and a `500` names the box types that already switched.
- `DELETE /pmu-config` returns every box type to its default groups: `200 {}`.
- `GET /pmu-config` returns the configs in the shape of the `PUT` reply.

A config exports, from its first sample on:

- `ocellus_uncore_count_total{box_type,package,unit,group,slot}`: the raw
  count; `unit` is the box id, or `sum` for `"aggregate": "sum"`.
- `ocellus_uncore_running_seconds_total{box_type,package,group}`: how long the
  group has counted.
- `ocellus_uncore_config_id{box_type}` and
  `ocellus_uncore_config_start_timestamp_seconds{box_type}`.
- `ocellus_uncore_schedule_first_group{box_type}`, for temporal configs: the
  group the latest sample started with (see below).
- `ocellus_uncore_event_info{config_id,box_type,package,group,slot,units,alias,event,umask,...} 1`:
  what each counter counts, with `filter0`/`filter1` for CHA,
  `ch_mask`/`fc_mask` for IIO, and `tid_en="1"` on Haswell-EP/Broadwell-EP
  CBo events that set it.

A delta of a box type's counts is valid only between two samples with equal
`ocellus_uncore_config_id` and equal
`ocellus_uncore_config_start_timestamp_seconds`: setting a config, even the
same one again, restarts its counts from 0. Rates are Δcount / Δrunning
seconds of the counter's group.

Collectors sample from one whole multiple of the measure interval (in Unix
time) to the next, so the samples of all box types cover the same intervals
to within a few milliseconds.

Temporal groups take turns within each sample, in group order from a first
group that is the same for every collector. Sample c, the one that ends c
measure intervals after the Unix epoch (its Unix second at a 1 s interval),
lies in block k = floor(c / n), where n is the box type's temporal group
count, and starts with group (c + r_k) mod n, where r_k = SplitMix64(k) mod n
(the SplitMix64 finalizer on the u64 k). Within a block every group counts at
every slice position once; across blocks the position of a fixed instant
moves pseudo-randomly, so a source whose period is a multiple of n does not
stay in one group's slice. `ocellus_uncore_schedule_first_group{box_type}` is
the group in slice 0 of a temporal config's latest sample: slice p of each
round ran group (first + p) mod n.

Rates are unbiased only over aligned windows. A window must start at a sample
whose cycle c ≡ 0 mod L_n and cover whole blocks of L_n samples, where L_n is
the lcm of the temporal group counts in the bundle (fixed and spatial configs
count as 1). Any n consecutive samples no longer balance the groups: an
unaligned window covers some group-slot pairs more often than others, so a
burst of B counts at a fixed phase of each sample biases a group's rate by
up to ±B/m per second over a window of m blocks of n 1 s samples. No randomized
schedule can keep every sliding window balanced: if both of two windows of n
samples that overlap by n - 1 are balanced, the sample that enters repeats
the one that leaves, so the schedule repeats every n samples, the fixed
rotation that a source with a period of a multiple of n locks to. Single
samples still swing where a group meets a source locked to a phase of the
interval.

## RDT memory bandwidth on Skylake-SP and Cascade Lake

`ocellus_rdt_memory_bandwidth_bytes_per_second` exports resctrl's MBM counts
unchanged (106,496 B per count on the host below). On a 2-socket Xeon Gold 6230R
(Linux 6.8), some runs report phantom bandwidth on many CPUs at once: bursts
of up to about 360 MB/s per CPU, all counted toward node 0 (package-0 `local`,
package-1 `remote`), that neither the IMC CAS counters nor the CHA home-request
counters see. Other runs showed none for 40 hours. This matches the RMID-dependent
MBM inaccuracy Intel documents for these parts (specification update erratum
SKX99; Cascade Lake RDT reference manual 338822, sections 3.2 and 3.6).

Treat per-CPU MBM on Skylake-SP and Cascade Lake as valid only alongside an
IMC or CHA cross-check. One detection method, per CPU series over windows of
about 16 minutes:

1. A burst second is a sample of at least 50 MB/s.
2. A window with at least 10 burst seconds in at least 3 bursts is a
   candidate.
3. Regress the destination node's IMC read plus write bandwidth (the CPU's
   own package for `local`, the other package for `remote`) on the series
   over the window. Real traffic shows up in the IMC; a slope below 0.5 even
   at three standard errors above the estimate marks the window as phantom.

## Releases

Tagged releases publish a statically linked Linux x86_64 binary:

```sh
curl -LO https://github.com/minhuw/ocellus/releases/download/v0.1.0/ocellus
curl -LO https://github.com/minhuw/ocellus/releases/download/v0.1.0/ocellus.sha256
sha256sum -c ocellus.sha256
chmod +x ocellus
```

Each release also includes versioned aliases such as `ocellus-v0.1.0` and
`ocellus-v0.1.0.sha256`.

## Dashboards

The demo Grafana dashboards are maintained as Grafana v2 resources in
`demo/grafana/dashboards` and published to `https://ocellus.minhuw.dev`.
The website also generates classic dashboard JSON for Grafana's import dialog.

- Catalog: `https://ocellus.minhuw.dev/`
- Manifest: `https://ocellus.minhuw.dev/dashboards/index.json`
- Versioned release manifest: `https://ocellus.minhuw.dev/dashboards/v0.12.0/index.json`

Existing Grafana installations can use the manifest with file provisioning.
The default sync format is the v2 resource JSON:

```sh
npx --yes --package=github:minhuw/ocellus ocellus-sync-dashboards \
  --manifest https://ocellus.minhuw.dev/dashboards/index.json \
  --output /var/lib/grafana/dashboards/ocellus
```

Add `--interval-seconds 300` to keep syncing dashboards in a long-running process.
Use a versioned manifest URL to pin dashboards to a tagged Ocellus release.
Use `--format classic` if your Grafana provisioning flow expects classic
dashboard JSON. Classic sync rewrites import placeholders to the datasource UID
`Prometheus`; pass `--datasource-uid UID` if your Grafana datasource uses a
different UID.

The sync CLI is also available from Nix:

```sh
nix run github:minhuw/ocellus#dashboard-sync -- \
  --output /var/lib/grafana/dashboards/ocellus \
  --datasource-name VictoriaMetrics \
  --datasource-uid victoria \
  --prune
```
