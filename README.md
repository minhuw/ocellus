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

On Skylake-SP and Cascade Lake, daemon mode lets a client set uncore event
groups at runtime. While a box type (`cha`, `iio`, `irp`, `imc`) has a config,
its collector counts the config's groups instead of its default ones, and
`/metrics` exports raw counts cumulative since the config was set in place of
the box type's default `ocellus_<box type>_*` metrics.

```sh
curl -X PUT localhost:8080/pmu-config -H 'Content-Type: application/json' -d '{
  "pmus": {"cha": {"groups": [{
    "filter1": {"opcodes": ["ItoM"]},
    "events": [{"alias": "tor_inserts", "source": "io", "result": "miss"}]
  }]}}
}'
```

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
- `ocellus_uncore_event_info{config_id,box_type,package,group,slot,units,alias,event,umask,...} 1`:
  what each counter counts, with `filter0`/`filter1` for CHA and
  `ch_mask`/`fc_mask` for IIO.

A delta of a box type's counts is valid only between two samples with equal
`ocellus_uncore_config_id` and equal
`ocellus_uncore_config_start_timestamp_seconds`: setting a config, even the
same one again, restarts its counts from 0. Rates are Δcount / Δrunning
seconds of the counter's group.

Collectors sample from one whole multiple of the measure interval (in Unix
time) to the next, so the samples of all box types cover the same intervals
to within a few milliseconds.

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
