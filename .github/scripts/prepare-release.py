#!/usr/bin/env python3
"""Prepare release metadata locally; never commit or push generated files."""

import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import tomllib


def output(*args):
    return subprocess.check_output(args, text=True).strip()


def run(*args):
    subprocess.run(args, check=True)


def version():
    return tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]


def version_tuple(value):
    if not re.fullmatch(r"\d+\.\d+\.\d+", value):
        raise ValueError(f"Expected a stable semantic version, got {value!r}")
    return tuple(map(int, value.split(".")))


def emit(**values):
    with open(os.environ["GITHUB_OUTPUT"], "a") as destination:
        for name, value in values.items():
            print(f"{name}={value}", file=destination)


head = output("git", "rev-parse", "HEAD")
tags = []
current_tag = None
for tag in output("git", "tag", "--merged", "HEAD", "--sort=-version:refname").splitlines():
    if not re.fullmatch(r"v\d+\.\d+\.\d+", tag):
        continue
    if output("git", "rev-list", "-n", "1", tag) == head:
        if current_tag:
            raise ValueError("Source commit has multiple version tags")
        current_tag = tag
    else:
        tags.append(tag)

if current_tag:
    release = subprocess.run(
        ["gh", "release", "view", current_tag, "--json", "isDraft"],
        text=True, capture_output=True,
    )
    if release.returncode == 0 and not json.loads(release.stdout)["isDraft"]:
        print(f"{current_tag} is already published; leaving it unchanged.")
        emit(ready="false", tag=current_tag, version=current_tag[1:])
        raise SystemExit(0)
    if release.returncode and "not found" not in release.stderr.lower():
        raise RuntimeError(release.stderr)

previous_tag = tags[0] if tags else None
with tempfile.TemporaryDirectory(prefix="rkdeveloptool-release-") as directory:
    temporary = Path(directory)
    version_config = temporary / "version.toml"
    version_config.write_text("[workspace]\nchangelog_update = false\n")

    def set_version(value, manifest="Cargo.toml"):
        version_tuple(value)
        run("release-plz", "set-version", "--config", str(version_config),
            "--manifest-path", str(Path(manifest).resolve()), value)

    if previous_tag:
        # Source tags retain the repository's original Cargo version. Normalize
        # both sides of release-plz's registry-manifest comparison to the tag.
        previous_version = previous_tag[1:]
        baseline = temporary / "baseline"
        run("git", "clone", "--quiet", "--no-hardlinks", ".", str(baseline))
        run("git", "-C", str(baseline), "checkout", "--quiet", previous_tag)
        set_version(previous_version, baseline / "Cargo.toml")
        set_version(previous_version)
        update_config = temporary / "update.toml"
        update_config.write_text(
            Path("release-plz.toml").read_text().replace("git_only = true", "git_only = false", 1)
        )
        run("release-plz", "update", "--allow-dirty", "--config", str(update_config),
            "--registry-manifest-path", str(baseline / "Cargo.toml"))
        if version_tuple(version()) <= version_tuple(previous_version):
            # A main push that only changes CI/docs still gets its own release.
            major, minor, patch = version_tuple(previous_version)
            set_version(f"{major}.{minor}.{patch + 1}")
    else:
        run("release-plz", "update", "--allow-dirty")

    if current_tag and version() != current_tag[1:]:
        set_version(current_tag[1:])
    release_version = version()
    version_tuple(release_version)
    run("cargo", "update", "--workspace")

tag = f"v{release_version}"
if previous_tag and version_tuple(release_version) <= version_tuple(previous_tag[1:]):
    raise ValueError(f"{tag} must be newer than {previous_tag}")

# Supply a current-version entry when CI/docs-only changes have no changelog.
# release-plz reads this entry to create the Release description by default.
changelog = Path("CHANGELOG.md")
if not changelog.exists():
    commit_range = f"{previous_tag}..HEAD" if previous_tag else "HEAD"
    commits = output("git", "log", "--reverse", "--format=- %s (%h)", commit_range)
    changelog.write_text(f"## [{release_version}]\n\n### Changed\n\n{commits}\n")
print(f"Prepared {tag} from {head}; generated files stay in the build workspace.")
emit(ready="true", tag=tag, version=release_version)
