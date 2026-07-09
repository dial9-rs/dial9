# Contributing Guidelines

Thank you for your interest in contributing to our project. Whether it's a bug report, new feature, correction, or additional documentation, we greatly value feedback and contributions from our community.

Please read through this document before submitting any issues or pull requests to ensure we have all the necessary information to effectively respond to your bug report or contribution.

## Reporting Bugs/Feature Requests

We welcome you to use the GitHub issue tracker to report bugs or suggest features.

When filing an issue, please check [existing open](https://github.com/dial9-rs/dial9-tokio-telemetry/issues), or [recently closed](https://github.com/dial9-rs/dial9-tokio-telemetry/issues?utf8=%E2%9C%93&q=is%3Aissue%20is%3Aclosed%20), issues to make sure somebody else hasn't already reported the issue. Please try to include as much information as you can. Details like these are incredibly useful:

* A reproducible test case or series of steps
* The version of our code being used
* Any modifications you've made relevant to the bug
* Anything unusual about your environment or deployment

## Contributing via Pull Requests

Contributions via pull requests are much appreciated. Before sending us a pull request, please ensure that:

1. You are working against the latest source on the *main* branch.
2. You check existing open, and recently merged, pull requests to make sure someone else hasn't addressed the problem already.
3. You open an issue to discuss any significant work, we would hate for your time to be wasted.

To send us a pull request, please:

1. Fork the repository.
2. Modify the source; please focus on the specific change you are contributing. If you also reformat all the code, it will be hard for us to focus on your change.
3. Ensure local tests pass.
4. Commit to your fork using clear commit messages and ensure any Rust source files have been formatted with the [rustfmt tool](https://github.com/rust-lang/rustfmt#quick-start)
5. Send us a pull request, answering any default questions in the pull request interface.
6. Pay attention to any automated CI failures reported in the pull request, and stay involved in the conversation.

GitHub provides additional document on [forking a repository](https://help.github.com/articles/fork-a-repo/) and [creating a pull request](https://help.github.com/articles/creating-a-pull-request/).

## Finding contributions to work on

Looking at the existing issues is a great way to find something to contribute on. As our projects, by default, use the default GitHub issue labels (enhancement/bug/duplicate/help wanted/invalid/question/wontfix), looking at any ['help wanted'](https://github.com/dial9-rs/dial9-tokio-telemetry/labels/help%20wanted) issues is a great place to start.

## Dependencies on crates within the workspace

Within-workspace crate dependencies are managed centrally in the root `Cargo.toml` under `[workspace.dependencies]`, with both a `path` and a `version`:

```toml
# root Cargo.toml
[workspace.dependencies]
dial9-trace-format = { version = "0.3.2", path = "dial9-trace-format" }
```

Crates then reference these with `workspace = true`:

```toml
# dial9-tokio-telemetry/Cargo.toml
[dependencies]
dial9-trace-format = { workspace = true, features = ["serde"] }
```

The `version` in the workspace dependency is required for publishing. `release-plz` updates these versions automatically during releases.

Dev-dependencies on workspace crates should *not* include a `version` to avoid chicken-and-egg problems when publishing (since `release-plz` might update the version to the one you are currently publishing):

```toml
[dev-dependencies]
dial9-tokio-telemetry = { path = ".", features = ["analysis", "tracing-layer"] }
```

## Running tests
Some tests will only run with the `shuttle` cfg enabled. There is a script to run these: `scripts/test-shuttle.sh`.

For other tests, `cargo nextest run` will run all of the normal tests.

## Doing releases

Releases are human-initiated, not automatic. There are two kinds of release:

- **Mainline releases** cut from `main` — the normal path for shipping new work.
- **Maintenance releases** cut from a long-lived `release-*` branch (e.g.
  `release-0.3.x`) — for backporting fixes to an older release series without
  pulling in everything that has since landed on `main`.

Both use the same two-part shape (a release PR that bumps versions + updates the
changelog, then a manual publish), and both publish to crates.io via [trusted
publishing], so no tokens need to be managed. The differences are called out
below.

[trusted publishing]: https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html
[conventional commits]: https://www.conventionalcommits.org/en/v1.0.0/

### Releasing from `main`

1. **Release PR (automatic):** On every push to `main`, the `release-pr.yml`
   workflow runs `release-plz release-pr`, which creates/updates a PR with
   version bumps and changelog entries based on [conventional commits]. This PR
   accumulates over time — you can merge many feature PRs before releasing.
2. Review the changelog and version bumps. To force a bump (e.g. a major), edit
   the `Cargo.toml` versions in the release PR before merging.
3. **Trigger CI:** The release PR is created by `GITHUB_TOKEN`, so GitHub won't
   automatically run CI on it. Before merging, close and reopen the PR to
   trigger CI, and wait for the `CI Pass` check to go green.
4. Merge the release PR.
5. **Publish:** Go to **Actions → "Publish release" → Run workflow**, set
   `release_branch` to `main`, and run. A `dial9-maintainers` member then
   approves the deployment in the `release` environment before publishing
   proceeds.

### Releasing from a maintenance (`release-*`) branch

Use this when you need to ship a fix on an older series (e.g. patch `0.3.x`
while `main` is on `0.4.x`). The mechanics are the same, with a few
branch-specific wrinkles.

1. **Create the branch (once per series), if it doesn't exist.** Branch off the
   latest tag of that series and push it, e.g.:

   ```bash
   git branch release-0.3.x dial9-tokio-telemetry-v0.3.13
   git push origin release-0.3.x
   ```

   `release-*` branches are protected the same as `main` (PR review + `CI Pass`
   required, no direct pushes), and **creating one is restricted to repo
   admins** by a branch-protection ruleset, so no disabling of protection is
   needed — an admin just pushes the new branch.

2. **Backport your fix via a PR into the release branch.** Cherry-pick or
   re-implement the change with `release-0.3.x` as the PR base (not `main`).
   Keep changes API-compatible for the series where possible; `cargo-semver-checks`
   in CI compares against the PR's base branch, so it will flag breaking changes
   for that series specifically.

3. **Generate the release PR.** The `release-pr.yml` automation only runs on
   `main`, so for a release branch you run release-plz yourself from a checkout
   of the branch:

   ```bash
   git checkout release-0.3.x && git pull
   GIT_TOKEN="$(gh auth token)" release-plz release-pr --git-token "$(gh auth token)"
   ```

   This opens a version-bump/changelog PR against `release-0.3.x`. Review and
   merge it as usual (close/reopen to trigger CI, wait for `CI Pass`).

4. **Publish:** **Actions → "Publish release" → Run workflow**, set
   `release_branch` to the release branch (e.g. `release-0.3.x`), and run. As
   with `main`, a `dial9-maintainers` member approves the `release` environment
   deployment.

> **Why `release_branch` is an input.** `workflow_dispatch` runs the copy of a
> workflow that lives on the dispatched ref, which would otherwise mean every
> `release-*` branch carried (and drifted) its own copy of `release.yml`. The
> publish workflow instead always runs from `main`'s copy and takes the target
> branch as a required input, so the release logic has a single source of truth.
> A `validate-branch` step rejects any input that isn't `main` or `release-*`.

> **Note on `pr_branch_prefix`.** release-plz opens its release PR from an
> ephemeral branch. `main` uses the `release-plz/` prefix and each maintenance
> branch uses its own (e.g. `release-plz-0.3/`), configured via
> `pr_branch_prefix` in `release-plz.toml`. Two things depend on this: the
> prefix must **not** collide with the `release-*` protection glob (the trailing
> slash keeps it exempt), and it must **differ per series** — release-plz closes
> any open PR sharing its configured prefix, so a shared prefix would let a
> `main` release run close a release branch's PR (and vice versa).

### Semver checks

`cargo-semver-checks` runs on every PR as an advisory check. It won't block merge, but if it reports breaking changes, ensure the release PR reflects a major version bump before publishing.

### Breaking changes

You can freely merge breaking changes to `main`. The release PR will accumulate them. Before publishing, verify that `release-plz` has bumped the major version (it runs `semver_check = true` and should do this automatically). If it hasn't, manually adjust the version in the release PR.

### Publishing a new crate

trusted publishing is unable to publish new crates. If you want to add a new crate to the dial9 family, you should:

1. create a branch that contains the crate you are publishing (it should be in the root `Cargo.toml`'s `workspace.members`, and in a publishable state).
2. add the package name to the `changelog_include` list in the `[[package]] name = "dial9-tokio-telemetry"` entry in `release-plz.toml`.
3. run `cargo publish -p <package> --dry-run`
4. get a temporary crates.io token just for the publishing
5. run `cargo login` with that token
6. run `cargo publish -p <package>`
7. set up trusted publishing via the crates.io WebUI to the following state:

   ```
   Publisher: Github
   Repository: dial9-rs/dial9-tokio-telemetry
   Workflow: release.yml
   Environment: release
   ```

8. revoke the temporary crates.io token

Further publishing should happen via release-plz, without needing to manually work with tokens.

## Licensing

See the [LICENSE](https://github.com/dial9-rs/dial9-tokio-telemetry/blob/main/LICENSE) file for our project's licensing.
