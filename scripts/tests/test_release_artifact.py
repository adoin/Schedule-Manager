import copy
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import release_artifact as release


class ReleaseArtifactTests(unittest.TestCase):
    def run_resolver(self, event_name, run, artifacts=None):
        with tempfile.TemporaryDirectory() as temporary:
            event_path = Path(temporary) / "event.json"
            event_path.write_text(json.dumps({"workflow_run": {"id": 123}}), encoding="utf-8")
            env = dict(GITHUB_REPOSITORY="owner/repo", GITHUB_EVENT_NAME=event_name,
                       GITHUB_REF_NAME="1.0.11", GITHUB_EVENT_PATH=str(event_path),
                       GITHUB_STEP_SUMMARY=str(Path(temporary) / "summary.md"))
            responses = [run if event_name == "workflow_run" else {"workflow_runs": [run]}]
            if artifacts is not None:
                responses.append({"artifacts": artifacts})
            with patch.dict(os.environ, env), \
                 patch.object(release, "command", side_effect=["abc", "1.0.11"]), \
                 patch.object(release.subprocess, "run"), \
                 patch.object(release.tomllib, "loads", return_value={"package": {"version": "1.0.11"}}), \
                 patch.object(release, "api", side_effect=responses), \
                 patch.object(release, "output") as outputs:
                release.resolve()
                return [call.kwargs for call in outputs.call_args_list]

    def successful_run(self):
        return dict(id=123, head_sha="abc", head_branch="main", head_repository={"full_name": "owner/repo"},
                    path=".github/workflows/build.yml", event="push", status="completed", conclusion="success")

    def test_tag_waits_without_polling_then_completion_publishes(self):
        run = self.successful_run()
        run.update(status="in_progress", conclusion=None)
        self.assertEqual(self.run_resolver("push", run), [dict(ready="false")])
        outputs = self.run_resolver("workflow_run", self.successful_run(),
                                    [dict(name="schedule-manager-windows-abc", expired=False)])
        self.assertEqual(outputs[-1], dict(ready="true", tag="1.0.11", commit="abc", version="1.0.11", run_id=123))

    def test_tag_after_success_reuses_build_immediately(self):
        outputs = self.run_resolver("push", self.successful_run(),
                                    [dict(name="schedule-manager-windows-abc", expired=False)])
        self.assertEqual(outputs[-1]["ready"], "true")

    def test_failed_or_expired_build_cannot_publish(self):
        failed = self.successful_run()
        failed["conclusion"] = "failure"
        with self.assertRaises(ValueError):
            self.run_resolver("push", failed)
        with self.assertRaises(ValueError):
            self.run_resolver("push", self.successful_run(),
                              [dict(name="schedule-manager-windows-abc", expired=True)])

    def test_build_before_tag_and_tag_before_build(self):
        self.assertIsNone(release.select_tag("1.0.11", []))
        self.assertEqual(release.select_tag("1.0.11", ["1.0.11"]), "1.0.11")
        self.assertEqual(release.select_tag("1.0.11", ["v1.0.11"], "v1.0.11"), "v1.0.11")
        with self.assertRaises(ValueError):
            release.select_tag("1.0.11", ["1.0.10"], "1.0.10")

    def test_only_successful_exact_commit_from_main_is_trusted(self):
        run = dict(head_sha="abc", head_branch="main", head_repository={"full_name": "owner/repo"},
                   path=".github/workflows/build.yml", event="push", status="completed", conclusion="success")
        self.assertTrue(release.trusted_success(run, "abc", "owner/repo"))
        for key, value in dict(head_sha="def", head_branch="feature", head_repository={"full_name": "fork/repo"},
                               path=".github/workflows/other.yml", event="pull_request", status="in_progress", conclusion="failure").items():
            with self.subTest(key=key):
                bad = copy.deepcopy(run)
                bad[key] = value
                self.assertFalse(release.trusted_success(bad, "abc", "owner/repo"))

    def test_expired_or_other_commit_artifact_is_rejected(self):
        artifact = dict(name="schedule-manager-windows-abc", expired=False)
        self.assertTrue(release.usable_artifact([artifact], "abc"))
        self.assertFalse(release.usable_artifact([artifact], "def"))
        artifact["expired"] = True
        self.assertFalse(release.usable_artifact([artifact], "abc"))

    def test_installer_identity_and_bytes_are_verified(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            installer = directory / "ScheduleManager-Setup-1.0.11.exe"
            installer.write_bytes(b"test installer")
            manifest = dict(commit="abc", version="1.0.11", run_id="123", filename=installer.name,
                            sha256=hashlib.sha256(installer.read_bytes()).hexdigest())
            manifest_path = directory / "build-manifest.json"
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8-sig")
            self.assertEqual(release.verify(directory, "abc", "1.0.11", "123"), installer)
            for args in [("def", "1.0.11", "123"), ("abc", "1.0.10", "123"), ("abc", "1.0.11", "456")]:
                with self.subTest(args=args), self.assertRaises(ValueError):
                    release.verify(directory, *args)
            manifest["filename"] = "../unexpected.exe"
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaises(ValueError):
                release.verify(directory, "abc", "1.0.11", "123")
            manifest["filename"] = installer.name
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            installer.write_bytes(b"modified installer")
            with self.assertRaises(ValueError):
                release.verify(directory, "abc", "1.0.11", "123")


if __name__ == "__main__":
    unittest.main()
