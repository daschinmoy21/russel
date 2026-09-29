#!/usr/bin/env bash
# Upload a locally built microVM kernel to a tag's release.
#
# The Release workflow builds the binaries and dashboard on a tag push and
# leaves a draft release. The kernel is not built there (linux from source
# takes ~2h on hosted runners). This script builds `.#microvm-kernel` from the
# tag's commit (a no-op when the store path is already local), uploads it as
# russel-kernel-<tag>-x86_64.bzImage, and adds its line to the release
# SHA256SUMS that contrib/install.sh verifies against. The kernel is GPL-2.0,
# so it also uploads the corresponding source (`.#microvm-kernel-source`:
# upstream tarball, patches, .config, build expressions) as
# russel-kernel-<tag>-source.tar. --publish then takes the release out of
# draft once every asset, including the source, is present.
#
#   contrib/release-kernel.sh v0.1.0 [--repo OWNER/NAME] [--publish]
set -euo pipefail

usage() {
  echo "usage: $0 TAG [--repo OWNER/NAME] [--publish]" >&2
  exit 2
}

tag=""
repo="daschinmoy21/russel"
publish=0
while (($#)); do
  case $1 in
    --repo)
      [[ $# -ge 2 ]] || usage
      repo=$2
      shift 2
      ;;
    --publish)
      publish=1
      shift
      ;;
    -*) usage ;;
    *)
      [[ -z "$tag" ]] || usage
      tag=$1
      shift
      ;;
  esac
done
[[ -n "$tag" ]] || usage

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
asset="russel-kernel-${tag}-x86_64.bzImage"
source_asset="russel-kernel-${tag}-source.tar"

if ! rev="$(git -C "$root" rev-parse --verify --quiet "${tag}^{commit}")"; then
  echo "tag ${tag} is not in ${root}: fetch it so the kernel matches the tagged tree" >&2
  exit 1
fi
if ! gh release view "$tag" -R "$repo" >/dev/null; then
  echo "no release ${tag} in ${repo}: push the tag and let the Release workflow create the draft" >&2
  exit 1
fi
# The workflow built the binaries from the remote tag. A stale or retagged
# local tag would pair them with a kernel from another tree.
remote_rev="$(gh api "repos/${repo}/commits/${tag}" -q .sha)"
if [[ "$remote_rev" != "$rev" ]]; then
  echo "local ${tag} is ${rev} but ${repo} has ${remote_rev}: fetch the tag first" >&2
  exit 1
fi
# The Release workflow also rewrites SHA256SUMS; wait for it instead of racing.
running="$(gh run list -R "$repo" --workflow release.yml --branch "$tag" \
  --json status -q '[.[] | select(.status != "completed")] | length')"
if [[ "$running" != 0 ]]; then
  echo "the Release workflow for ${tag} is still running in ${repo}; rerun once it finishes" >&2
  exit 1
fi

# The asset is x86_64 whatever the build host is; never pick the host system's
# package by default.
echo "building microvm-kernel (x86_64-linux) at ${tag} (${rev})"
flake="git+file://${root}?rev=${rev}"
kernel="$(nix build "${flake}#packages.x86_64-linux.microvm-kernel" \
  --no-link --print-out-paths)"
echo "kernel: ${kernel}"
# GPL-2.0 section 3: ship the source the bzImage was built from next to it.
kernel_source="$(nix build "${flake}#packages.x86_64-linux.microvm-kernel-source" \
  --no-link --print-out-paths)"
echo "kernel source: ${kernel_source}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
install -m 0644 "${kernel}/bzImage" "${work}/${asset}"
# Stub files are a few KB; the virtio-builtin bzImage is tens of MB.
size="$(wc -c <"${work}/${asset}")"
if ((size < 1000000)); then
  echo "kernel is ${size} bytes; expected a real bzImage" >&2
  exit 1
fi
install -m 0644 "$kernel_source" "${work}/${source_asset}"
source_list="$(tar -tf "${work}/${source_asset}")"
for want in 'config' 'linux-[^/]*\.tar\.xz' 'build/russel/nix/microvm-kernel\.nix'; do
  if ! grep -qE "^[^/]+/${want}\$" <<<"$source_list"; then
    echo "${source_asset} has no ${want}; expected the kernel source bundle" >&2
    exit 1
  fi
done

gh release download "$tag" -R "$repo" -p SHA256SUMS -D "$work"
# Replace any earlier kernel lines so a re-upload never leaves two entries.
{
  grep -v ' russel-kernel-' "${work}/SHA256SUMS" || true
  (cd "$work" && sha256sum "$asset" "$source_asset")
} >"${work}/SHA256SUMS.new"
mv "${work}/SHA256SUMS.new" "${work}/SHA256SUMS"
cat "${work}/SHA256SUMS"

# Assets first: until SHA256SUMS lands the installer rejects the unlisted asset
# instead of trusting it.
gh release upload "$tag" -R "$repo" --clobber "${work}/${source_asset}" "${work}/${asset}"
gh release upload "$tag" -R "$repo" --clobber "${work}/SHA256SUMS"

# Read back what landed: a concurrent SHA256SUMS writer would drop our lines.
gh release download "$tag" -R "$repo" -p SHA256SUMS -O "${work}/SHA256SUMS.remote" --clobber
for name in "$asset" "$source_asset"; do
  want_line="$(grep -F "  ${name}" "${work}/SHA256SUMS")"
  if ! grep -qxF "$want_line" "${work}/SHA256SUMS.remote"; then
    echo "SHA256SUMS in ${repo} ${tag} lost the ${name} line; rerun $0" >&2
    exit 1
  fi
done

if ((publish)); then
  assets="$(gh release view "$tag" -R "$repo" --json assets -q '.assets[].name')"
  for want in "russel-${tag}-x86_64" "russel-ctrl-${tag}-x86_64" \
    "russel-ctrl-${tag}.service" "russel-dashboard-${tag}.tar.gz" \
    "$asset" "$source_asset" LICENSE NOTICE; do
    if ! grep -qxF "$want" <<<"$assets"; then
      echo "not publishing: ${want} is missing from ${tag}" >&2
      exit 1
    fi
    # The installer rejects any asset without a checksum line.
    if ! grep -qF "  ${want}" "${work}/SHA256SUMS.remote"; then
      echo "not publishing: ${want} has no SHA256SUMS line" >&2
      exit 1
    fi
  done
  if ! grep -qxF SHA256SUMS <<<"$assets"; then
    echo "not publishing: SHA256SUMS is missing from ${tag}" >&2
    exit 1
  fi
  gh release edit "$tag" -R "$repo" --draft=false
  echo "published ${tag} in ${repo}"
else
  echo "uploaded; publish with: $0 ${tag} --repo ${repo} --publish"
fi
