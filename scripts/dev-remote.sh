#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <ssh-host>" >&2
  exit 2
fi

remote="$1"
remote_dir="${OCELLUS_REMOTE_DIR:-~/projects/ocellus}"
forward_bind="${OCELLUS_FORWARD_BIND:-0.0.0.0}"
local_port="${OCELLUS_LOCAL_PORT:-8080}"
remote_port="${OCELLUS_REMOTE_PORT:-8080}"
sync_interval="${OCELLUS_SYNC_INTERVAL_SECONDS:-2}"

ensure_remote_dir() {
  ssh "${remote}" "mkdir -p ${remote_dir}"
}

sync_once() {
  rsync -az --delete \
    --exclude '.git/' \
    --exclude 'target/' \
    --exclude '.cargo/target/' \
    --exclude '.direnv/' \
    --exclude 'demo/grafana/data/' \
    ./ "${remote}:${remote_dir}/"
}

watch_sync() {
  ensure_remote_dir
  while true; do
    sync_once
    sleep "${sync_interval}"
  done
}

forward_metrics() {
  while true; do
    ssh -N \
      -o ExitOnForwardFailure=yes \
      -o ServerAliveInterval=15 \
      -o ServerAliveCountMax=3 \
      -L "${forward_bind}:${local_port}:127.0.0.1:${remote_port}" \
      "${remote}" || true
    sleep 1
  done
}

ensure_remote_dir
sync_once
watch_sync &
sync_pid="$!"
forward_metrics &
forward_pid="$!"

trap 'kill "${sync_pid}" "${forward_pid}" 2>/dev/null || true' INT TERM EXIT
wait -n "${sync_pid}" "${forward_pid}" || true
