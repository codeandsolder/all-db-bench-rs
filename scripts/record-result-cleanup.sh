#!/usr/bin/env bash
# Shared post-process cleanup for record benchmarks.
# The benchmark child runs with --keep-db so engine background workers are gone
# before the runner removes the case database.
record_cleanup_result_db() {
  local out=$1 data_dir=$2 db_path data_real root_real tmp
  [[ -s "$out" ]] || { echo "record cleanup: missing result: $out" >&2; return 2; }
  db_path=$(jq -er '.path | strings' "$out") || {
    echo "record cleanup: result has no database path: $out" >&2
    return 2
  }
  root_real=$(readlink -f -- "$data_dir") || return 2
  data_real=$(readlink -m -- "$db_path") || return 2
  case "$data_real" in
    "$root_real"/*) ;;
    *) echo "record cleanup: refusing path outside data root: $data_real" >&2; return 2 ;;
  esac
  if [[ -e "$data_real" ]]; then
    rm -rf --one-file-system -- "$data_real" || {
      echo "record cleanup: failed to remove $data_real" >&2
      return 2
    }
  fi
  [[ ! -e "$data_real" ]] || {
    echo "record cleanup: path remains after removal: $data_real" >&2
    return 2
  }
  tmp="${out}.cleanup.$$"
  jq -c '.db_kept = false' "$out" > "$tmp" || { rm -f "$tmp"; return 2; }
  mv "$tmp" "$out"
}
