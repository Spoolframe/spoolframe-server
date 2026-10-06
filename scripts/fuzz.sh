#!/usr/bin/env bash
# Fuzz campaigns over what Spoolframe reads from peers, files and a PMS
# (PX-10). See FUZZING.md.
#
#   scripts/fuzz.sh [SECONDS] [TARGET...]
#
# Runs each TARGET (all of them by default) for SECONDS (default 60) with
# cargo-fuzz and libFuzzer, under AddressSanitizer and debug assertions.
# The nightly toolchain and cargo-fuzz come from fuzz/toolchain.nix, so the
# dev shell is not needed. Each target grows its corpus in fuzz/corpus/
# (not tracked) from the seeds in fuzz/seeds/ and its crate's fixtures. A
# crash is kept in fuzz/artifacts/TARGET/; the script goes on with the
# other targets and exits 1 if any target crashed.
#
# Extra libFuzzer flags go in FUZZ_FLAGS, e.g. FUZZ_FLAGS="-jobs=4 -workers=4".
set -euo pipefail
cd "$(dirname "$0")/.."

seconds=${1:-60}
shift || true
all=(protocol_frame protocol_cbor pgs dvb vobsub bif plex_json ffprobe_json)
targets=("${@:-${all[@]}}")

# Read-only seed directories for a target, beyond fuzz/seeds/TARGET.
fixtures() {
  case "$1" in
  bif | plex_json) echo crates/spoolframe-plex/tests/fixtures ;;
  ffprobe_json) echo crates/spoolframe-media/tests/fixtures ;;
  esac
}

failed=()
for target in "${targets[@]}"; do
  corpus=fuzz/corpus/$target
  mkdir -p "$corpus"
  seeds=()
  [[ -d fuzz/seeds/$target ]] && seeds+=("$PWD/fuzz/seeds/$target")
  for dir in $(fixtures "$target"); do seeds+=("$PWD/$dir"); done
  echo "== $target: ${seconds}s"
  # shellcheck disable=SC2086 # FUZZ_FLAGS is a list of flags.
  if ! nix shell --impure -f fuzz/toolchain.nix -c \
    bash -c 'cd fuzz && exec cargo fuzz run "$@"' _ \
    "$target" "$PWD/$corpus" "${seeds[@]}" -- \
    -max_total_time="$seconds" -timeout=10 -rss_limit_mb=2048 -print_final_stats=1 \
    ${FUZZ_FLAGS:-}; then
    failed+=("$target")
  fi
done

if ((${#failed[@]})); then
  echo "crashed: ${failed[*]} (inputs in fuzz/artifacts/)" >&2
  exit 1
fi
echo "no crashes: ${targets[*]}"
