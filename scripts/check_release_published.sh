#!/usr/bin/env bash
set -euo pipefail

base=${1:?target branch commit required}
candidate=${2:-}
releases() {
  git log --first-parent --format=%H --extended-regexp \
    '--grep=^chore: release($| )' "$@" --
}

# A merge group may end with a release PR, but cannot merge another PR after it.
if [[ -n "$candidate" ]]; then
  queued=$(releases "$base..$candidate")
  if [[ -n "$queued" && "$queued" != "$candidate" ]]; then
    echo "::error::Queue the release PR without subsequent PRs, publish it, then requeue them."
    exit 1
  fi
fi

release=$(releases -1 "$base")
if [[ -z "$release" ]]; then
  exit 0
fi
# Older releases predate the publish workflow's completion tags.
if ! git cat-file -e "$release:scripts/check_release_published.sh" 2>/dev/null; then
  echo "Release predates the publication gate."
  exit 0
fi

if ! git ls-remote --exit-code --tags origin "refs/tags/release-published/$release"; then
  echo "::error::Release $release is awaiting publication. Run Publish release for the target branch, approve the release environment, then rerun failed CI jobs."
  exit 1
fi
