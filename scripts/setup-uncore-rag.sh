#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
usage: scripts/setup-uncore-rag.sh [options]

Rebuild the local untracked Ocellus uncore RAG corpus under .resources.

Options:
  --resources-dir DIR       Write resources to DIR (default: .resources)
  --python BIN              Python 3.11-3.13 binary for the Docmancer venv
                             (default: $PYTHON or python3)
  --skip-downloads          Rebuild corpus/index from already-downloaded sources
  --skip-docmancer          Download/build corpus only; do not install or ingest
  --install-codex-skill     Install/update the Codex Docmancer skill for this index
  --clean                   Remove the selected resources dir before rebuilding
  -h, --help                Show this help

Example:
  scripts/setup-uncore-rag.sh

Nix one-shot example:
  nix shell nixpkgs#git nixpkgs#curl nixpkgs#unzip nixpkgs#python312 -c \
    scripts/setup-uncore-rag.sh

Query after setup:
  .resources/docmancer/venv/bin/docmancer \
    --config .resources/docmancer/docmancer.yaml \
    query 'CAPID6 CAPID7 CHA count Skylake Xeon'
EOF
}

log() {
  printf '[ocellus-rag] %s\n' "$*" >&2
}

die() {
  printf '[ocellus-rag] error: %s\n' "$*" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
repo_root="$(cd -- "${script_dir}/.." && pwd -P)"
resources_dir="${OCELLUS_RAG_RESOURCES_DIR:-${repo_root}/.resources}"
python_bin="${PYTHON:-python3}"
skip_downloads=0
skip_docmancer=0
install_codex_skill=0
clean=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --resources-dir)
      [[ $# -ge 2 ]] || die "--resources-dir requires a value"
      resources_dir="$2"
      shift 2
      ;;
    --python)
      [[ $# -ge 2 ]] || die "--python requires a value"
      python_bin="$2"
      shift 2
      ;;
    --skip-downloads)
      skip_downloads=1
      shift
      ;;
    --skip-docmancer)
      skip_docmancer=1
      shift
      ;;
    --install-codex-skill)
      install_codex_skill=1
      shift
      ;;
    --clean)
      clean=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

need git
need curl
need unzip
need cp
need find
need mkdir
need rm
need "$python_bin"

if [[ "${resources_dir}" != /* ]]; then
  resources_dir="${repo_root}/${resources_dir}"
fi
resources_dir="$(mkdir -p -- "${resources_dir}" && cd -- "${resources_dir}" && pwd -P)"

if [[ "${clean}" -eq 1 ]]; then
  case "${resources_dir}" in
    "${repo_root}/.resources"| "${repo_root}/.resources/"*)
      log "removing ${resources_dir}"
      rm -rf -- "${resources_dir}"
      mkdir -p -- "${resources_dir}"
      ;;
    *)
      die "--clean refuses to remove non-repo resources dir: ${resources_dir}"
      ;;
  esac
fi

ensure_git_exclude() {
  [[ -d "${repo_root}/.git" ]] || return 0
  [[ "${resources_dir}" == "${repo_root}/"* ]] || return 0

  local rel
  rel="${resources_dir#${repo_root}/}"
  rel="${rel%/}/"
  if git -C "${repo_root}" check-ignore -q "${rel}"; then
    return 0
  fi

  touch "${repo_root}/.git/info/exclude"
  if ! grep -Fxq "${rel}" "${repo_root}/.git/info/exclude"; then
    printf '\n%s\n' "${rel}" >> "${repo_root}/.git/info/exclude"
    log "added ${rel} to .git/info/exclude"
  fi
}

download() {
  local url="$1"
  local out="$2"
  if [[ -s "${out}" ]]; then
    log "using existing ${out#${repo_root}/}"
    return 0
  fi

  mkdir -p -- "$(dirname -- "${out}")"
  local tmp="${out}.tmp.$$"
  log "downloading ${url}"
  curl -fL --retry 3 --retry-delay 2 --connect-timeout 20 --max-time 300 \
    "${url}" -o "${tmp}"
  mv -- "${tmp}" "${out}"
}

copy_if_exists() {
  local src="$1"
  local dst="$2"
  if [[ -f "${src}" ]]; then
    mkdir -p -- "$(dirname -- "${dst}")"
    cp -- "${src}" "${dst}"
  else
    log "warning: missing ${src#${repo_root}/}"
  fi
}

check_python() {
  "${python_bin}" - <<'PY'
import sys
version = sys.version_info[:2]
if not ((3, 11) <= version < (3, 14)):
    raise SystemExit(f"Docmancer requires Python 3.11-3.13, got {sys.version.split()[0]}")
PY
}

ensure_git_exclude

intel_dir="${resources_dir}/intel"
perfmon_dir="${intel_dir}/perfmon/intel-perfmon"
docs_dir="${intel_dir}/docs"
linux_dir="${resources_dir}/linux/linux-perf"
docmancer_dir="${resources_dir}/docmancer"
corpus_dir="${docmancer_dir}/corpus"

mkdir -p -- "${docs_dir}" "${docmancer_dir}"

if [[ "${skip_downloads}" -eq 0 ]]; then
  if [[ ! -d "${perfmon_dir}/.git" ]]; then
    mkdir -p -- "$(dirname -- "${perfmon_dir}")"
    git clone --depth=1 --filter=blob:none \
      https://github.com/intel/perfmon.git "${perfmon_dir}"
  else
    log "using existing ${perfmon_dir#${repo_root}/}"
  fi

  if [[ ! -d "${linux_dir}/.git" ]]; then
    mkdir -p -- "$(dirname -- "${linux_dir}")"
    git clone --depth=1 --filter=blob:none --sparse \
      https://github.com/torvalds/linux.git "${linux_dir}"
  else
    log "using existing ${linux_dir#${repo_root}/}"
  fi
  git -C "${linux_dir}" sparse-checkout set --skip-checks \
    arch/x86/events/intel \
    tools/perf/Documentation \
    tools/perf/pmu-events/arch/x86 \
    Documentation/admin-guide \
    Documentation/ABI/testing

  download \
    'https://www.intel.co.jp/content/dam/www/public/us/en/documents/design-guides/xeon-e5-2600-uncore-guide.pdf' \
    "${docs_dir}/327043-intel-xeon-e5-2600-uncore-guide.pdf"
  download \
    'https://cdrdv2.intel.com/v1/dl/getContent/671290' \
    "${docs_dir}/329468-intel-xeon-processor-e5-2600-v2-uncore.pdf"
  download \
    'https://cdrdv2.intel.com/v1/dl/getContent/671052' \
    "${docs_dir}/331051-intel-xeon-processor-e5-e7-v3-uncore.pdf"
  download \
    'https://cdrdv2.intel.com/v1/dl/getContent/671389' \
    "${docs_dir}/336274-intel-xeon-processor-scalable-memory-family-uncore.pdf"
  download \
    'https://cdrdv2.intel.com/v1/dl/getContent/639778' \
    "${docs_dir}/icx-639778.zip"
  download \
    'https://cdrdv2.intel.com/v1/dl/getContent/642245' \
    "${docs_dir}/spr-642245.zip"
  download \
    'https://cdrdv2.intel.com/v1/dl/getContent/817509?fileName=817509-EMR_XCC_UPG_Guide-Rev_001.pdf' \
    "${docs_dir}/817509-emr-xcc-upg-guide-rev-001.pdf"
  download \
    'https://cdrdv2.intel.com/v1/dl/getContent/671098' \
    "${docs_dir}/671098-intel-sdm-volume-4-msrs.pdf"

  mkdir -p -- "${docs_dir}/icx-639778" "${docs_dir}/spr-642245"
  unzip -oq "${docs_dir}/icx-639778.zip" -d "${docs_dir}/icx-639778"
  unzip -oq "${docs_dir}/spr-642245.zip" -d "${docs_dir}/spr-642245"
fi

log "building focused corpus"
rm -rf -- "${corpus_dir}"
mkdir -p -- \
  "${corpus_dir}/intel-docs" \
  "${corpus_dir}/intel-perfmon" \
  "${corpus_dir}/linux-uncore" \
  "${corpus_dir}/text-shadows/intel-perfmon" \
  "${corpus_dir}/text-shadows/linux-pmu-events" \
  "${corpus_dir}/text-shadows/linux-uncore"

cat > "${corpus_dir}/README.md" <<'EOF'
# Ocellus Uncore RAG Corpus

This corpus is a local, untracked research index for Ocellus Intel uncore PMU work.

Trust order for event claims:

1. Intel uncore programming/reference manuals and SDM material.
2. Linux kernel uncore PMU driver code and sysfs ABI documentation.
3. Intel perfmon public JSON event files.
4. Intel perfmon experimental JSON event files.
5. Local machine validation with `perf`, MSR reads, or Ocellus exporter output.

Treat `*_uncore_experimental.json` entries as hints, not as sufficient evidence for enabling exporter metrics or dashboards. If an event appears only in experimental perfmon JSON, verify it against a manual, Linux support, or hardware before relying on it.

The original source trees are kept under `.resources/intel` and `.resources/linux`. Event JSON tables are normalized into per-event Markdown under `normalized-events/`. Text-shadow files under `text-shadows/` exist only because Docmancer indexes `.txt` directly, while CSV, C, header, and extensionless ABI files are easier to retrieve after copying them with a `.txt` suffix.
EOF

perfmon_files=(
  mapfile.csv
  SNB/events/sandybridge_uncore.json
  IVT/events/ivytown_uncore.json
  HSX/events/haswellx_uncore.json
  BDX/events/broadwellx_uncore.json
  BDW-DE/events/broadwellde_uncore.json
  SKX/events/skylakex_uncore.json
  SKX/events/skylakex_uncore_experimental.json
  CLX/events/cascadelakex_uncore.json
  CLX/events/cascadelakex_uncore_experimental.json
  ICX/events/icelakex_uncore.json
  ICX/events/icelakex_uncore_experimental.json
  SPR/events/sapphirerapids_uncore.json
  SPR/events/sapphirerapids_uncore_experimental.json
  EMR/events/emeraldrapids_uncore.json
  EMR/events/emeraldrapids_uncore_experimental.json
)

for rel in "${perfmon_files[@]}"; do
  copy_if_exists "${perfmon_dir}/${rel}" "${corpus_dir}/intel-perfmon/${rel}"
done

shopt -s nullglob
for src in "${docs_dir}"/*.pdf; do
  cp -- "${src}" "${corpus_dir}/intel-docs/"
done
for src in "${docs_dir}"/icx-639778/*/*.pdf \
           "${docs_dir}"/icx-639778/*/*_uc_*.txt \
           "${docs_dir}"/spr-642245/*/*.pdf \
           "${docs_dir}"/spr-642245/*/spr_uc_events*.txt; do
  [[ -f "${src}" ]] && cp -- "${src}" "${corpus_dir}/intel-docs/"
done
shopt -u nullglob

for src in "${linux_dir}"/arch/x86/events/intel/uncore*; do
  [[ -f "${src}" ]] && cp -- "${src}" "${corpus_dir}/linux-uncore/"
done
for src in "${linux_dir}"/Documentation/ABI/testing/sysfs-bus-event_source-devices*; do
  [[ -f "${src}" ]] && cp -- "${src}" "${corpus_dir}/linux-uncore/"
done

find "${corpus_dir}/intel-perfmon" -type f \( -name '*.json' -o -name '*.csv' \) | while IFS= read -r src; do
  rel="${src#${corpus_dir}/intel-perfmon/}"
  if [[ "${src}" == *.json ]]; then
    continue
  fi
  copy_if_exists "${src}" "${corpus_dir}/text-shadows/intel-perfmon/${rel}.txt"
done

find "${corpus_dir}/linux-uncore" -maxdepth 1 -type f | while IFS= read -r src; do
  copy_if_exists "${src}" "${corpus_dir}/text-shadows/linux-uncore/$(basename -- "${src}").txt"
done

log "generating normalized event metadata"
"${python_bin}" "${repo_root}/scripts/generate-uncore-rag-metadata.py" \
  --corpus-dir "${corpus_dir}" \
  --linux-dir "${linux_dir}"

if [[ "${skip_docmancer}" -eq 1 ]]; then
  log "skipped Docmancer install/index"
  exit 0
fi

check_python
log "installing Docmancer into ${docmancer_dir#${repo_root}/}/venv"
"${python_bin}" -m venv "${docmancer_dir}/venv"
"${docmancer_dir}/venv/bin/python" -m pip install --upgrade pip docmancer

config="${docmancer_dir}/docmancer.yaml"
cat > "${config}" <<EOF
index:
  provider: sqlite
  db_path: ${docmancer_dir}/ocellus-uncore.db
  extracted_dir: ${docmancer_dir}/extracted

query:
  default_budget: 3200
  default_limit: 10
  default_expand: adjacent

web_fetch:
  workers: 4
  default_page_cap: 100
  browser_fallback: false

loaders:
  default_chunk_size: 900
  default_chunk_overlap: 120
  formats:
    pdf:
      chunk_size: 900
      chunk_overlap: 120
    txt:
      chunk_size: 1000
      chunk_overlap: 120

retrieval:
  default_mode: lexical
  fusion:
    method: rrf
    rrf_k: 60
    weights: {}
  hierarchical:
    enabled: false
    documents_limit: 5
    candidate_pool: 200
    sections_per_document: 10
EOF

log "indexing corpus with Docmancer FTS5"
"${docmancer_dir}/venv/bin/docmancer" \
  --config "${config}" \
  ingest "${corpus_dir}" \
  --format md \
  --format pdf \
  --format txt \
  --recreate \
  --no-vectors

if [[ "${install_codex_skill}" -eq 1 ]]; then
  "${docmancer_dir}/venv/bin/docmancer" install codex --config "${config}"
fi

log "done"
log "query with: ${docmancer_dir#${repo_root}/}/venv/bin/docmancer --config ${config#${repo_root}/} query 'your uncore question'"
