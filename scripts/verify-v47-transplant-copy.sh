#!/usr/bin/env bash
# Rust-backed representative-copy rehearsal for the current v43 -> v47 migration chain.
# The supplied source bundle is read only as files; SQLite opens only staged copies.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

src="${OPENCRAB_REHEARSAL_DB:-}"
if [[ -z "$src" ]]; then
  echo "OPENCRAB_REHEARSAL_DB is required (isolated v43 source copy)" >&2
  exit 2
fi
if [[ ! -f "$src" ]]; then
  echo "OPENCRAB_REHEARSAL_DB is not a file" >&2
  exit 2
fi

workdir="$(mktemp -d "${TMPDIR:-/tmp}/v47-rehearsal.XXXXXX")"
cleanup() { rm -rf "$workdir"; }
trap cleanup EXIT

clone_file() {
  local from="$1" to="$2"
  if cp -c "$from" "$to" 2>/dev/null; then
    return
  fi
  rm -f "$to"
  if cp --reflink=auto -p "$from" "$to" 2>/dev/null; then
    return
  fi
  rm -f "$to"
  cp -p "$from" "$to"
}

bundle_manifest() {
  local db="$1" suffix file
  for suffix in "" -wal -shm; do
    file="${db}${suffix}"
    if [[ -f "$file" ]]; then
      printf '%s|present|' "$suffix"
      stat -f %z "$file" 2>/dev/null || stat -c %s "$file"
      shasum -a 256 "$file" | awk '{print $1}'
    else
      printf '%s|absent\n' "$suffix"
    fi
  done
}

bundle_manifest "$src" > "${workdir}/source-before.txt"
clone_file "$src" "${workdir}/staged.db"
for suffix in -wal -shm; do
  if [[ -f "${src}${suffix}" ]]; then
    clone_file "${src}${suffix}" "${workdir}/staged.db${suffix}"
  fi
done

echo "==> create pristine SQLite backup from staged source bundle"
sqlite3 "file:${workdir}/staged.db?mode=ro" ".backup '${workdir}/pristine.db'"
sqlite3 "${workdir}/pristine.db" ".backup '${workdir}/a.db'"
sqlite3 "${workdir}/pristine.db" ".backup '${workdir}/b.db'"

echo "==> Rust v43 -> v47 data/schema/fixed-point rehearsal"
OPENCRAB_V47_REHEARSAL_DIR="$workdir" cargo test -p opencrab-db --lib \
  schema::migration_tests::rehearse_v43_copy_to_v47 -- --exact --nocapture

bundle_manifest "$src" > "${workdir}/source-after.txt"
if ! cmp -s "${workdir}/source-before.txt" "${workdir}/source-after.txt"; then
  echo "source DB/WAL/SHM bundle changed during rehearsal" >&2
  exit 1
fi

echo "  source DB/WAL/SHM existence, size, and SHA-256 unchanged"
echo "v43 -> v47 representative-copy rehearsal GREEN"
