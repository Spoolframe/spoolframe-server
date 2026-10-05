#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo deny --locked --config deny.toml check licenses
cargo deny --locked --manifest-path crates/spoolframe-client-sdk/Cargo.toml --config deny-sdk.toml check licenses
# Discover the public crates from their license, so new workspace members
# cannot accidentally skip the stricter SDK boundary.
metadata=$(cargo metadata --locked --no-deps --format-version 1)
while IFS= read -r manifest; do
  cargo deny --locked --manifest-path "$manifest" --config deny-sdk.toml check licenses
done < <(jq -r '.workspace_members as $members | .packages[] | select(.id as $id | $members | index($id)) | select(.license == "MIT OR Apache-2.0" and .name != "spoolframe-client-sdk") | .manifest_path' <<< "$metadata")
