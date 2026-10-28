#!/usr/bin/env bash
# Builds the release images (nix/docker.nix) and pushes them to ghcr, by
# hand after a release: nothing in CI pushes them.
#
#   scripts/push-images.sh              amd64 and arm64
#   scripts/push-images.sh amd64        one architecture
#
# Each architecture's image goes up under its own tag (`<ver>-amd64`,
# `<ver>-slim-arm64`, …), then `<ver>` and `<ver>-slim` become manifest
# lists over them, so `docker pull` picks the host's. `-nvidia` and its
# `-cpu` alias are amd64 only: NVIDIA's container toolkit image is built
# for x86-64 here. Every tag also gets its exact-build twin, the commit's
# first 8 hex digits after it (`<ver>-slim-0fc3bbeb`), which never moves.
#
# arm64 needs a builder for aarch64-linux: a remote one, or this host
# emulating it (boot.binfmt.emulatedSystems), which takes hours.
# Pushing needs `docker login ghcr.io`; skopeo reads the same login.
set -euo pipefail
cd "$(dirname "$0")/.."

repo=ghcr.io/spoolframe/spoolframe-server
if [ $# -eq 0 ]; then
  set -- amd64 arm64
fi
archs=("$@")

if [ -n "$(git status --porcelain)" ]; then
  echo "push-images: the tree has changes; push images from a commit" >&2
  exit 1
fi
version=$(nix eval --raw .#packages.x86_64-linux.default.version)
rev=$(git rev-parse --short=8 HEAD)
# skopeo refuses to copy without a trust policy (containers' policy.json),
# which a host without podman doesn't have. The source is the image this
# script just built, so there is nothing to verify: accept it.
skopeo=(nix run nixpkgs#skopeo -- --insecure-policy)

system_of() {
  case "$1" in
    amd64) echo x86_64-linux ;;
    arm64) echo aarch64-linux ;;
    *) echo "push-images: unknown architecture $1" >&2; exit 1 ;;
  esac
}

# push <flake package> <tag>: build its streaming script, push the stream.
push() {
  local out
  out=$(nix build --no-link --print-out-paths ".#packages.$1")
  "$out" | "${skopeo[@]}" copy --quiet docker-archive:/dev/stdin "docker://$repo:$2"
  echo "pushed $repo:$2"
}

# list <tag> <arch>...: <tag> becomes a manifest list over <tag>-<arch>.
list() {
  local tag=$1
  shift
  docker manifest rm "$repo:$tag" >/dev/null 2>&1 || true
  docker manifest create "$repo:$tag" "${@/#/$repo:$tag-}" >/dev/null
  docker manifest push --purge "$repo:$tag" >/dev/null
  echo "listed $repo:$tag over $*"
}

for arch in "${archs[@]}"; do
  system=$(system_of "$arch")
  push "$system.docker" "$version-$arch"
  push "$system.docker" "$version-$rev-$arch"
  push "$system.docker-slim" "$version-slim-$arch"
  push "$system.docker-slim" "$version-slim-$rev-$arch"
done

for tag in "$version" "$version-$rev" "$version-slim" "$version-slim-$rev"; do
  list "$tag" "${archs[@]}"
done

if [[ " ${archs[*]} " == *" amd64 "* ]]; then
  for tag in "$version-nvidia" "$version-nvidia-$rev" "$version-cpu" "$version-cpu-$rev"; do
    push x86_64-linux.docker-nvidia "$tag"
  done
fi
