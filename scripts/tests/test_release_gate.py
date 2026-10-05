import contextlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_release_published as gate


class ReleaseGateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.addCleanup(self.directory.cleanup)
        self.git_patch = patch.object(gate, "git", self.git)
        self.git_patch.start()
        self.addCleanup(self.git_patch.stop)
        self.git("init", "--quiet")
        self.git("config", "user.email", "release-gate@example.invalid")
        self.git("config", "user.name", "Release gate test")
        self.write("Cargo.toml", '[workspace]\nmembers = ["one", "two", "example"]\n')
        self.write("release-plz.toml", "[workspace]\n")
        self.package("one", "1.0.0")
        self.package("two", "2.0.0")
        self.package("example", "0.1.0", publish=False)
        self.initial = self.commit("Initial crates")
        self.published = {("one", "1.0.0"), ("two", "2.0.0")}

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args], text=True, stderr=subprocess.DEVNULL
        ).strip()

    def write(self, path, contents):
        file = self.root / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(contents)

    def package(self, name, version, publish=True):
        self.write(
            f"{name}/Cargo.toml",
            f'[package]\nname = "{name}"\nversion = "{version}"\n'
            f"publish = {str(publish).lower()}\n",
        )

    def commit(self, subject):
        self.git("add", ".")
        self.git("commit", "--quiet", "-m", subject)
        return self.git("rev-parse", "HEAD")

    def release(self):
        self.package("one", "1.0.1")
        self.package("two", "2.0.1")
        return self.commit("chore: release (#10)")

    def check(self, base, candidate=None):
        with contextlib.redirect_stdout(io.StringIO()):
            gate.check_release(base, candidate, self.published.__contains__)

    def test_before_first_release_does_not_block_unpublished_new_crates(self):
        self.check(self.initial)

    def test_merged_release_blocks_all_following_prs(self):
        release = self.release()
        self.write("README.md", "Docs only\n")
        head = self.commit("docs: update README")
        with self.assertRaisesRegex(ValueError, "one 1.0.1, two 2.0.1"):
            self.check(head)
        with self.assertRaisesRegex(ValueError, "awaiting publication"):
            self.check(release)

    def test_partial_publish_does_not_unlock_merges(self):
        release = self.release()
        self.published.add(("one", "1.0.1"))
        with self.assertRaisesRegex(ValueError, "awaiting publication: two 2.0.1"):
            self.check(release)
        self.published.add(("two", "2.0.1"))
        self.check(release)

    def test_release_pr_can_merge_when_previous_release_is_published(self):
        base = self.release()
        self.published.update({("one", "1.0.1"), ("two", "2.0.1")})
        self.package("one", "1.0.2")
        candidate = self.commit("chore: release v1.0.2 (#11)")
        self.check(base)
        self.check(base, candidate)

    def test_merge_group_cannot_include_changes_after_release_pr(self):
        base = self.release()
        self.published.update({("one", "1.0.1"), ("two", "2.0.1")})
        self.package("one", "1.0.2")
        self.commit("chore: release (#11)")
        self.write("README.md", "A PR queued after the release\n")
        candidate = self.commit("docs: update README (#12)")
        with self.assertRaisesRegex(ValueError, "changes after a release PR"):
            self.check(base, candidate)

    def test_merge_group_can_end_with_release_pr(self):
        base = self.release()
        self.published.update({("one", "1.0.1"), ("two", "2.0.1")})
        self.write("README.md", "A PR queued before the release\n")
        self.commit("docs: update README (#11)")
        self.package("one", "1.0.2")
        candidate = self.commit("chore: release (#12)")
        self.check(base, candidate)

    def test_new_crate_after_published_release_does_not_freeze_merges(self):
        self.release()
        self.published.update({("one", "1.0.1"), ("two", "2.0.1")})
        self.write("Cargo.toml", '[workspace]\nmembers = ["one", "two", "example", "new"]\n')
        self.package("new", "0.1.0")
        head = self.commit("feat: add a crate")
        self.check(head)

    def test_release_configuration_excludes_unpublished_crates(self):
        self.write(
            "release-plz.toml",
            '[workspace]\n[[package]]\nname = "two"\nrelease = false\n',
        )
        release = self.release()
        self.published.add(("one", "1.0.1"))
        self.check(release)

    def test_workspace_inherited_version(self):
        self.write(
            "Cargo.toml",
            '[workspace]\nmembers = ["one", "two", "example"]\n'
            '[workspace.package]\nversion = "1.0.1"\n',
        )
        self.write("one/Cargo.toml", '[package]\nname = "one"\nversion.workspace = true\n')
        release = self.commit("chore: release (#10)")
        self.published.add(("one", "1.0.1"))
        self.check(release)

    def test_registry_failure_blocks_merges(self):
        release = self.release()
        with patch.object(gate, "urlopen", side_effect=OSError("Registry unavailable")):
            with patch.object(sys, "argv", ["check", "--base-ref", release]):
                with contextlib.redirect_stderr(io.StringIO()) as output:
                    self.assertEqual(gate.main(), 1)
                self.assertIn("Registry unavailable", output.getvalue())


class RegistryTests(unittest.TestCase):
    def test_exact_published_version(self):
        response = io.BytesIO(json.dumps({"version": {"crate": "one", "num": "1.0.1"}}).encode())
        with patch.object(gate, "urlopen", return_value=response):
            self.assertTrue(gate.crate_published(("one", "1.0.1")))

    def test_missing_version(self):
        error = HTTPError("https://crates.io", 404, "Not found", {}, None)
        with patch.object(gate, "urlopen", side_effect=error):
            self.assertFalse(gate.crate_published(("one", "1.0.1")))

    def test_registry_error_is_not_treated_as_unpublished(self):
        error = HTTPError("https://crates.io", 503, "Unavailable", {}, None)
        with patch.object(gate, "urlopen", side_effect=error):
            with self.assertRaisesRegex(RuntimeError, "Cannot check one 1.0.1"):
                gate.crate_published(("one", "1.0.1"))

    def test_unexpected_response_cannot_unlock_merges(self):
        response = io.BytesIO(json.dumps({"version": {"crate": "one", "num": "1.0.0"}}).encode())
        with patch.object(gate, "urlopen", return_value=response):
            with self.assertRaisesRegex(ValueError, "Unexpected registry response"):
                gate.crate_published(("one", "1.0.1"))


if __name__ == "__main__":
    unittest.main()
