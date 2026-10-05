#!/usr/bin/env python3
"""Block merges until every crate in the latest merged release is published."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import PurePosixPath
import subprocess
import sys
import tomllib
from urllib.error import HTTPError
from urllib.request import Request, urlopen


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def release_commits(revision):
    # release-plz's default PR titles start with "chore: release", including
    # single-crate releases that append a version. Main uses squash merges.
    return git(
        "log",
        "--first-parent",
        "--format=%H",
        "--extended-regexp",
        "--grep=^chore: release($| )",
        revision,
        "--",
    ).splitlines()


def manifest(revision, path):
    return tomllib.loads(git("show", f"{revision}:{path}"))


def release_packages(revision):
    # Read the release snapshot, not the PR's manifests: a version bump in the
    # release PR must pass, and a newly added crate must not freeze all merges.
    workspace = manifest(revision, "Cargo.toml")["workspace"]
    config = manifest(revision, "release-plz.toml")
    defaults = config.get("workspace", {})
    overrides = {package["name"]: package for package in config.get("package", [])}
    packages = []
    for member in workspace["members"]:
        package = manifest(revision, PurePosixPath(member) / "Cargo.toml")["package"]
        name = package["name"]
        publish = package.get("publish", True)
        settings = defaults | overrides.get(name, {})
        if publish is False or publish == []:
            continue
        if settings.get("release", True) is False or settings.get("publish", True) is False:
            continue
        if isinstance(publish, list) and "crates-io" not in publish:
            raise ValueError(f"{name}: the release gate only supports crates.io")
        version = package["version"]
        if isinstance(version, dict) and version.get("workspace") is True:
            version = workspace["package"]["version"]
        if not isinstance(version, str):
            raise ValueError(f"{name}: expected a package version")
        packages.append((name, version))
    if not packages:
        raise ValueError(f"Release {revision} contains no publishable crates")
    return packages


def crate_published(package):
    name, version = package
    request = Request(
        f"https://crates.io/api/v1/crates/{name}/{version}",
        headers={"User-Agent": "dial9-release-gate (https://github.com/dial9-rs/dial9)"},
    )
    try:
        with urlopen(request, timeout=20) as response:
            data = json.load(response)["version"]
    except HTTPError as error:
        if error.code == 404:
            return False
        raise RuntimeError(f"Cannot check {name} {version}: {error}") from error
    if data["crate"] != name or data["num"] != version:
        raise ValueError(f"Unexpected registry response for {name} {version}")
    return True


def check_release(base_ref, candidate_ref=None, published=crate_published):
    if candidate_ref is not None:
        candidate = git("rev-parse", "--verify", candidate_ref)
        # A merge queue can build several squash commits together. Do not let
        # a release PR and any subsequent PR pass in the same group.
        if any(commit != candidate for commit in release_commits(f"{base_ref}..{candidate}")):
            raise ValueError(
                "This merge group contains changes after a release PR. "
                "Queue the release PR without subsequent PRs, publish it, "
                "then requeue the remaining PRs."
            )

    releases = release_commits(base_ref)
    if not releases:
        print("No release PR has merged on the target branch yet.")
        return
    release = releases[0]
    packages = release_packages(release)
    with ThreadPoolExecutor(max_workers=4) as pool:
        results = list(pool.map(published, packages))
    pending = [f"{name} {version}" for (name, version), exists in zip(packages, results) if not exists]
    if pending:
        raise ValueError(
            f"Release {release} is awaiting publication: {', '.join(pending)}. "
            "Run Publish release for the target branch, approve the release "
            "environment, then rerun the failed CI jobs."
        )
    print(f"All {len(packages)} crate versions in release {release} are published.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", required=True)
    parser.add_argument("--candidate-ref", help="Merge-group head (omit for pull requests)")
    args = parser.parse_args()
    try:
        check_release(args.base_ref, args.candidate_ref)
    except (OSError, subprocess.CalledProcessError, ValueError, KeyError, RuntimeError) as error:
        print(f"::error::{error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
