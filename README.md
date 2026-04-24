# ocellus

`ocellus` is a minimal Rust skeleton for a hardware telemetry exporter inspired by `peacock`.

The intended direction is to expose Intel PMU, uncore, RDT, and RAPL metrics through a small Prometheus-compatible service.

## Development

```sh
nix develop
cargo fmt
cargo test
cargo run -- --help
```

## Build

```sh
nix build
nix run
```
