#!/usr/bin/env bash
# Bumps the version, tags and pushes; release.yml publishes the tag once CI passes its commit.
# Usage: ./scripts/tag-release.sh major|minor|patch|v1.2.3
set -euo pipefail

usage() {
  echo "Usage: $0 major|minor|patch|vX.Y.Z" >&2
  exit 1
}

[[ $# -ne 1 ]] && usage

INPUT="$1"
BRANCH=""

ensure_release_branch_current() {
  BRANCH=$(git symbolic-ref --short HEAD 2>/dev/null || echo "")
  if [[ -z "$BRANCH" ]]; then
    echo "Error: must be on a branch, not detached HEAD." >&2
    exit 1
  fi

  if git diff --quiet && git diff --cached --quiet; then
    git pull --ff-only origin "$BRANCH"
  else
    git fetch origin "$BRANCH"
    if ! git merge-base --is-ancestor "refs/remotes/origin/${BRANCH}" HEAD; then
      echo "Error: ${BRANCH} is behind or diverged from origin/${BRANCH}." >&2
      echo "Pull/rebase before including local changes in a release." >&2
      exit 1
    fi
  fi
}

verify_release_branch_still_current() {
  git fetch origin "$BRANCH"
  if ! git merge-base --is-ancestor "refs/remotes/origin/${BRANCH}" HEAD; then
    echo "Error: origin/${BRANCH} changed while preparing the release." >&2
    echo "Pull/rebase, rerun the release script, and create the tag from the updated branch." >&2
    exit 1
  fi
}

commit_remaining_changes() {
  if git diff --quiet && git diff --cached --quiet; then
    return
  fi

  echo "Uncommitted changes remain:"
  git status --short
  echo

  read -r -p "Include these changes in a release prep commit? [y/N] " INCLUDE_CHANGES
  case "${INCLUDE_CHANGES}" in
    [Yy]|[Yy][Ee][Ss])
      read -r -p "Commit message [chore: prepare release]: " RELEASE_COMMIT_MESSAGE
      RELEASE_COMMIT_MESSAGE="${RELEASE_COMMIT_MESSAGE:-chore: prepare release}"
      git add -A
      git commit -m "$RELEASE_COMMIT_MESSAGE"
      ;;
    *)
      echo "Aborted. Commit, include, or stash changes before releasing." >&2
      exit 1
      ;;
  esac
}

ensure_release_branch_current
commit_remaining_changes

# Re-check origin before tagging so concurrent updates fail without leaving an unpushable tag.
verify_release_branch_still_current

CURRENT=$(git describe --tags --abbrev=0 --match 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null || echo "v0.0.0")
CURRENT_CLEAN="${CURRENT#v}"
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT_CLEAN"

VERSION=""
case "$INPUT" in
  major)
    MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
  minor)
    MINOR=$((MINOR + 1)); PATCH=0 ;;
  patch)
    PATCH=$((PATCH + 1)) ;;
  v[0-9]*.[0-9]*.[0-9]*)
    VERSION="$INPUT" ;;
  [0-9]*.[0-9]*.[0-9]*)
    VERSION="v$INPUT" ;;
  *)
    echo "Error: unrecognised argument '$INPUT'" >&2
    usage ;;
esac

[[ -z "$VERSION" ]] && VERSION="v${MAJOR}.${MINOR}.${PATCH}"

if git rev-parse -q --verify "refs/tags/${VERSION}" >/dev/null; then
  echo "Error: local tag ${VERSION} already exists." >&2
  exit 1
fi

if git ls-remote --exit-code --tags origin "refs/tags/${VERSION}" >/dev/null 2>&1; then
  echo "Error: remote tag ${VERSION} already exists." >&2
  exit 1
fi

echo "Current tag : ${CURRENT}"
echo "New tag     : ${VERSION}"
echo "Branch      : ${BRANCH}"
echo

read -r -p "Tag ${BRANCH} at ${VERSION} and push branch+tag? [y/N] " CONFIRM
case "${CONFIRM}" in
  [Yy]|[Yy][Ee][Ss]) ;;
  *) echo "Aborted."; exit 0 ;;
esac

RELEASE_WORKSPACES=(rust rust/crates/dstreamy-obs)
RELEASE_LOCKFILES=("${RELEASE_WORKSPACES[@]}" rust/fuzz)

set_version() {
  local version=$1 workspace spec
  for workspace in "${RELEASE_WORKSPACES[@]}"; do
    sed -i.bak -E '/^\[workspace\.package\]/,/^\[/ s/^version = "[^"]*"/version = "'"$version"'"/' "$workspace/Cargo.toml"
    rm "$workspace/Cargo.toml.bak"
    grep -q "^version = \"$version\"" "$workspace/Cargo.toml" || { echo "Error: could not set the version in $workspace/Cargo.toml" >&2; exit 1; }
    git add "$workspace/Cargo.toml"
  done
  # The plugin and fuzz lockfiles also pin root workspace crates, so they update after both manifests.
  for workspace in "${RELEASE_LOCKFILES[@]}"; do
    (cd "$workspace" && cargo update --workspace --quiet)
    git add "$workspace/Cargo.lock"
  done
  spec=$(jq --indent 4 --arg v "$version" '.version = $v' obs/buildspec.json)
  printf '%s\n' "$spec" > obs/buildspec.json
  git add obs/buildspec.json
}

# The release workflow publishes only when the tag equals dstreamy-cli's version and the plugin's.
set_version "${VERSION#v}"
if ! git diff --cached --quiet; then
  git commit -m "chore: release ${VERSION}"
fi

pushed_already() {
  local tag branch
  tag=$(git ls-remote --exit-code origin "refs/tags/${VERSION}" | cut -f1) || return 1
  branch=$(git ls-remote --exit-code origin "refs/heads/${BRANCH}" | cut -f1) || return 1
  [[ $tag == "$(git rev-parse "refs/tags/${VERSION}")" && $branch == "$(git rev-parse HEAD)" ]]
}

git tag -a "${VERSION}" -m "Release ${VERSION}"
if ! git push --atomic origin "${BRANCH}" "${VERSION}"; then
  echo >&2
  if pushed_already; then
    echo "git reported a failed push, but origin has ${BRANCH} and ${VERSION} at the release commit; treating it as pushed." >&2
  elif ! git ls-remote origin >/dev/null 2>&1 || git ls-remote --exit-code origin "refs/tags/${VERSION}" >/dev/null 2>&1; then
    echo "Error: the push failed and origin's state is unclear; the local tag ${VERSION} is kept." >&2
    echo "If origin has ${VERSION} on the release commit, the release is pushed. Otherwise run" >&2
    echo "'git tag -d ${VERSION}' and rerun $0 ${INPUT}." >&2
    exit 1
  else
    git tag -d "${VERSION}" >/dev/null
    echo "Error: origin rejected the push of ${BRANCH} and ${VERSION}; neither ref was updated." >&2
    echo "The local tag ${VERSION} was deleted. The release commit stays on ${BRANCH}." >&2
    echo "${BRANCH} must accept a direct push from you. Fix the cause and rerun $0 ${INPUT}." >&2
    exit 1
  fi
fi

REPO=$(git remote get-url origin | sed -E 's#.*github\.com[:/](.*)$#\1#; s#\.git$##')
echo
echo "✓ Pushed ${VERSION}."
echo "  The release workflow will publish it automatically once CI passes on this tagged commit."
echo "  https://github.com/${REPO}/actions"
