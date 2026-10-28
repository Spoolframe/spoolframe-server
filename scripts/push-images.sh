#!/usr/bin/env bash
# Builds the release images (nix/docker.nix) and pushes them to ghcr: by
# hand after a release, or from GitHub's images workflow
# (.github/workflows/images.yaml).
#
#   scripts/push-images.sh              amd64 and arm64
#   scripts/push-images.sh amd64        one architecture
#   scripts/push-images.sh --if-missing --no-list arm64
#   scripts/push-images.sh --list-only amd64 arm64
#
# Each architecture's image goes up under its own tag (`<ver>-amd64`,
# `<ver>-slim-arm64`, …), then `<ver>` and `<ver>-slim` become manifest
# lists over them, so `docker pull` picks the host's. `-nvidia` and its
# `-cpu` alias are amd64 only: NVIDIA's container toolkit image is built
# for x86-64 here. Every tag also gets its exact-build twin, the commit's
# first 8 hex digits after it (`<ver>-slim-0fc3bbeb`), which never moves.
# The lists also make `<major>.<minor>` (`0.16`, `0.16-slim`, …) and
# `latest` follow the version pushed; --no-latest keeps `latest` where it
# is, for a patch to an older line. A prerelease (`0.16.3-rc.1`) gets its
# own tags only.
#
# --if-missing skips an architecture whose `<ver>-<arch>` is already on
# ghcr, so a run for a version that is out does not build it again.
# --no-list pushes the architectures' tags and stops; --list-only makes
# the lists and pushes nothing. The workflow pushes each architecture on
# its own runner, then lists once over both.
#
# arm64 needs a builder for aarch64-linux: GitHub's arm64 runners, a
# remote one, or this host emulating it (boot.binfmt.emulatedSystems),
# which takes hours. Pushing needs `docker login ghcr.io`; skopeo reads
# the same login.
set -euo pipefail
cd "$(dirname "$0")/.."

repo=ghcr.io/spoolframe/spoolframe-server
if_missing=false
do_push=true
do_list=true
move_latest=true
archs=()
for arg in "$@"; do
  case "$arg" in
    --if-missing) if_missing=true ;;
    --no-list) do_list=false ;;
    --list-only) do_push=false ;;
    --no-latest) move_latest=false ;;
    -*) echo "push-images: unknown option $arg" >&2; exit 1 ;;
    *) archs+=("$arg") ;;
  esac
done
if [ ${#archs[@]} -eq 0 ]; then
  archs=(amd64 arm64)
fi

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

# published <tag>: whether <tag> is on ghcr.
published() {
  "${skopeo[@]}" inspect --raw "docker://$repo:$1" >/dev/null 2>&1
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
  list_as "$1" "$@"
}

# list_as <name> <tag> <arch>...: <name> becomes a manifest list over
# <tag>-<arch>.
list_as() {
  local name=$1 tag=$2
  shift 2
  docker manifest rm "$repo:$name" >/dev/null 2>&1 || true
  docker manifest create "$repo:$name" "${@/#/$repo:$tag-}" >/dev/null
  docker manifest push --purge "$repo:$name" >/dev/null
  echo "listed $repo:$name over $tag: $*"
}

# retag <tag> <name>: <name> points at what <tag> does, copied inside ghcr.
retag() {
  "${skopeo[@]}" copy --quiet --all "docker://$repo:$1" "docker://$repo:$2"
  echo "tagged $repo:$2 as $1"
}

if $do_push; then
  for arch in "${archs[@]}"; do
    system=$(system_of "$arch")
    if $if_missing && published "$version-$arch"; then
      echo "skipped $arch: $repo:$version-$arch is already published"
      continue
    fi
    push "$system.docker" "$version-$arch"
    push "$system.docker" "$version-$rev-$arch"
    push "$system.docker-slim" "$version-slim-$arch"
    push "$system.docker-slim" "$version-slim-$rev-$arch"
    if [ "$arch" = amd64 ]; then
      for tag in "$version-nvidia" "$version-nvidia-$rev" "$version-cpu" "$version-cpu-$rev"; do
        push x86_64-linux.docker-nvidia "$tag"
      done
    fi
  done
fi

if $do_list; then
  for tag in "$version" "$version-slim"; do
    list "$tag" "${archs[@]}"
  done
  # An architecture skipped by --if-missing was pushed from another commit,
  # so this commit's exact tags may not exist for every architecture. Its
  # exact lists are then left alone: the earlier commit's still stand.
  for tag in "$version-$rev" "$version-slim-$rev"; do
    for arch in "${archs[@]}"; do
      if ! published "$tag-$arch"; then
        echo "not listed $repo:$tag: $repo:$tag-$arch is not published"
        continue 2
      fi
    done
    list "$tag" "${archs[@]}"
  done
  # The release lines' tags: `<major>.<minor>` and `latest` (with -slim,
  # -nvidia and -cpu) move to this version. Pushing an older line's patch
  # would move `latest` back, so --no-latest leaves it alone. A release
  # candidate (`0.16.3-rc.1`) moves neither: it is pulled by its own tag,
  # and `0.16` stays on the line's last release.
  lines=()
  if [[ $version == *-* ]]; then
    echo "not moved $repo:${version%.*} and latest: $version is a prerelease"
  else
    lines+=("${version%.*}")
    $move_latest && lines+=(latest)
  fi
  for line in "${lines[@]}"; do
    list_as "$line" "$version" "${archs[@]}"
    list_as "$line-slim" "$version-slim" "${archs[@]}"
    if [[ " ${archs[*]} " == *" amd64 "* ]]; then
      retag "$version-nvidia" "$line-nvidia"
      retag "$version-cpu" "$line-cpu"
    fi
  done
fi
