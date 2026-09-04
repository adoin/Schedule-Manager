"""Resolve a tested main-branch build, then verify its installer before publishing."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib


def command(*args):
    return subprocess.check_output(args, text=True, encoding="utf-8").strip()


def api(endpoint):
    return json.loads(command("gh", "api", endpoint))


def select_tag(version, tags, requested=None):
    allowed = (version, "v" + version)
    if requested is not None:
        if requested not in allowed or requested not in tags:
            raise ValueError("Tag must match Cargo.toml and point to the built commit")
        return requested
    return next((tag for tag in allowed if tag in tags), None)


def trusted_success(run, commit, repository):
    return (
        run.get("head_sha") == commit
        and run.get("head_branch") == "main"
        and run.get("head_repository", {}).get("full_name") == repository
        and run.get("path") == ".github/workflows/build.yml"
        and run.get("event") in ("push", "workflow_dispatch")
        and run.get("status") == "completed"
        and run.get("conclusion") == "success"
    )


def usable_artifact(artifacts, commit):
    name = "schedule-manager-windows-" + commit
    return any(item.get("name") == name and not item.get("expired", True)
               for item in artifacts)


def output(**values):
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        for key, value in values.items():
            stream.write(f"{key}={value}\n")


def resolve():
    repository = os.environ["GITHUB_REPOSITORY"]
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text(encoding="utf-8"))
    commit = command("git", "rev-parse", "HEAD")
    # A version tag is only eligible for artifacts built from trusted main history.
    subprocess.run(["git", "merge-base", "--is-ancestor", commit, "origin/main"], check=True)
    version = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    requested = os.environ["GITHUB_REF_NAME"] if os.environ["GITHUB_EVENT_NAME"] == "push" else None
    tag = select_tag(version, command("git", "tag", "--points-at", "HEAD").splitlines(), requested)
    output(ready="false")
    if tag is None:
        print("Build saved. No matching release tag on this commit yet.")
        return

    if os.environ["GITHUB_EVENT_NAME"] == "workflow_run":
        runs = [api(f"repos/{repository}/actions/runs/{event['workflow_run']['id']}")]
    else:
        # The repository endpoint also works while a newly added build workflow
        # is still being registered during the first simultaneous main/tag push.
        runs = api(f"repos/{repository}/actions/runs?head_sha={commit}&branch=main&per_page=100")["workflow_runs"]
    candidates = sorted((run for run in runs if trusted_success(run, commit, repository)),
                        key=lambda run: run["id"], reverse=True)
    for run in candidates:
        artifacts = api(f"repos/{repository}/actions/runs/{run['id']}/artifacts?per_page=100")["artifacts"]
        if usable_artifact(artifacts, commit):
            output(ready="true", tag=tag, commit=commit, version=version, run_id=run["id"])
            print(f"Reusing tested installer from build {run['id']} for {tag} ({commit}).")
            return
    if not runs or any(run.get("status") != "completed" for run in runs):
        # No polling runner: successful build completion triggers this workflow again.
        message = "Installer build pending. Publication will resume on successful Build Windows completion."
        print(message)
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
            stream.write(message + "\n")
        return
    raise ValueError("No successful build with a retained installer for this commit. Rerun its Build Windows run.")


def verify(directory, commit, version, run_id):
    directory = Path(directory)
    manifest = json.loads((directory / "build-manifest.json").read_text(encoding="utf-8-sig"))
    expected = dict(commit=commit, version=version, run_id=str(run_id),
                    filename=f"ScheduleManager-Setup-{version}.exe")
    for key, value in expected.items():
        if manifest.get(key) != value:
            raise ValueError(f"Artifact {key} mismatch")
    installer = directory / expected["filename"]
    with installer.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    if digest != manifest.get("sha256"):
        raise ValueError("Installer checksum mismatch")
    return installer


if __name__ == "__main__":
    if sys.argv[1:] == ["resolve"]:
        resolve()
    elif sys.argv[1:] == ["verify"]:
        print(verify("release-assets", os.environ["EXPECTED_COMMIT"],
                     os.environ["EXPECTED_VERSION"], os.environ["EXPECTED_RUN_ID"]))
    else:
        raise SystemExit("Usage: release_artifact.py resolve|verify")
