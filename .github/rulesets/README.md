# Branch protection & release config (snapshots)

These files are **checked-in snapshots** of the GitHub repository configuration
that is *not* stored in the workflow YAML: branch-protection rulesets and the
`release` deployment environment.

> **Important:** GitHub does **not** enforce config from these files. They are
> documentation / disaster-recovery snapshots and a place to review changes.
> The source of truth is the live repo settings. If you change protection in
> the UI, re-export (see below) and commit so these stay in sync.

## Files

| File | What it is | Applies to |
|------|-----------|-----------|
| `protect-main.json` | Full protection for `main` | `refs/heads/main` |
| `protect-release-glob.json` | Same content protection as main, minus the merge queue (release branches are low-traffic) | `refs/heads/release-*` |
| `restrict-release-creation.json` | Only repo admins may create `release-*` branches | `refs/heads/release-*` |
| `protect-release-slash.json` | Legacy, weaker protection (no required check) | `refs/heads/release/*` |
| `../environments/release.json` | `release` deploy environment: required reviewers + allowed branches | `main`, `release-*` |

## Design notes

- **No merge queue on release branches.** Release branches are low-traffic, so
  they merge directly once a PR is approved and `CI Pass` is green — no queue.
  Only `main` carries a merge queue (inline in `protect-main.json`; an exact
  ref, because GitHub rejects `merge_queue` on a glob). If a future release
  series ever needs a queue, add a *separate* exact-ref ruleset for it (a glob
  like `release-*` can't carry one).
- **`bypass_actors` is deliberately empty** on the content rulesets
  (`protect-*`) so PR review / `CI Pass` / deletion / non-fast-forward are
  unbypassable by *everyone*, including admins. The admin bypass lives only in
  `restrict-release-creation.json`, so admins can create release branches
  without also being able to bypass content protection.
- **`RepositoryRole` `actor_id: 5`** is the built-in **admin** role.
- **Release publishing is double-gated:** the `release` environment requires
  `dial9-maintainers` approval on every deploy, enforced by GitHub regardless
  of workflow-file contents (a tampered `release.yml` can't bypass it).

## Re-applying after an accidental change

### Apply all rulesets (idempotent)

Run from this directory (`.github/rulesets/`). For each `*.json` it matches the
live ruleset by `name`: **PUT** if one exists (update in place, preserving its
id), otherwise **POST** (create). Safe to run repeatedly and after a partial
deletion.

```bash
REPO=dial9-rs/dial9
# map of live ruleset name -> id
existing=$(gh api "repos/$REPO/rulesets" --jq '.[] | "\(.name)\t\(.id)"')
for f in *.json; do
  name=$(jq -r .name "$f")
  id=$(printf '%s\n' "$existing" | awk -F'\t' -v n="$name" '$1==n{print $2; exit}')
  if [ -n "$id" ]; then
    echo "PUT  $f -> ruleset $id ($name)"
    gh api --method PUT "repos/$REPO/rulesets/$id" --input "$f" >/dev/null
  else
    echo "POST $f (create '$name')"
    gh api --method POST "repos/$REPO/rulesets" --input "$f" >/dev/null
  fi
done
```

### Apply a single ruleset

```bash
# create
gh api --method POST repos/dial9-rs/dial9/rulesets --input protect-main.json
# update in place (find <id> via: gh api repos/dial9-rs/dial9/rulesets)
gh api --method PUT repos/dial9-rs/dial9/rulesets/<id> --input protect-main.json
```

Environment (`reviewers` uses `{type, id}`; the `_name` key in the snapshot is a
human-readable comment — drop it before PUT):

```bash
gh api --method PUT repos/dial9-rs/dial9/environments/release \
  -f 'wait_timer=0' -F 'prevent_self_review=false' \
  -F 'reviewers[][type]=Team' -F 'reviewers[][id]=16676353' \
  -F 'deployment_branch_policy[protected_branches]=false' \
  -F 'deployment_branch_policy[custom_branch_policies]=true'
# then re-add branch patterns:
for p in main 'release-*'; do
  gh api --method POST repos/dial9-rs/dial9/environments/release/deployment-branch-policies \
    -f "name=$p" -f 'type=branch'
done
```

## Re-exporting a fresh snapshot

Pull current live config and overwrite these files, then review the diff:

```bash
for id in $(gh api repos/dial9-rs/dial9/rulesets --jq '.[].id'); do
  gh api repos/dial9-rs/dial9/rulesets/$id
done
gh api repos/dial9-rs/dial9/environments/release
gh api repos/dial9-rs/dial9/environments/release/deployment-branch-policies
```
