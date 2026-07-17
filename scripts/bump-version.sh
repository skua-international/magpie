#!/usr/bin/env bash
# Called by release.config.js's @semantic-release/exec `prepareCmd`, with
# the new semver (no leading "v") as $1. Every crate in the Cargo
# workspace inherits its version from [workspace.package] (`version.workspace
# = true`), so bumping is one edit to the root manifest, not one per crate
# -- see Cargo.toml's own comment on why. Also bumps generated/ts's
# package.json, the one other artifact in the repo with its own version
# field. @semantic-release/git then commits whatever this touches, and
# @semantic-release/github tags the resulting commit.
set -euo pipefail

VERSION="${1:?usage: bump-version.sh X.Y.Z}"

sed -i -E "s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"\$/version = \"${VERSION}\"/" Cargo.toml

python3 - "$VERSION" <<'EOF'
import json
import sys

path = "generated/ts/package.json"
with open(path) as f:
    data = json.load(f)
data["version"] = sys.argv[1]
with open(path, "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
EOF

# Regenerates Cargo.lock's per-member version fields to match -- any cargo
# invocation that touches the lockfile does this; check is the cheapest
# one that still resolves every workspace member.
cargo check --workspace

echo "Bumped to ${VERSION}"
