#!/usr/bin/env bash
# Adds the packages in $DIST to the APT repository on the gh-pages branch and
# pushes the result as one commit without parent, so the branch never grows a
# history of old packages.
#
# Two publications at the same time (a commit on main and a tag) are safe:
# the push only succeeds if gh-pages is still the state this run started
# from (--force-with-lease); otherwise the run starts over from the new state.
# Runs in the repository checkout; needs the image wanderlan-aptrepo.
set -euo pipefail
: "${DIST:?}" "${MESSAGE:?}" "${APT_GPG_PRIVATE_KEY:?}"
branch=gh-pages
pages=.apt-pages
export GIT_AUTHOR_NAME="github-actions[bot]"
export GIT_AUTHOR_EMAIL="41898282+github-actions[bot]@users.noreply.github.com"
export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME" GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"

shopt -s nullglob
debs=("$DIST"/*.deb)
if [ "${#debs[@]}" -eq 0 ]; then
  echo "::error::no .deb packages in $DIST"
  exit 1
fi

for attempt in 1 2 3 4 5; do
  git worktree remove --force "$pages" 2>/dev/null || rm -rf "$pages"
  git worktree prune
  if git ls-remote --exit-code --heads origin "$branch" >/dev/null; then
    git fetch --quiet --depth 1 origin "+refs/heads/$branch:refs/remotes/origin/$branch"
    lease=$(git rev-parse "refs/remotes/origin/$branch")
    git worktree add --quiet --detach "$pages" "$lease"
  else
    lease=""   # the branch must still not exist when pushing
    git worktree add --quiet --orphan -b "apt-publish-$attempt" "$pages"
  fi

  docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/w" -w /w \
    -e APT_GPG_PRIVATE_KEY -e APT_GPG_PASSPHRASE -e KEEP \
    wanderlan-aptrepo scripts/apt-repo.sh "$pages" "${debs[@]}"

  git -C "$pages" add -A
  tree=$(git -C "$pages" write-tree)
  commit=$(git -C "$pages" commit-tree "$tree" -m "$MESSAGE")
  if git push --force-with-lease="refs/heads/$branch:$lease" origin "$commit:refs/heads/$branch"; then
    echo "published: $MESSAGE"
    git worktree remove --force "$pages"
    exit 0
  fi
  echo "gh-pages changed meanwhile (attempt $attempt); starting over from the new state"
  sleep $((attempt * 5))
done
echo "::error::could not push $branch after 5 attempts"
exit 1
