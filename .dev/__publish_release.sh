#!/usr/bin/env bash
# Package the musl release binary and publish it to GitHub Releases.
# Usage: __publish_release.sh <branch|tag> <ref-name> <sha>
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

ref_type="${1:?ref type}"
ref_name="${2:?ref name}"
sha="${3:?sha}"
target=x86_64-unknown-linux-musl
name="pomb-$target"

mkdir -p dist
cp "target/$target/release/pomb" "dist/$name"
file "dist/$name"
file "dist/$name" | grep -Eq 'statically linked|static-pie linked' || {
	echo "dist/$name is not statically linked (static or static-pie)" >&2
	exit 1
}
(cd dist && sha256sum "$name" >"$name.sha256")

assets=("dist/$name" "dist/$name.sha256")

if [[ $ref_type == tag ]]; then
	if gh release view "$ref_name" >/dev/null 2>&1; then
		gh release upload "$ref_name" "${assets[@]}" --clobber
	else
		gh release create "$ref_name" "${assets[@]}" --title "$ref_name" --verify-tag --notes "commit: $sha"
	fi
	exit 0
fi

if [[ $(git ls-remote origin refs/heads/main | cut -f1) != "$sha" ]]; then
	echo "main moved past $sha; not publishing nightly"
	exit 0
fi

git tag -f nightly "$sha"
git push -f origin refs/tags/nightly
if gh release view nightly >/dev/null 2>&1; then
	gh release edit nightly --prerelease --target "$sha" --notes "commit: $sha"
	gh release upload nightly "${assets[@]}" --clobber
else
	gh release create nightly "${assets[@]}" --prerelease --title nightly --target "$sha" --notes "commit: $sha"
fi
