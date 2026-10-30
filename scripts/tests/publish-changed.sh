#!/usr/bin/env bash
# Tests for scripts/publish-changed.sh, on a fixture workspace and a fake
# sparse index served from a directory on loopback: the token variable each
# registry needs, the `publish` gate, the index taken from cargo's config,
# and which crates a dry run would publish, in order. And for
# scripts/publish-pr.sh, whose rewrite of a throwaway workspace is checked
# file by file. Nothing is published.
#
# Requires on PATH what the script does (cargo, curl, jq, date) and
# python3, for the index. Run from anywhere:
#   bash scripts/tests/publish-changed.sh

set -euo pipefail

script="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/publish-changed.sh"
pr_script="$(dirname "$script")/publish-pr.sh"
work=$(mktemp -d)
server_pid=""
cleanup() {
    [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null
    rm -rf "$work"
}
trap cleanup EXIT

failures=0
pass() { echo "ok   $1"; }
fail() {
    echo "FAIL $1" >&2
    failures=$((failures + 1))
}

# run ARGS...: the script's exit status in $status and its output (both
# streams) in $out, with no registry token or index in the environment.
run() {
    status=0
    out=$(env -u CARGO_REGISTRY_TOKEN -u CARGO_REGISTRIES_COLDSTEEL_TOKEN \
        -u CARGO_REGISTRIES_COLDSTEEL_INDEX "$@" 2>&1) || status=$?
}

# The fake index: alpha 1.0.0 is on it; beta is not.
index="$work/index"
mkdir -p "$index/al/ph"
echo '{"name":"alpha","vers":"1.0.0","deps":[],"cksum":"0","features":{},"yanked":false}' >"$index/al/ph/alpha"
python3 - "$index" "$work/port" <<'PY' &
import functools, http.server, os, sys

class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

server = http.server.HTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=sys.argv[1]))
with open(sys.argv[2] + ".tmp", "w") as f:
    f.write(str(server.server_port))
os.rename(sys.argv[2] + ".tmp", sys.argv[2])
server.serve_forever()
PY
server_pid=$!
for _ in $(seq 100); do
    [[ -s "$work/port" ]] && break
    sleep 0.1
done
port=$(cat "$work/port")

# fixture NAME PUBLISH: a workspace of alpha, beta (depending on alpha) and
# gamma (never published); alpha's `publish` line is PUBLISH, empty for
# none. Its .cargo/config.toml names the coldsteel registry at the index.
fixture() {
    local root="$work/$1" publish="$2" crate
    for crate in alpha beta gamma; do
        mkdir -p "$root/$crate/src"
        touch "$root/$crate/src/lib.rs"
    done
    mkdir -p "$root/.cargo"
    cat >"$root/.cargo/config.toml" <<TOML
[registries.coldsteel]
index = "sparse+http://127.0.0.1:$port/"
TOML
    cat >"$root/Cargo.toml" <<TOML
[workspace]
members = ["alpha", "beta", "gamma"]
resolver = "3"
TOML
    cat >"$root/alpha/Cargo.toml" <<TOML
[package]
name = "alpha"
version = "1.0.0"
edition = "2024"
$publish
TOML
    cat >"$root/beta/Cargo.toml" <<TOML
[package]
name = "beta"
version = "1.0.0"
edition = "2024"
publish = ["coldsteel", "crates-io"]

[dependencies]
alpha = { path = "../alpha", version = "1.0.0" }
TOML
    cat >"$root/gamma/Cargo.toml" <<TOML
[package]
name = "gamma"
version = "1.0.0"
edition = "2024"
publish = false
TOML
    echo "$root"
}

good=$(fixture good 'publish = ["crates-io", "coldsteel"]')

# Each registry's token, by the variable cargo reads it from.
run "$script" --publish --registry coldsteel "$good"
if [[ $status -ne 0 && "$out" == *"requires CARGO_REGISTRIES_COLDSTEEL_TOKEN"* ]]; then
    pass "coldsteel's token is CARGO_REGISTRIES_COLDSTEEL_TOKEN"
else
    fail "coldsteel's token is CARGO_REGISTRIES_COLDSTEEL_TOKEN: $out"
fi
run "$script" --publish --registry crates-io "$good"
if [[ $status -ne 0 && "$out" == *"requires CARGO_REGISTRY_TOKEN"* ]]; then
    pass "crates.io's token is CARGO_REGISTRY_TOKEN"
else
    fail "crates.io's token is CARGO_REGISTRY_TOKEN: $out"
fi

# A dry run reads the index from cargo's config, skips what is on it and
# would publish the rest, dependencies first, never a `publish = false`.
run "$script" --registry coldsteel "$good"
expected="  [skip]    alpha 1.0.0 (already on coldsteel)
  [would publish] beta 1.0.0 (not on coldsteel)"
selected=$(grep -E '^  \[' <<<"$out" || true)
if [[ $status -eq 0 && "$selected" == "$expected" ]]; then
    pass "a dry run skips what is on the index and would publish the rest, in order"
else
    fail "a dry run skips what is on the index and would publish the rest, in order: $out"
fi

# CARGO_REGISTRIES_COLDSTEEL_INDEX wins over the config, as for cargo.
run env CARGO_REGISTRIES_COLDSTEEL_INDEX="sparse+http://127.0.0.1:1/" \
    "$script" --registry coldsteel "$good"
if [[ $status -ne 0 && "$out" == *"127.0.0.1:1"* ]]; then
    pass "the index variable wins over the config"
else
    fail "the index variable wins over the config: $out"
fi

# A registry cargo's config does not name has no index.
run "$script" --registry elsewhere "$good"
if [[ $status -ne 0 && "$out" == *"no sparse index for registry elsewhere"* ]]; then
    pass "a registry with no index is refused"
else
    fail "a registry with no index is refused: $out"
fi

# The gate: crates.io alone, or no `publish` at all, stops the run.
for case in 'publish = ["crates-io"]' ''; do
    root=$(fixture "gate-${#case}" "$case")
    run "$script" --registry coldsteel "$root"
    if [[ $status -ne 0 && "$out" == *"alpha 1.0.0: publish = "* ]]; then
        pass "the gate refuses ${case:-no publish field}"
    else
        fail "the gate refuses ${case:-no publish field}: $out"
    fi
done

# Publishing runs with a cargo home of its own, with nothing replacing
# crates-io: one that does (the devenv shell's kellnr proxy) resolves a
# crate's dependencies through that proxy, whose index can lag crates.io
# for good. A fake cargo on PATH records the home and the arguments `cargo
# publish` gets and puts FAKE_VERS (1.0.0 unless set) of the `-p` crate on
# the index, as the registry would; the rest is cargo.
real_cargo=$(command -v cargo)
mkdir -p "$work/bin" "$work/home"
cat >"$work/home/config.toml" <<'TOML'
[source.crates-io]
replace-with = "mirror"

[source.mirror]
registry = "sparse+http://127.0.0.1:1/"
TOML
cat >"$work/bin/cargo" <<SH
#!/usr/bin/env bash
if [[ "\$1" == publish ]]; then
    { echo "home \$CARGO_HOME"; cat "\$CARGO_HOME/config.toml" 2>/dev/null; echo "args \$*"; } >>"$work/published"
    name=\$(sed -E 's/.* -p ([^ ]+).*/\1/' <<<"\$*")
    mkdir -p "$index/\${name:0:2}/\${name:2:2}"
    echo "{\"name\":\"\$name\",\"vers\":\"\${FAKE_VERS:-1.0.0}\",\"deps\":[],\"cksum\":\"0\",\"features\":{},\"yanked\":false}" >>"$index/\${name:0:2}/\${name:2:2}/\$name"
    exit 0
fi
exec "$real_cargo" "\$@"
SH
chmod +x "$work/bin/cargo"
run env PATH="$work/bin:$PATH" CARGO_HOME="$work/home" CARGO_REGISTRIES_COLDSTEEL_TOKEN=t \
    "$script" --publish --registry coldsteel "$good"
published=$(cat "$work/published" 2>/dev/null || true)
if [[ $status -eq 0 && "$published" == "home "* && "$published" != "home $work/home"* \
    && "$published" != *replace-with* ]]; then
    pass "cargo publish runs with a home that replaces nothing"
else
    fail "cargo publish runs with a home that replaces nothing: $published $out"
fi

# A clean tree stays required unless the caller says otherwise.
if [[ "$published" != *--allow-dirty* ]]; then
    pass "a publish does not allow a dirty tree by default"
else
    fail "a publish does not allow a dirty tree by default: $published"
fi

# --allow-dirty reaches cargo, for the registry a PR build goes to.
rm -f "$work/published" "$index/be/ta/beta"
run env PATH="$work/bin:$PATH" CARGO_REGISTRIES_COLDSTEEL_TOKEN=t \
    "$script" --publish --allow-dirty --registry coldsteel "$good"
published=$(cat "$work/published" 2>/dev/null || true)
if [[ $status -eq 0 && "$published" == *"--allow-dirty"* ]]; then
    pass "--allow-dirty reaches cargo publish"
else
    fail "--allow-dirty reaches cargo publish: $published $out"
fi

# Nothing dirty ever goes to crates.io: refused before anything is read.
rm -f "$work/published" "$index/be/ta/beta"
run env PATH="$work/bin:$PATH" CARGO_REGISTRY_TOKEN=t \
    "$script" --publish --allow-dirty --registry crates-io "$good"
if [[ $status -ne 0 && "$out" == *"--allow-dirty is not for crates-io"* && ! -e "$work/published" ]]; then
    pass "--allow-dirty is refused for crates.io"
else
    fail "--allow-dirty is refused for crates.io: $out"
fi

# pr_fixture NAME VERSION: a committed workspace of alpha, beta (depending on
# alpha) and gamma (never published, depending on both), at VERSION, laid
# out as this repository's: versions in [workspace.package], siblings in
# [workspace.dependencies]. It is a git repository: the build's version
# comes from its commit.
pr_fixture() {
    local root="$work/$1" version="$2" crate
    for crate in alpha beta gamma; do
        mkdir -p "$root/$crate/src"
        touch "$root/$crate/src/lib.rs"
    done
    mkdir -p "$root/.cargo"
    cat >"$root/.cargo/config.toml" <<TOML
[registries.coldsteel]
index = "sparse+http://127.0.0.1:$port/"
TOML
    cat >"$root/Cargo.toml" <<TOML
[workspace]
members = ["alpha", "beta", "gamma"]
resolver = "3"

[workspace.package]
version = "$version"
edition = "2024"

[workspace.dependencies]
# version = "$version" in a comment stays.
alpha = { path = "alpha", version = "$version" }
beta = { path = "beta", version = "$version", default-features = false }
gamma = { path = "gamma" }

[profile.release]
debug = false
TOML
    cat >"$root/alpha/Cargo.toml" <<TOML
[package]
name = "alpha"
version.workspace = true
edition.workspace = true
publish = ["crates-io", "coldsteel"]
TOML
    cat >"$root/beta/Cargo.toml" <<TOML
[package]
name = "beta"
version.workspace = true
edition.workspace = true
publish = ["crates-io", "coldsteel"]

[dependencies]
alpha = { workspace = true }
TOML
    cat >"$root/gamma/Cargo.toml" <<TOML
[package]
name = "gamma"
version.workspace = true
edition.workspace = true
publish = false

[dependencies]
alpha = { workspace = true }
beta = { workspace = true }
TOML
    (
        cd "$root"
        cargo generate-lockfile --offline -q
        git init -q
        git add -A
        git -c user.name=t -c user.email=t@t commit -q -m fixture
    )
    echo "$root"
}

pr=$(pr_fixture pr 1.0.0-beta.1)
sha=$(git -C "$pr" rev-parse --short=7 HEAD)
vers="1.0.0-beta.1.g$sha"

# A dry run: the tree is rewritten, the crates are only reported.
run "$pr_script" "$pr"
expected="  [would publish] alpha $vers (not on coldsteel)
  [would publish] beta $vers (not on coldsteel)"
selected=$(grep -E '^  \[' <<<"$out" || true)
if [[ $status -eq 0 && "$selected" == "$expected" ]]; then
    pass "a PR build's version is the base and the short sha, on every published crate"
else
    fail "a PR build's version is the base and the short sha, on every published crate: $out"
fi
if [[ $status -eq 0 && "$out" != *"::notice"* ]]; then
    pass "a dry run announces nothing"
else
    fail "a dry run announces nothing: $out"
fi

# The rewrite, line by line. `=`: a caret requirement on a pre-release takes
# any later pre-release of that version, another commit's sibling.
if grep -qxF "alpha = { path = \"alpha\", version = \"=$vers\", registry = \"coldsteel\" }" "$pr/Cargo.toml"; then
    pass "a sibling gets the exact version and the registry"
else
    fail "a sibling gets the exact version and the registry: $(cat "$pr/Cargo.toml")"
fi
if grep -qxF "beta = { path = \"beta\", version = \"=$vers\", registry = \"coldsteel\", default-features = false }" "$pr/Cargo.toml"; then
    pass "a sibling keeps its other keys"
else
    fail "a sibling keeps its other keys: $(cat "$pr/Cargo.toml")"
fi
if grep -qxF 'gamma = { path = "gamma" }' "$pr/Cargo.toml" \
    && grep -qxF '# version = "1.0.0-beta.1" in a comment stays.' "$pr/Cargo.toml" \
    && grep -qxF "version = \"$vers\"" "$pr/Cargo.toml"; then
    pass "the rewrite touches the workspace version and published siblings alone"
else
    fail "the rewrite touches the workspace version and published siblings alone: $(cat "$pr/Cargo.toml")"
fi

# `cargo publish --locked` needs the lock to match.
if grep -qxF "version = \"$vers\"" "$pr/Cargo.lock" \
    && (cd "$pr" && cargo metadata --locked --offline --format-version 1 >/dev/null 2>&1); then
    pass "the lock is refreshed"
else
    fail "the lock is refreshed"
fi

# Publishing: to Coldsteel, on a tree it knows is dirty, and it says what.
pr=$(pr_fixture pr-publish 1.0.0-beta.1)
sha=$(git -C "$pr" rev-parse --short=7 HEAD)
vers="1.0.0-beta.1.g$sha"
rm -f "$work/published"
run env PATH="$work/bin:$PATH" CARGO_REGISTRIES_COLDSTEEL_TOKEN=t FAKE_VERS="$vers" \
    "$pr_script" --publish "$pr"
published=$(grep '^args ' "$work/published" 2>/dev/null || true)
expected="args publish --registry coldsteel -p alpha --locked --no-verify --allow-dirty
args publish --registry coldsteel -p beta --locked --no-verify --allow-dirty"
if [[ $status -eq 0 && "$published" == "$expected" && "$out" == *"::notice"*"$vers"* ]]; then
    pass "a PR build publishes to Coldsteel alone and names the version"
else
    fail "a PR build publishes to Coldsteel alone and names the version: $published $out"
fi

# A release base has no pre-release to extend, so the sha starts one.
pr=$(pr_fixture pr-release 1.0.0)
sha=$(git -C "$pr" rev-parse --short=7 HEAD)
run "$pr_script" "$pr"
if [[ $status -eq 0 && "$out" == *"[would publish] alpha 1.0.0-g$sha "* ]]; then
    pass "a PR build of a release version is still valid semver"
else
    fail "a PR build of a release version is still valid semver: $out"
fi

# A sibling declared where the rewrite does not reach would publish against
# crates.io: the script stops rather than publish half a graph.
pr=$(pr_fixture pr-missed 1.0.0-beta.1)
sed -i 's/^alpha = { workspace = true }$/alpha = { path = "..\/alpha", version = "1.0.0-beta.1" }/' "$pr/beta/Cargo.toml"
run "$pr_script" "$pr"
if [[ $status -ne 0 && "$out" == *"beta -> alpha"* && "$out" != *"[would publish]"* ]]; then
    pass "a sibling the rewrite missed stops the run"
else
    fail "a sibling the rewrite missed stops the run: $out"
fi

if [[ $failures -gt 0 ]]; then
    echo "$failures failed" >&2
    exit 1
fi
echo "all passed"
