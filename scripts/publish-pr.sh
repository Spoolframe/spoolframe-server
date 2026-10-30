#!/usr/bin/env bash
# publish-pr: a pre-release of the crates from a PR's branch, to the
# Coldsteel registry only, so an app can try a new SDK before the PR merges.
# .forgejo/workflows/publish-pr.yaml runs it; crates.io never sees these.
#
# Each commit gets its own version, the base and the short sha:
# 0.17.0-beta.3.gab12d3b. The `g` keeps the sha valid semver (a pre-release
# number cannot start with a zero, and an all-digit sha would be one). It
# sorts after the base and before the release and the next beta, and
# `"0.17"` never picks it up. A release base (0.17.0) has no pre-release to
# extend, so it becomes 0.17.0-gab12d3b, which sorts before that release.
#
# The crates depend on each other as crates.io crates, where this version
# does not exist. So this rewrites the root Cargo.toml, in the working tree
# and NEVER to be committed: run it on a throwaway checkout. The workspace
# version becomes the new one, and each published sibling in
# [workspace.dependencies] gets it too, with `registry = "coldsteel"`.
# Published to Coldsteel, such a dependency has no registry in the index,
# which means "this one", so an app needs no [patch]. The version is exact
# (`=`): a caret requirement on a pre-release also takes any later
# pre-release, another commit's sibling.
#
# The rest is scripts/publish-changed.sh: the order, the index check, the
# retries. A crate already there is skipped, so the same commit can run
# again. `--no-verify` is its own for this registry: CI builds the commit.
#
# Requires what publish-changed.sh does, and git.
#
# Usage:
#   publish-pr.sh [--publish] <workspace-root>
#
# Options:
#   --publish  Publish. Requires CARGO_REGISTRIES_COLDSTEEL_TOKEN. Without
#              this flag the script rewrites the tree and reports what it
#              would publish.
#   -h, --help Show this help.

set -euo pipefail

usage() {
    sed -n '2,36p' "$0" | sed 's/^# \{0,1\}//'
}

publish=()
case "${1:-}" in
    --publish)
        publish=(--publish)
        shift
        ;;
    -h|--help)
        usage
        exit 0
        ;;
esac

if [[ $# -ne 1 ]]; then
    usage >&2
    exit 1
fi

scripts=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# From the root, as cargo must be: `registry = "coldsteel"` means nothing
# outside the reach of its .cargo/config.toml.
cd "$1"

# The crates that go to Coldsteel, each on one line with its version.
published() {
    cargo metadata --no-deps --format-version 1 | jq -r '
        .packages[]
        | select((.publish // []) | index("coldsteel") != null)
        | "\(.name) \(.version)"
    '
}

mapfile -t crates < <(published)
if [[ ${#crates[@]} -eq 0 ]]; then
    echo "Error: no crate here publishes to coldsteel." >&2
    exit 1
fi

base=${crates[0]#* }
sha=$(git rev-parse --short=7 HEAD)
if [[ "$base" == *-* ]]; then
    version="$base.g$sha"
else
    version="$base-g$sha"
fi

rewrite=(-e "/^\[workspace\.package\]/,/^\[/ s/^version = \".*\"\$/version = \"$version\"/")
for crate in "${crates[@]}"; do
    name=${crate% *}
    rewrite+=(-e "/^\[workspace\.dependencies\]/,/^\[/ s/^($name = \{.*)version = \"[^\"]*\"/\1version = \"=$version\", registry = \"coldsteel\"/")
done
sed -i -E "${rewrite[@]}" Cargo.toml

# Only the workspace's own entries move; every other crate stays locked.
cargo update --workspace

# What the rewrite did not reach would go out at the wrong version, or
# against crates.io, where it cannot resolve. Stop before anything does.
wrong=$(cargo metadata --no-deps --format-version 1 | jq -r --arg v "$version" '
    [.packages[] | select((.publish // []) | index("coldsteel") != null)] as $crates
    | ($crates | map(.name)) as $names
    | $crates[]
    | .name as $name
    | (select(.version != $v) | "\($name) is at \(.version)"),
      (.dependencies[]
       | select(.name as $dep | $names | index($dep) != null)
       | select(.req != "=\($v)" or .registry == null)
       | "\($name) -> \(.name) \(.req), registry \(.registry)")
')
if [[ -n "$wrong" ]]; then
    echo "Error: the rewrite to $version missed:" >&2
    echo "$wrong" >&2
    exit 1
fi

"$scripts/publish-changed.sh" "${publish[@]}" --allow-dirty --registry coldsteel .

if [[ ${#publish[@]} -gt 0 ]]; then
    echo "::notice title=Coldsteel PR build::spoolframe-client-sdk = { version = \"=$version\", registry = \"coldsteel\" }"
else
    echo "Version: $version"
fi
