#!/usr/bin/env bash
# Prints the version to report in builds: the release tag without its leading "v" on tag builds, otherwise the version
# in Cargo.toml with a "-dev" suffix.
set -euo pipefail
if [[ "${GITHUB_REF:-}" == refs/tags/v* ]]; then
  echo "${GITHUB_REF_NAME#v}"
else
  echo "$(sed -n 's/^version = "\(.*\)"/\1/p' "$(dirname "$0")/../../Cargo.toml" | head -1)-dev"
fi
