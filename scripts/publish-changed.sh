#!/usr/bin/env bash
# publish-changed: publish-on-version-change for cargo workspaces.
#
# For each given workspace root: read the workspace's publishable crates
# from `cargo metadata` (respecting each crate's `publish` field — never a
# hand-maintained list), compute their topological dependency order (deps
# before dependents — crates.io rejects a crate whose in-workspace dep
# versions aren't present yet), and for each crate compare its Cargo.toml
# version against one registry's sparse index: crates.io's
# (https://index.crates.io/) or the Coldsteel registry's (kellnr, where
# the apps take the crates from during development). A version already on
# the index is skipped; an absent version is reported (dry run, the
# default) or published (--publish). One run is one registry:
# .forgejo/workflows/publish.yaml runs it for each.
#
# This is the version-diff mechanism, not a path/tag trigger: it is safe to
# run on every push to master, including pushes that touch nothing
# publishable, and a missed or failed publish self-heals on the next run
# instead of being lost.
#
# Publishing must be opted into explicitly: every crate whose manifest is
# not `publish = false` must say `publish = ["crates-io", "coldsteel"]`
# exactly (either order), the two registries every public crate goes to. An
# absent `publish` field — and the equivalent `publish = true` — also
# means crates.io to cargo, but a crate that never said so would go public
# by default the moment it joined the workspace. That is a hard error
# here, not a skip.
#
# crates.io is slower to take a crate than the Coldsteel registry, in
# three ways this script waits out, each bounded and logged (the last for
# every registry, the first two for crates.io):
#   - it rate-limits NEW crates (a small burst, then about one every 10
#     minutes): a publish refused with HTTP 429 or "rate limit" waits the
#     time crates.io names (or 10 minutes) and tries again;
#   - its index lags a publish by seconds: the next crate in order waits
#     until the version it depends on shows on the index;
#   - `cargo publish` resolves a crate's dependencies through crates.io's
#     own index, which may lag a publish in turn: a publish refused
#     because a just-published dependency is not found yet waits and
#     tries again. Never through what replaces crates-io in the caller's
#     cargo home (the devenv shell's kellnr proxy, devenv.nix): `cargo
#     publish` runs with a home of its own (PUBLISH_HOME, below).
#
# Copied from foundry's scripts/publish-changed.sh when spoolframe left the
# monorepo; .forgejo/workflows/publish.yaml runs it on every master push.
#
# Requires on PATH: cargo (the workspace's own toolchain — run this inside
# that workspace's devenv shell so `cargo publish`'s verify build has the
# right native deps), curl, jq, date (GNU, to read crates.io's retry time).
#
# crates.io goes first (.forgejo/workflows/publish.yaml), another
# registry after it. The crates depend on each other as crates.io crates
# (the apps patch them to the Coldsteel copies), and `cargo publish`
# resolves them for the packaged Cargo.lock even with `--no-verify`: to
# another registry, a new version's siblings must be on crates.io
# already. That publish is `--no-verify`, because the crates.io run has
# just verify-built the same sources; and it resolves crates.io through
# whatever replaces it in cargo's config (the kellnr proxy), which may
# lag crates.io, so a sibling not found yet is waited for as below.
#
# Usage:
#   publish-changed.sh [--publish] [--registry NAME] [--]
#                      <workspace-root> [<workspace-root> ...]
#
# Options:
#   --registry NAME  The registry to compare with and publish to: crates-io
#                    (the default) or one cargo's config names (coldsteel).
#                    Its sparse index is cargo's: CARGO_REGISTRIES_<NAME>_INDEX,
#                    else [registries.<name>] in the first workspace root's
#                    .cargo/config.toml.
#   --publish        Actually run `cargo publish` for crates found absent
#                    from the index. Requires the registry's token, as
#                    cargo reads it: CARGO_REGISTRY_TOKEN for crates-io,
#                    CARGO_REGISTRIES_<NAME>_TOKEN for another
#                    (CARGO_REGISTRIES_COLDSTEEL_TOKEN). Without this flag
#                    the script only reports what it would do (dry run) and
#                    never touches the registry beyond reading its index.
#   -h, --help       Show this help.
#
# A publish failure aborts the script immediately (no silent skip) — the
# job that runs this must go red, because a red publish job means every
# app resolving these crates is about to be wrong.
#
# Exit status: 0 if every configured workspace was processed without a
# publish error; 1 on any hard failure (bad workspace, index unreachable,
# publish failed or still rate-limited after its retries, dependency cycle
# among publishable crates, a crate that is publishable without an
# explicit `publish = ["crates-io", "coldsteel"]`).

set -euo pipefail

REGISTRY_NAME="crates-io"
INDEX_BASE=""
# What every publishable crate's `publish` must be, sorted.
PUBLISH_TO='["coldsteel","crates-io"]'
# A refused publish is tried this many times in all: to crates.io, and to
# another registry.
PUBLISH_ATTEMPTS=8
OTHER_ATTEMPTS=8
# Between tries to another registry, but for a dependency not found yet.
OTHER_WAIT=10
# Without a time in crates.io's answer: its rate for new crates.
RATE_LIMIT_WAIT=600
# Never wait longer than this for one rate limit, whatever crates.io says.
RATE_LIMIT_WAIT_MAX=1800
# A dependency cargo cannot find yet is looked for again after this long.
DEP_WAIT=30

usage() {
    sed -n '2,87p' "$0" | sed 's/^# \{0,1\}//'
}

publish=0
roots=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --publish)
            publish=1
            shift
            ;;
        --registry)
            REGISTRY_NAME="${2:?--registry needs a name}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        -*)
            echo "Error: Unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
        *)
            break
            ;;
    esac
done

roots+=("$@")

if [[ ${#roots[@]} -eq 0 ]]; then
    echo "Error: at least one workspace root is required." >&2
    usage >&2
    exit 1
fi


# CARGO_REGISTRIES_<NAME>_..., as cargo spells a registry's variables.
REGISTRY_VAR="CARGO_REGISTRIES_$(tr '[:lower:]-' '[:upper:]_' <<<"$REGISTRY_NAME")"

# The variable cargo reads the registry's token from.
if [[ "$REGISTRY_NAME" == "crates-io" ]]; then
    TOKEN_VAR="CARGO_REGISTRY_TOKEN"
else
    TOKEN_VAR="${REGISTRY_VAR}_TOKEN"
fi

if [[ "$publish" -eq 1 && -z "${!TOKEN_VAR:-}" ]]; then
    echo "Error: --publish to $REGISTRY_NAME requires $TOKEN_VAR to be set." >&2
    exit 1
fi

for bin in cargo curl jq date; do
    if ! command -v "$bin" >/dev/null 2>&1; then
        echo "Error: '$bin' not found on PATH." >&2
        exit 1
    fi
done

# `cargo publish` runs with a cargo home of its own, made empty here and
# removed on exit, so a crate's dependencies resolve through crates.io's
# own index. The caller's home may replace crates-io (the devenv shell
# writes coldsteel-mirror, kellnr's crates.io proxy, into it), and that
# proxy's index entry for a crate can stay stale for good: 0.17.0-beta.1's
# spoolframe-protocol found no spoolframe-types 0.17.0-beta.1 through it,
# eight tries over, on both registries' runs. The home holds nothing a
# publish needs: tokens come from the environment, and [registries.*]
# from the workspace's .cargo/config.toml. cargo metadata and the index
# reads (curl) use the caller's.
PUBLISH_HOME=$(mktemp -d)
trap 'rm -rf "$PUBLISH_HOME"' EXIT

# config_index ROOT
# `index` under [registries.<name>] in ROOT's .cargo/config.toml; empty
# where it has none.
config_index() {
    local config="$1/.cargo/config.toml"
    [[ -f "$config" ]] || return 0
    awk -v section="[registries.$REGISTRY_NAME]" '
        /^[[:space:]]*\[/ { inside = ($0 == section) }
        inside && /^[[:space:]]*index[[:space:]]*=/ {
            sub(/^[^=]*=[[:space:]]*"/, ""); sub(/".*$/, ""); print; exit
        }
    ' "$config"
}

# The registry's sparse index, as cargo has it.
if [[ "$REGISTRY_NAME" == "crates-io" ]]; then
    INDEX_BASE="https://index.crates.io"
else
    index_var="${REGISTRY_VAR}_INDEX"
    INDEX_BASE="${!index_var:-$(config_index "${roots[0]}")}"
    if [[ "$INDEX_BASE" != sparse+* ]]; then
        echo "Error: no sparse index for registry $REGISTRY_NAME: set $index_var or [registries.$REGISTRY_NAME] index = \"sparse+...\" in ${roots[0]}/.cargo/config.toml (found: '${INDEX_BASE}')." >&2
        exit 1
    fi
    INDEX_BASE="${INDEX_BASE#sparse+}"
fi
INDEX_BASE="${INDEX_BASE%/}"

# Sparse index path for a crate name, per Cargo's registry index format:
# https://doc.rust-lang.org/cargo/reference/registries.html#index-format
index_path() {
    local name="$1" len
    len=${#name}
    case "$len" in
        1) echo "1/$name" ;;
        2) echo "2/$name" ;;
        3) echo "3/${name:0:1}/$name" ;;
        *) echo "${name:0:2}/${name:2:2}/$name" ;;
    esac
}

# version_published NAME VERSION
# Returns success (0) if VERSION is present on the index, failure (1) if
# the crate/version is absent. Hard-exits the whole script on any
# registry-index error other than a clean 404 (unknown crate) — an
# unreachable registry must not be misread as "nothing published yet".
version_published() {
    local name="$1" version="$2" path url status tmp found
    path=$(index_path "$name")
    url="$INDEX_BASE/$path"
    tmp=$(mktemp)

    if ! status=$(curl -sS -o "$tmp" -w '%{http_code}' "$url"); then
        echo "Error: failed to reach registry index for $name: $url" >&2
        rm -f "$tmp"
        exit 1
    fi

    if [[ "$status" == "404" ]]; then
        rm -f "$tmp"
        return 1
    fi

    if [[ "$status" != "200" ]]; then
        echo "Error: unexpected HTTP $status from registry index for $name: $url" >&2
        cat "$tmp" >&2
        rm -f "$tmp"
        exit 1
    fi

    found=$(jq -s --arg v "$version" '[.[] | select(.vers == $v)] | length' "$tmp")
    rm -f "$tmp"
    [[ "$found" -gt 0 ]]
}

# wait_for_index NAME VERSION
# Poll the index after a publish until the new version is visible, so the
# next crate in topological order doesn't try to resolve it too early.
# crates.io's index trails a publish by seconds, sometimes more: five
# minutes before giving up. Hard-exits if it never shows up — a publish
# that "succeeded" but never reached the index is exactly the failure mode
# this job exists to catch.
wait_for_index() {
    local name="$1" version="$2" attempt
    for ((attempt = 0; attempt < 60; attempt++)); do
        if version_published "$name" "$version"; then
            return 0
        fi
        [[ "$attempt" -eq 0 ]] && echo "  [wait]    $name $version: not on the index yet, polling every 5 s"
        sleep 5
    done
    echo "Error: $name $version did not appear on $REGISTRY_NAME's index after publishing (timed out after 5 minutes)." >&2
    exit 1
}

# rate_limit_wait LOG
# Seconds to wait before publishing again, from a refusal crates.io
# rate-limited ("Please try again after Tue, 27 Oct 2026 15:20:00 GMT"):
# until that time and a few seconds more, at least 1 s and at most
# RATE_LIMIT_WAIT_MAX; RATE_LIMIT_WAIT when it names no time this can read.
rate_limit_wait() {
    local log="$1" when until now
    when=$(grep -o -i -E 'try again after [^.]*GMT' "$log" | head -1 | sed -E 's/^[Tt]ry again after //') || true
    if [[ -n "$when" ]] && until=$(date -d "$when" +%s 2>/dev/null); then
        now=$(date +%s)
        local secs=$((until - now + 5))
        ((secs < 1)) && secs=1
        ((secs > RATE_LIMIT_WAIT_MAX)) && secs=$RATE_LIMIT_WAIT_MAX
        echo "$secs"
    else
        echo "$RATE_LIMIT_WAIT"
    fi
}

# dependency_not_found LOG
# Whether a refused `cargo publish` in LOG could not find a spoolframe
# dependency: one published moments ago, not seen yet where cargo looks.
dependency_not_found() {
    # The backticks are cargo's, matched as they are.
    # shellcheck disable=SC2016
    grep -q -E 'no matching package named `spoolframe-|failed to select a version for the requirement `spoolframe-' "$1"
}

# publish_other ROOT NAME VERSION
# `cargo publish --no-verify` to a registry other than crates.io (see the
# header). Tried again, up to OTHER_ATTEMPTS in all, when a sibling it
# depends on is not found yet through the source cargo resolves crates.io
# with (waiting DEP_WAIT), or for any other refusal (waiting OTHER_WAIT),
# unless the version is on the index after all (an upload whose answer was
# lost). Hard-exits past the last.
publish_other() {
    local root="$1" name="$2" version="$3" attempt log
    log=$(mktemp)
    for ((attempt = 1; attempt <= OTHER_ATTEMPTS; attempt++)); do
        if (cd "$root" && CARGO_HOME="$PUBLISH_HOME" cargo publish --registry "$REGISTRY_NAME" -p "$name" --locked --no-verify) 2>&1 | tee "$log"; then
            rm -f "$log"
            return 0
        fi
        if version_published "$name" "$version"; then
            rm -f "$log"
            return 0
        fi
        if ((attempt == OTHER_ATTEMPTS)); then
            break
        fi
        if dependency_not_found "$log"; then
            echo "  [wait]    $name $version: cargo does not see a dependency on crates.io yet (attempt $attempt of $OTHER_ATTEMPTS); waiting ${DEP_WAIT}s"
            sleep "$DEP_WAIT"
        else
            echo "  [retry]   $name $version: $REGISTRY_NAME refused it (attempt $attempt of $OTHER_ATTEMPTS); waiting ${OTHER_WAIT}s"
            sleep "$OTHER_WAIT"
        fi
    done
    rm -f "$log"
    echo "Error: cargo publish to $REGISTRY_NAME failed for $name $version in $root" >&2
    exit 1
}

# publish_crate ROOT NAME VERSION
# `cargo publish` to crates.io, tried again — up to PUBLISH_ATTEMPTS in
# all — when crates.io rate-limits it, or when the dependency it was just
# preceded by is not found yet through the source cargo resolves with.
# Any other failure, or one of those past the last attempt, hard-exits.
#
# `--registry crates-io`, not the default: where a cargo config replaces
# crates-io (the coldsteel devenv profile's kellnr proxy), naming it makes
# the upload go to crates.io itself rather than to the replacement.
publish_crate() {
    local root="$1" name="$2" version="$3" attempt log wait
    log=$(mktemp)
    for ((attempt = 1; attempt <= PUBLISH_ATTEMPTS; attempt++)); do
        if (cd "$root" && CARGO_HOME="$PUBLISH_HOME" cargo publish --registry "$REGISTRY_NAME" -p "$name" --locked) 2>&1 | tee "$log"; then
            rm -f "$log"
            return 0
        fi
        # Uploaded after all: a retry whose first try timed out after the
        # upload, or a crate published by hand meanwhile.
        if version_published "$name" "$version"; then
            rm -f "$log"
            return 0
        fi
        if ((attempt == PUBLISH_ATTEMPTS)); then
            break
        fi
        if grep -q -i -E '429|too many requests|rate limit|too many new crates|try again after' "$log"; then
            wait=$(rate_limit_wait "$log")
            echo "  [rate-limited] $name $version: crates.io refused it (attempt $attempt of $PUBLISH_ATTEMPTS); waiting ${wait}s"
            sleep "$wait"
        elif dependency_not_found "$log"; then
            echo "  [wait]    $name $version: cargo does not see a dependency it was published after yet (attempt $attempt of $PUBLISH_ATTEMPTS); waiting ${DEP_WAIT}s"
            sleep "$DEP_WAIT"
        else
            break
        fi
    done
    rm -f "$log"
    echo "Error: cargo publish failed for $name $version in $root" >&2
    exit 1
}

# process_workspace ROOT
process_workspace() {
    local root="$1"
    local manifest="$root/Cargo.toml"

    echo "=== workspace: $root, registry: $REGISTRY_NAME ($INDEX_BASE) ==="

    if [[ ! -f "$manifest" ]]; then
        echo "Error: no Cargo.toml at $manifest" >&2
        exit 1
    fi

    local meta
    if ! meta=$(cargo metadata --no-deps --format-version 1 --manifest-path "$manifest"); then
        echo "Error: cargo metadata failed for $root" >&2
        exit 1
    fi

    # Opt-in gate. Publishing is a deliberate, per-crate decision, so the
    # only two manifest states this script accepts are `publish = false`
    # (an unpublishable crate — `[]` in cargo metadata) and
    # `publish = ["crates-io", "coldsteel"]` exactly. Everything else is a
    # hard error:
    #
    #   - an absent `publish` field, and the identical-in-metadata
    #     `publish = true`, both mean "publishable to crates.io" too, but
    #     by cargo's default, which makes the dangerous state the one you
    #     get by writing nothing — a new crate in a configured workspace
    #     would publish itself on the next master push, and nobody decided
    #     it should be public;
    #   - an array naming any other registry, or only one of the two, is a
    #     crate whose blast radius differs from this job's.
    #
    # No skip, no "publish the crates.io ones and ignore the rest": a
    # manifest this script cannot vouch for stops the whole workspace rather
    # than falling back to a guess. The fix is always
    # one line in the named crate's Cargo.toml.
    local unvetted
    unvetted=$(jq -r --argjson to "$PUBLISH_TO" '
        .packages[]
        | select(.publish != [] and ((.publish // []) | sort) != $to)
        | "  \(.name) \(.version): publish = " +
          (if .publish == null then "absent (or `true`)" else (.publish | tojson) end)
    ' <<<"$meta")

    if [[ -n "$unvetted" ]]; then
        echo "Error: crate(s) in $root are publishable without an explicit \`publish = [\"crates-io\", \"coldsteel\"]\`:" >&2
        echo "$unvetted" >&2
        echo "Set \`publish = [\"crates-io\", \"coldsteel\"]\` to publish it to both, or \`publish = false\` to keep it out of every registry." >&2
        exit 1
    fi

    # Publishable crates: exactly those that named this registry above.
    # `publish = false` comes back from cargo metadata as an empty array,
    # which the index() check naturally excludes. Never a hand-maintained
    # crate list — this reads straight off the manifests, same as cargo
    # itself would.
    local names_raw
    names_raw=$(jq -r --arg reg "$REGISTRY_NAME" '
        .packages[]
        | select(.publish | index($reg) != null)
        | .name
    ' <<<"$meta" | sort)

    if [[ -z "$names_raw" ]]; then
        echo "No publishable crates in $root."
        return 0
    fi

    local -a names=()
    while IFS= read -r line; do
        [[ -n "$line" ]] && names+=("$line")
    done <<<"$names_raw"

    # version[name], indeg[name] (in-degree for Kahn's algorithm), and
    # adj[name] (space-separated dependents) — all local to this function
    # call, so processing multiple workspace roots in one invocation never
    # cross-contaminates.
    local -A version=()
    local -A indeg=()
    local -A adj=()

    local n
    for n in "${names[@]}"; do
        version[$n]=$(jq -r --arg n "$n" '.packages[] | select(.name == $n) | .version' <<<"$meta")
        indeg[$n]=0
        adj[$n]=""
    done

    # Edges: for each publishable crate, each in-workspace path-dependency
    # that is itself publishable becomes an edge dep -> dependent. A path
    # dependency on a non-publishable sibling (e.g. an examples crate) is
    # not a publish-ordering concern and is ignored.
    local dep deps_raw
    for n in "${names[@]}"; do
        deps_raw=$(jq -r --arg n "$n" '
            .packages[] | select(.name == $n) | .dependencies[]
            | select(.path != null) | .name
        ' <<<"$meta")
        while IFS= read -r dep; do
            [[ -z "$dep" ]] && continue
            # A crate may dev-depend on itself to switch a feature on for
            # its tests; that is not an ordering edge, and as one it is a
            # cycle nothing can break.
            [[ "$dep" == "$n" ]] && continue
            if [[ -n "${version[$dep]+x}" ]]; then
                adj[$dep]="${adj[$dep]} $n"
                indeg[$n]=$((indeg[$n] + 1))
            fi
        done <<<"$deps_raw"
    done

    # Kahn's algorithm. The frontier is re-sorted each pass so the order is
    # deterministic (alphabetical among crates at the same dependency tier)
    # rather than dependent on associative-array iteration order.
    local -a queue=() order=()
    for n in "${names[@]}"; do
        [[ "${indeg[$n]}" -eq 0 ]] && queue+=("$n")
    done

    local cur dependent
    while [[ ${#queue[@]} -gt 0 ]]; do
        mapfile -t queue < <(printf '%s\n' "${queue[@]}" | sort)
        cur="${queue[0]}"
        queue=("${queue[@]:1}")
        order+=("$cur")
        for dependent in ${adj[$cur]}; do
            indeg[$dependent]=$((indeg[$dependent] - 1))
            if [[ "${indeg[$dependent]}" -eq 0 ]]; then
                queue+=("$dependent")
            fi
        done
    done

    if [[ ${#order[@]} -ne ${#names[@]} ]]; then
        echo "Error: dependency cycle detected among publishable crates in $root" >&2
        exit 1
    fi

    local v
    for n in "${order[@]}"; do
        v="${version[$n]}"
        if version_published "$n" "$v"; then
            echo "  [skip]    $n $v (already on $REGISTRY_NAME)"
            continue
        fi

        if [[ "$publish" -eq 0 ]]; then
            echo "  [would publish] $n $v (not on $REGISTRY_NAME)"
            continue
        fi

        echo "  [publish] $n $v"
        if [[ "$REGISTRY_NAME" == "crates-io" ]]; then
            publish_crate "$root" "$n" "$v"
        else
            publish_other "$root" "$n" "$v"
        fi
        wait_for_index "$n" "$v"
    done
}

for root in "${roots[@]}"; do
    process_workspace "$root"
done

echo "Done."
