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

The demo Grafana dashboards are published from `demo/grafana/dashboards` to `https://ocellus.minhuw.dev`.

- Catalog: `https://ocellus.minhuw.dev/`
- Manifest: `https://ocellus.minhuw.dev/dashboards/index.json`
- Versioned release manifest: `https://ocellus.minhuw.dev/dashboards/v0.12.0/index.json`

Existing Grafana installations can use the manifest with file provisioning:

```sh
npx --yes --package=github:minhuw/ocellus ocellus-sync-dashboards \
  --manifest https://ocellus.minhuw.dev/dashboards/index.json \
  --output /var/lib/grafana/dashboards/ocellus
```

Add `--interval-seconds 300` to keep syncing dashboards in a long-running process.
Use a versioned manifest URL to pin dashboards to a tagged Ocellus release.
