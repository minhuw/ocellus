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
