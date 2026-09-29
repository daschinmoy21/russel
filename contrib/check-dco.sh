#!/usr/bin/env bash
# Check that every non-merge commit in a range carries a DCO sign-off
# (`Signed-off-by: Name <email>`) whose email matches the commit author.
# Add one with `git commit -s`. `-s` signs with your configured user.email,
# so that must be the address the commits are authored with.
#
#   contrib/check-dco.sh [RANGE]    # default: origin/main..HEAD
set -euo pipefail

range="${1:-origin/main..HEAD}"
commits="$(git rev-list --no-merges "$range")"
if [[ -z "$commits" ]]; then
  echo "no commits in ${range}"
  exit 0
fi

failed=0
while read -r commit; do
  email="$(git show -s --format='%ae' "$commit")"
  subject="$(git show -s --format='%s' "$commit")"
  # Compare the one <email> that ends each sign-off line, exactly (case-
  # insensitive). A substring match would accept `<author> <other>`.
  signed="$(git show -s --format='%B' "$commit" |
    sed -n 's/^Signed-off-by: .* <\([^<>]*\)>[[:space:]]*$/\1/Ip')"
  if grep -qixF -- "$email" <<<"$signed"; then
    echo "ok       ${commit:0:12} ${subject}"
  else
    echo "missing  ${commit:0:12} ${subject} (no Signed-off-by for <${email}>)" >&2
    failed=1
  fi
done <<<"$commits"

if ((failed)); then
  cat >&2 <<'EOF'

Some commits have no DCO sign-off matching their author email. A sign-off
uses your git user.email, so first make it the email the commits should carry:
  git config user.email you@example.com
then re-sign the branch, resetting each commit's author to that identity:
  git rebase --exec 'git commit --amend --no-edit --reset-author --signoff' origin/main
  git push --force-with-lease
See "Developer Certificate of Origin" in CONTRIBUTING.md.
EOF
  exit 1
fi
