from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "check_release_published.sh"


class ReleaseGateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name) / "repo"
        self.root.mkdir()
        self.remote = self.root.parent / "remote.git"
        self.git("init", "--quiet", "--bare", str(self.remote))
        self.git("init", "--quiet")
        self.git("config", "user.email", "release-gate@example.invalid")
        self.git("config", "user.name", "Release gate test")
        self.git("remote", "add", "origin", str(self.remote))
        self.initial = self.commit("Initial repo")
        self.commit("chore: release (#1)")
        script = self.root / "scripts/check_release_published.sh"
        script.parent.mkdir()
        script.write_text(SCRIPT.read_text())
        self.base = self.commit("ci: add publication gate")

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.root), *args], text=True, stderr=subprocess.DEVNULL
        ).strip()

    def commit(self, subject):
        self.git("add", ".")
        self.git("commit", "--quiet", "--allow-empty", "-m", subject)
        return self.git("rev-parse", "HEAD")

    def publish(self, release):
        # This is the same marker push as the publish workflow, including reruns.
        self.git("push", "origin", f"{release}:refs/tags/release-published/{release}")

    def check(self, base, candidate=None, succeeds=True):
        args = ["bash", str(SCRIPT), base]
        if candidate:
            args.append(candidate)
        result = subprocess.run(args, cwd=self.root, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0 if succeeds else 1, result.stdout + result.stderr)
        return result.stdout + result.stderr

    def test_no_release_or_legacy_release_needs_no_marker(self):
        self.check(self.initial)
        self.check(self.base)

    def test_pending_release_blocks_subsequent_prs(self):
        release = self.commit("chore: release (#2)")
        head = self.commit("docs: update README (#3)")
        self.assertIn("awaiting publication", self.check(head, succeeds=False))
        self.assertIn(release, self.check(release, succeeds=False))

    def test_publication_unlocks_merges_and_reruns_are_idempotent(self):
        release = self.commit("chore: release (#2)")
        self.check(release, succeeds=False)
        self.publish(release)
        self.publish(release)
        self.check(release)

    def test_old_completion_tag_does_not_unlock_next_release(self):
        first = self.commit("chore: release (#2)")
        self.publish(first)
        second = self.commit("chore: release v1.0.2 (#3)")
        self.assertIn(second, self.check(second, succeeds=False))

    def test_release_pr_can_merge_after_previous_release_is_published(self):
        first = self.commit("chore: release (#2)")
        self.publish(first)
        second = self.commit("chore: release (#3)")
        self.check(first, second)

    def test_merge_group_cannot_include_pr_after_release(self):
        self.commit("chore: release (#2)")
        candidate = self.commit("fix: queued after release (#3)")
        self.assertIn("requeue", self.check(self.base, candidate, succeeds=False))

    def test_merge_group_can_end_with_release(self):
        self.commit("fix: queued before release (#2)")
        candidate = self.commit("chore: release (#3)")
        self.check(self.base, candidate)

    def test_remote_error_keeps_merges_blocked(self):
        release = self.commit("chore: release (#2)")
        self.git("remote", "set-url", "origin", str(self.remote / "missing"))
        self.check(release, succeeds=False)


if __name__ == "__main__":
    unittest.main()
