#!/usr/bin/env bash
# Representative-copy rehearsal for the current v43 -> v47 migration chain.
# The supplied source bundle is read only as ordinary files; SQLite opens only staged copies.
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

python3 scripts/verify_v47_transplant_copy.py \
  manifest "$src" "${workdir}/source-before.json"
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
python3 scripts/verify_v47_transplant_copy.py \
  before "${workdir}/pristine.db" "${workdir}/before.json"

echo "==> initialize fresh v47 schema catalog"
OPENCRAB_V47_APPLY_DB="${workdir}/fresh.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v47_copy_db -- --exact --nocapture

echo "==> initialize copy A once"
OPENCRAB_V47_APPLY_DB="${workdir}/a.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v47_copy_db -- --exact --nocapture

echo "==> initialize copy B twice (second apply must be a no-op)"
OPENCRAB_V47_APPLY_DB="${workdir}/b.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v47_copy_db -- --exact --nocapture
OPENCRAB_V47_APPLY_DB="${workdir}/b.db" cargo test -p opencrab-db --lib \
  schema::migration_tests::apply_initialize_to_v47_copy_db -- --exact --nocapture

python3 scripts/verify_v47_transplant_copy.py after \
  "${workdir}/before.json" "${workdir}/a.db" "${workdir}/b.db" "${workdir}/fresh.db"

python3 scripts/verify_v47_transplant_copy.py \
  manifest "$src" "${workdir}/source-after.json"
if ! cmp -s "${workdir}/source-before.json" "${workdir}/source-after.json"; then
  echo "source DB/WAL/SHM bundle changed during rehearsal" >&2
  exit 1
fi

echo "  source DB/WAL/SHM existence, size, and SHA-256 unchanged"
echo "v43 -> v47 representative-copy rehearsal GREEN"
