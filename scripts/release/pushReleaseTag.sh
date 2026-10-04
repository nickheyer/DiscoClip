#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat << 'EOF'
pushReleaseTag.sh              # patch: v1.0.8 -> v1.0.9
pushReleaseTag.sh --minor      # v1.0.8 -> v1.1.0
pushReleaseTag.sh --major      # v1.0.8 -> v2.0.0
pushReleaseTag.sh --set 2.5.0  # exactly v2.5.0
pushReleaseTag.sh --dry-run    # print the plan, touch nothing
EOF
}

BUMP='patch'
SET=''
DRY=false
while [ $# -gt 0 ]; do
  case "$1" in
    --major | major) BUMP=major ;;
    --minor | minor) BUMP=minor ;;
    --patch | patch) BUMP='patch' ;;
    --set)
      SET="${2:?--set needs a version}"
      shift
      ;;
    --dry-run | -n) DRY=true ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "pushReleaseTag: unknown argument '$1'" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

cd "$(git rev-parse --show-toplevel)"
git symbolic-ref -q HEAD > /dev/null || {
  echo "pushReleaseTag: detached HEAD - check out a branch first" >&2
  exit 1
}

git fetch --tags --quiet origin || echo "warning: could not fetch tags from origin; using local tags" >&2

BASE=$(git tag --list 'v[0-9]*' | sed 's/^v//' | sort -V | tail -1)
BASE=${BASE:-0.0.0}
BASE=${BASE%%-*}
[[ $BASE =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "pushReleaseTag: highest tag v$BASE is not X.Y.Z" >&2
  exit 1
}

if [ -n "$SET" ]; then
  NEW=$SET
  [[ $NEW =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
    echo "pushReleaseTag: --set '$SET' is not X.Y.Z" >&2
    exit 1
  }
else
  IFS=. read -r MA MI PA <<< "$BASE"
  case "$BUMP" in
    major) NEW="$((MA + 1)).0.0" ;;
    minor) NEW="$MA.$((MI + 1)).0" ;;
    patch) NEW="$MA.$MI.$((PA + 1))" ;;
  esac
fi
TAG="v$NEW"

git rev-parse -q --verify "refs/tags/$TAG" > /dev/null && {
  echo "pushReleaseTag: $TAG already exists locally" >&2
  exit 1
}
[ -n "$(git ls-remote --tags origin "$TAG" 2> /dev/null)" ] && {
  echo "pushReleaseTag: $TAG already exists on origin" >&2
  exit 1
}

echo "pushReleaseTag: v$BASE -> $TAG"

TOUCHED=()
for f in $(git ls-files -- 'Cargo.toml' '*/Cargo.toml'); do
  grep -qE '^version[[:space:]]*=[[:space:]]*"' "$f" || continue
  if $DRY; then
    echo "  would bump $f"
    continue
  fi
  sed -i -E "0,/^version[[:space:]]*=[[:space:]]*\"[^\"]*\"/s//version = \"$NEW\"/" "$f"
  TOUCHED+=("$f")
done

for f in $(git ls-files -- 'Cargo.lock' '*/Cargo.lock'); do
  if $DRY; then
    echo "  would resync $f (cargo update -w)"
    continue
  fi
  if command -v cargo > /dev/null; then
    (cd "$(dirname "$f")" && { cargo update -w --offline -q 2> /dev/null || cargo update -w -q; })
    TOUCHED+=("$f")
  else
    echo "warning: cargo not found; $f left stale (the next build resyncs it)" >&2
  fi
done

if $DRY; then
  echo "  would commit as 'release: $TAG', tag $TAG, and push HEAD + tag to origin"
  ! git diff --cached --quiet && echo "  note: currently staged changes would ride the release commit"
  exit 0
fi

[ ${#TOUCHED[@]} -gt 0 ] && git add -- "${TOUCHED[@]}"

if ! git diff --cached --quiet; then
  git commit -q -m "release: $TAG"
  echo "committed release: $TAG"
else
  echo "nothing to commit (versions already at $NEW); tagging HEAD as-is"
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "warning: unstaged/untracked changes left behind - they are NOT part of $TAG:" >&2
  git status --porcelain | sed 's/^/    /' >&2
fi

git tag -a "$TAG" -m "discoclip $NEW"
git push origin HEAD "refs/tags/$TAG"
echo "pushed $TAG - release.yml is building it"
