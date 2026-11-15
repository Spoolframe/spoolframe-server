#!/usr/bin/env bash
# The client SDK built and tested against the newest rusqlite its range
# allows (crates/spoolframe-client-sdk/Cargo.toml), as an app resolves it.
# This workspace's own lock cannot: the server's sqlx caps libsqlite3-sys,
# and a tree has one copy of it. So the tracked tree is copied, the copy's
# workspace narrowed to the SDK (its path dependencies join it as
# members), and rusqlite updated to the newest in range there; the real
# lock is never touched.
#
# The copy lives under target/ (SDK_SQLITE_DIR), with its own target dir,
# kept between runs so a second run builds only what changed.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
work=${SDK_SQLITE_DIR:-$root/target/sdk-newest-sqlite}
src=$work/src

rm -rf "$src"
mkdir -p "$src"
# Tracked files and new ones not ignored: the tree as it would be
# committed, local edits included.
(cd "$root" && git ls-files -z --cached --others --exclude-standard) |
  (cd "$root" && tar --null -T - -cf -) | tar -xf - -C "$src"

sed -i 's|^members = \["crates/\*"\]$|members = ["crates/spoolframe-client-sdk"]|' "$src/Cargo.toml"
grep -q '^members = \["crates/spoolframe-client-sdk"\]$' "$src/Cargo.toml" || {
  echo "the workspace's members line changed; update $0" >&2
  exit 1
}

cd "$src"
export CARGO_TARGET_DIR=$work/target
cargo update -p rusqlite
version() { cargo tree -p spoolframe-client-sdk -e normal -i "$1" --depth 0 | head -1; }
rusqlite=$(version rusqlite)
sqlite=$(version libsqlite3-sys)
echo "resolved: $rusqlite, $sqlite"
# The server's own pair: if this is all the range resolves to, the test
# proves nothing.
case "$rusqlite" in
  "rusqlite v0.39."*) echo "rusqlite was not updated past 0.39" >&2; exit 1 ;;
esac
cargo test -p spoolframe-client-sdk --locked
