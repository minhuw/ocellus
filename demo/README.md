# ocellus demo

This demo runs Prometheus and Grafana in Docker Compose and scrapes an `ocellus`
daemon running on the host.

Start `ocellus`:

```sh
sudo modprobe msr
cargo build
sudo -E ./target/debug/ocellus --daemon --listen 0.0.0.0:8080 --measure-interval-ms 1000
```

Start the dashboard stack:

```sh
cd demo
docker compose up
```

Open:

- Grafana: <http://localhost:3000>
- Prometheus: <http://localhost:9090>

Grafana defaults to `admin` / `admin`.
