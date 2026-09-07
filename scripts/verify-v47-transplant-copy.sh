#!/usr/bin/env bash
# Read-only representative-copy rehearsal for the current v43 -> v47 migration chain.
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

file_size() {
  stat -f %z "$1" 2>/dev/null || stat -c %s "$1"
}
file_sha256() {
  shasum -a 256 "$1" | awk '{print $1}'
}

src_size_before="$(file_size "$src")"
src_hash_before="$(file_sha256 "$src")"
src_version_before="$(sqlite3 "file:${src}?mode=ro" 'PRAGMA user_version;')"
if [[ "$src_version_before" != "43" ]]; then
  echo "source user_version=$src_version_before; want 43" >&2
  exit 1
fi

echo "==> create pristine read-only SQLite backup"
sqlite3 "file:${src}?mode=ro" ".backup '${workdir}/pristine.db'"
sqlite3 "${workdir}/pristine.db" ".backup '${workdir}/a.db'"
sqlite3 "${workdir}/pristine.db" ".backup '${workdir}/b.db'"
python3 scripts/verify_v47_transplant_copy.py \
  before "${workdir}/pristine.db" "${workdir}/before.json"

echo "==> initialize copy A once"
OPENCRAB_V47_APPLY_DB="${workdir}/a.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v47_copy_db -- --exact --nocapture

echo "==> initialize copy B twice (second apply must be a no-op)"
OPENCRAB_V47_APPLY_DB="${workdir}/b.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v47_copy_db -- --exact --nocapture
OPENCRAB_V47_APPLY_DB="${workdir}/b.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v47_copy_db -- --exact --nocapture

python3 scripts/verify_v47_transplant_copy.py \
  after "${workdir}/before.json" "${workdir}/a.db" "${workdir}/b.db"

src_size_after="$(file_size "$src")"
src_hash_after="$(file_sha256 "$src")"
src_version_after="$(sqlite3 "file:${src}?mode=ro" 'PRAGMA user_version;')"
if [[ "$src_size_after" != "$src_size_before" || \
      "$src_hash_after" != "$src_hash_before" || \
      "$src_version_after" != "$src_version_before" ]]; then
  echo "source copy changed during read-only rehearsal" >&2
  exit 1
fi

echo "  source size/hash/user_version unchanged"
echo "v43 -> v47 representative-copy rehearsal GREEN"
