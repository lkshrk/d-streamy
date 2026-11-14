#!/usr/bin/env python3
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
import zipfile
from typing import TypedDict
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
ISOLATED_GIT = dict(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
spec = importlib.util.spec_from_file_location("notices", ROOT / "scripts/third-party-notices.py")
assert spec is not None and spec.loader is not None
notices = importlib.util.module_from_spec(spec)
spec.loader.exec_module(notices)


def workflow(name):
    return (ROOT / ".github/workflows" / name).read_text()


def step(name, title):
    source = workflow(name)
    section = source.split(f"- name: {title}\n", 1)[1]
    section = re.split(r"\n(?:      - |  [a-z][a-z-]*:)", section, maxsplit=1)[0]
    body = section.split("run: |\n", 1)[1]
    return "\n".join(line[10:] for line in body.splitlines()) + "\n"


def job_section(name, source=None):
    source = source or workflow("release.yml")
    return re.split(r"\n  [a-z-]+:\n", source.split(f"\n  {name}:\n", 1)[1], maxsplit=1)[0]


class MetadataPackage(TypedDict):
    id: str
    name: str
    version: str
    license: str | None
    authors: list[str]
    manifest_path: str


class DependencyKind(TypedDict):
    kind: str | None


class Dependency(TypedDict):
    pkg: str
    dep_kinds: list[DependencyKind]


class MetadataNode(TypedDict):
    id: str
    deps: list[Dependency]


class MetadataResolve(TypedDict):
    nodes: list[MetadataNode]


class MetadataFixture(TypedDict):
    packages: list[MetadataPackage]
    workspace_members: list[str]
    resolve: MetadataResolve


class WorkflowTests(unittest.TestCase):
    def shell(self, script, env=None, cargo_body="printf '0.1.0\\n'", jq_body="if [[ \"$*\" == *packages* ]]; then cat >/dev/null; fi; printf '0.1.0\\n'"):
        with tempfile.TemporaryDirectory(prefix="release-fixture-") as directory:
            directory = Path(directory)
            for name, body in (("cargo", cargo_body), ("jq", jq_body)):
                file = directory / name
                file.write_text("#!/bin/bash\n" + body + "\n")
                file.chmod(0o755)
            values = dict(os.environ, PATH=f"{directory}:{os.environ['PATH']}",
                          GITHUB_OUTPUT=str(directory / "output"),
                          GITHUB_STEP_SUMMARY=str(directory / "summary"), OBS_TAG="0.1.0")
            values.update(env or {})
            result = subprocess.run(["bash", "-c", script], env=values, text=True, capture_output=True)
            output = directory / "output"
            return result, output.read_text() if output.exists() else ""

    VERSION_STEP = "Resolve the version and whether this run publishes"

    def test_exact_publication_refs(self):
        script = step("release.yml", self.VERSION_STEP)
        for ref, ref_type, accepted in (("v0.1.0", "tag", True), ("0.1.0", "tag", False),
                                         ("rs-v0.1.0", "tag", False), ("v0.1.0", "branch", False),
                                         ("v0.2.0", "tag", False)):
            with self.subTest(ref=ref, ref_type=ref_type):
                result, output = self.shell(script, dict(TAG=ref, REF_TYPE=ref_type,
                    INPUT_PUBLISH="true", GITHUB_EVENT_NAME="workflow_dispatch"))
                self.assertEqual(result.returncode == 0, accepted, result.stdout + result.stderr)
                if accepted:
                    self.assertIn("publish=true", output)

    def test_rehearsal_describes_external_effects(self):
        source = workflow("release.yml")
        self.assertNotIn("nothing was published", source)
        self.assertIn("No GitHub release, Homebrew update or pacman repository was published", source)
        self.assertIn("Workflow artifacts were uploaded", source)
        self.assertIn("signing and notarization may contact Apple", source)

    def test_push_requires_exact_publication_ref(self):
        script = step("release.yml", self.VERSION_STEP)
        for tag, accepted in (("v0.1.0", True), ("0.1.0", False)):
            with self.subTest(tag=tag):
                result, output = self.shell(script, dict(TAG=tag, REF_TYPE="tag", INPUT_PUBLISH="",
                    GITHUB_EVENT_NAME="push"))
                self.assertEqual(result.returncode == 0, accepted)
                if accepted:
                    self.assertIn("publish=true", output)

    def test_dry_run_and_rehearsal_branch_never_publish(self):
        script = step("release.yml", self.VERSION_STEP)
        for event, publish in (("workflow_dispatch", "false"), ("push", "")):
            with self.subTest(event=event):
                result, output = self.shell(script, dict(TAG="release-rehearsal", REF_TYPE="branch",
                    INPUT_PUBLISH=publish, GITHUB_EVENT_NAME=event))
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("publish=false", output)

    def test_plugin_version_must_match_the_cli(self):
        script = step("release.yml", self.VERSION_STEP)
        jq = ("if [[ \"$*\" == *packages* ]]; then cat >/dev/null; printf '0.1.0\\n'; "
              "elif [[ \"$*\" == '-r .version obs/buildspec.json' ]]; then printf '0.2.0\\n'; "
              "else printf '0.1.0\\n'; fi")
        result, _ = self.shell(script, dict(TAG="release-rehearsal", REF_TYPE="branch", INPUT_PUBLISH="",
            GITHUB_EVENT_NAME="push"), jq_body=jq)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("obs/buildspec.json has version 0.2.0", result.stdout + result.stderr)

    def test_source_date_epoch_is_read_once_and_never_empty(self):
        script = step("release.yml", "Read the commit timestamp for reproducible archives")
        with tempfile.TemporaryDirectory(prefix="epoch-fixture-") as directory:
            env = dict(os.environ, **ISOLATED_GIT, GITHUB_OUTPUT=str(Path(directory) / "output"))
            result = subprocess.run(["bash", "-c", script], cwd=directory, env=env, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0, "an unreadable repository gave an epoch")
            git = lambda *args: subprocess.run(["git", "-C", directory, *args], check=True, capture_output=True,
                                               env=env)
            env["GIT_COMMITTER_DATE"] = "@1700000000"
            git("init", "-q")
            git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "base")
            result = subprocess.run(["bash", "-c", script], cwd=directory, env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual((Path(directory) / "output").read_text(), "source_date_epoch=1700000000\n")
        pin = step("release.yml", "Pin build paths and timestamps")
        for epoch, accepted in (("", False), ("@", False), ("17000x", False), ("1700000000", True)):
            with self.subTest(epoch=epoch), tempfile.TemporaryDirectory(prefix="epoch-fixture-") as directory:
                env = dict(os.environ, SOURCE_DATE_EPOCH=epoch, RUNNER_OS="Linux", GITHUB_WORKSPACE=directory,
                           GITHUB_ENV=str(Path(directory) / "env"))
                result = subprocess.run(["bash", "-c", pin], env=env, text=True, capture_output=True)
                self.assertEqual(result.returncode == 0, accepted, result.stdout + result.stderr)
        source = workflow("release.yml")
        self.assertNotIn("git log", source.split("\n  ci-gate:\n", 1)[1])
        for job in ("cli", "obs-linux", "arch"):
            with self.subTest(job=job):
                self.assertIn("SOURCE_DATE_EPOCH: ${{ needs.version.outputs.source_date_epoch }}", job_section(job))
        for job in ("cli", "obs-linux"):
            with self.subTest(job=job):
                self.assertIn('--mtime="@${SOURCE_DATE_EPOCH:?}"', job_section(job))

    def gate(self, obs_runs, change_obs, failed=""):
        """Runs the CI gate against a two-commit repo; obs_runs maps 'base'/'tip' to whether OBS ran there."""
        script = step("release.yml", "Wait for CI on the tagged commit")
        with tempfile.TemporaryDirectory(prefix="gate-fixture-") as directory:
            repo = Path(directory) / "repo"
            (repo / ".github/workflows").mkdir(parents=True)
            (repo / "obs").mkdir()
            (repo / ".github/workflows/obs-ci.yml").write_text((ROOT / ".github/workflows/obs-ci.yml").read_text())
            (repo / "obs/plugin.c").write_text("1\n")
            git = lambda *args: subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True,
                                               env=dict(os.environ, **ISOLATED_GIT)).stdout.strip()
            git("init", "-q"); git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qam", "base", "--allow-empty")
            git("add", "."); git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "base")
            base = git("rev-parse", "HEAD")
            (repo / ("obs/plugin.c" if change_obs else "README.md")).write_text("2\n")
            git("add", "."); git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "tip")
            tip = git("rev-parse", "HEAD")
            lines = {"base": f"{base}\tcompleted\tsuccess\turl-base", "tip": f"{tip}\tcompleted\tsuccess\turl-tip"}
            gh = f"""case "$*" in
              *commits/*) echo {tip};;
              *obs-ci.yml*head_sha=*) {"echo '" + lines["tip"] + "'" if obs_runs.get("tip") else ":"};;
              *obs-ci.yml*status=success*) {"echo '" + lines["base"] + "'" if obs_runs.get("base") else ":"};;
              *{failed or "never-matches"}*) printf '%s\\tcompleted\\tfailure\\turl\\n' {tip};;
              *head_sha=*) echo '{lines["tip"]}';;
            esac"""
            stubs = Path(directory) / "bin"
            stubs.mkdir()
            for name, body in (("gh", gh),):
                (stubs / name).write_text("#!/bin/bash\n" + body + "\n")
                (stubs / name).chmod(0o755)
            env = dict(os.environ, PATH=f"{stubs}:{os.environ['PATH']}", REF="refs/tags/v0.1.0",
                       GITHUB_REPOSITORY="o/r", **ISOLATED_GIT)
            return subprocess.run(["bash", "-c", script], cwd=repo, env=env, text=True, capture_output=True)

    def test_gate_passes_when_every_workflow_passed_on_the_commit(self):
        result = self.gate({"tip": True}, change_obs=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_gate_accepts_an_obs_ancestor_only_when_its_paths_are_unchanged(self):
        result = self.gate({"base": True}, change_obs=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("paths are unchanged since", result.stdout)
        result = self.gate({"base": True}, change_obs=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("its paths changed", result.stdout + result.stderr)

    def test_gate_rejects_a_failed_workflow(self):
        result = self.gate({"tip": True}, change_obs=True, failed="rust-ci.yml")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rust-ci.yml finished failure", result.stdout + result.stderr)

    def test_universal_checks_avoid_verify_arch(self):
        for path in (ROOT / ".github/workflows/release.yml", ROOT / "scripts/release.sh",
                     ROOT / "scripts/build.sh"):
            with self.subTest(path=path):
                self.assertNotIn("-verify_arch", path.read_text())
        self.assertIn("check-universal.sh target/universal/dstreamy", workflow("release.yml"))
        self.assertIn("check-universal.sh", (ROOT / "scripts/release.sh").read_text())

    def test_normal_vendor_features_are_rejected(self):
        script = step("rust-ci.yml", "Shipped crates never enable test-only features")
        for feature in ("test-hooks", "test-rtp-start"):
            body = f'''case " $* " in
              *" -i discord-stream-rs "*) printf 'discord-stream-rs feature "{feature}"\\n';;
              *) printf 'clean production graph\\n';;
            esac'''
            with self.subTest(feature=feature):
                result, _ = self.shell(script, cargo_body=body)
                self.assertNotEqual(result.returncode, 0, "direct vendor feature escaped normal graph guard")
        result, _ = self.shell(script, cargo_body="printf 'clean production graph\\n'")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_swift_guard_checks_default_production_graph(self):
        script = step("release.yml", "Refuse test-only features in the bundled daemon")
        self.assertNotIn("--features browser-login", script)
        self.assertIn("aarch64-apple-darwin", script)
        self.assertIn("x86_64-apple-darwin", script)
        for feature in ("test-hooks", "test-rtp-start", "test-support", "test-pattern", "openh264"):
            result, _ = self.shell(script, cargo_body=f'''printf 'discord-stream-rs feature "{feature}"\\n' ''')
            self.assertNotEqual(result.returncode, 0)
        result, _ = self.shell(script, cargo_body="printf 'clean production graph\\n'")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_cli_notice_features_match_release(self):
        package = step("release.yml", "Package the CLI")
        self.assertRegex(package, r"--package dstreamy-cli\s+--features browser-login")
        self.assertIn('--target "$TARGET"', package)
        self.assertNotIn("--features browser-login", (ROOT / "scripts/build.sh").read_text())

    def test_obs_release_layout_matches_installer_and_loader(self):
        cmake = (ROOT / "obs/ui/CMakeLists.txt").read_text()
        loader = (ROOT / "rust/crates/dstreamy-obs/src/ui.rs").read_text()
        folder_match = re.search(r'const FOLDER: &str = "([^"]+)";', loader)
        self.assertIsNotNone(folder_match)
        assert folder_match is not None
        folder = folder_match.group(1)
        source = workflow("release.yml")
        for platform, base, extension, install_path in (
            ("linux", "$plugins", "so", "${CMAKE_INSTALL_LIBDIR}/obs-plugins/"),
            ("windows", "$bin", "dll", "${CMAKE_PROJECT_NAME}/bin/64bit/")):
            section = re.split(r"\n  [a-z-]+:\n", source.split(f"\n  obs-{platform}:\n", 1)[1], maxsplit=1)[0]
            with self.subTest(platform=platform):
                self.assertIn('DESTINATION "' + install_path + folder + '"', cmake)
                self.assertIn(folder + "/obs-dstreamy-ui." + extension, section)
                notice = base + "/" + folder + "/THIRD_PARTY_NOTICES.txt"
                self.assertIn('--append "' + notice + '"', section)
                self.assertIn('--output "' + notice + '"', section)

    def assets(self, directory, version="0.1.0", plugin_exe=True):
        dist = Path(directory)
        def archive(name, entries):
            path = dist / name
            if name.endswith(".zip"):
                with zipfile.ZipFile(path, "w") as z:
                    for entry in entries:
                        z.writestr(entry, "x")
            else:
                with tarfile.open(path, "w:gz") as t:
                    for entry in entries:
                        info = tarfile.TarInfo(entry)
                        info.size = 1
                        t.addfile(info, io.BytesIO(b"x"))
            (dist / (name + ".sha256")).write_text(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {name}\n")
        cli = ["README.md", "LICENSE", "THIRD_PARTY_NOTICES.txt"]
        for target in ("aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-unknown-linux-gnu",
                       "aarch64-unknown-linux-gnu"):
            stem = f"dstreamy-{version}-{target}"
            archive(stem + ".tar.gz", [f"{stem}/{e}" for e in ["dstreamy", *cli]])
        stem = f"dstreamy-{version}-x86_64-pc-windows-msvc"
        archive(stem + ".zip", [f"{stem}/{e}" for e in ["dstreamy.exe", *cli]])
        archive(f"D-Streamy-{version}-macos-universal.zip", ["D-Streamy.app/Contents/MacOS/dstreamy"])
        archive(f"obs-dstreamy-{version}-linux-x86_64.tar.gz", ["./lib/x86_64-linux-gnu/obs-plugins/obs-dstreamy.so"])
        archive(f"obs-dstreamy-{version}-macos-universal.zip", ["obs-dstreamy.plugin/Contents/MacOS/obs-dstreamy"])
        plugin = ["obs-dstreamy.dll", "dstreamy-ui/obs-dstreamy-ui.dll", "dstreamy-ui/THIRD_PARTY_NOTICES.txt"]
        if plugin_exe:
            plugin.append("dstreamy.exe")
        archive(f"obs-dstreamy-{version}-windows-x64.zip", [f"obs-dstreamy/bin/64bit/{e}" for e in plugin])
        return subprocess.run([str(ROOT / "scripts/check-release-assets.sh"), version, str(dist)],
                              text=True, capture_output=True)

    def test_release_assets_include_the_windows_cli_and_bundle_it_in_the_plugin(self):
        with tempfile.TemporaryDirectory(prefix="assets-fixture-") as directory:
            result = self.assets(directory)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        with tempfile.TemporaryDirectory(prefix="assets-fixture-") as directory:
            result = self.assets(directory, plugin_exe=False)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("lacks dstreamy.exe", result.stderr)

    def test_windows_plugin_ships_the_released_cli(self):
        source = workflow("release.yml")
        self.assertNotIn("Hold back the Windows plugin", source)
        cli = re.split(r"\n  [a-z-]+:\n", source.split("\n  cli:\n", 1)[1], maxsplit=1)[0]
        self.assertIn("- target: x86_64-pc-windows-msvc", cli)
        section = re.split(r"\n  [a-z-]+:\n", source.split("\n  obs-windows:\n", 1)[1], maxsplit=1)[0]
        self.assertRegex(section, r"needs: \[[^\]]*\bcli\b")
        self.assertIn("name: dstreamy-${{ needs.version.outputs.version }}-x86_64-pc-windows-msvc", section)
        self.assertIn('sha256sum -c "$ARCHIVE.zip.sha256"', section)
        self.assertIn('--append "$(cygpath -u "$RUNNER_TEMP")/cli/out/THIRD_PARTY_NOTICES.txt"', section)

    def test_obs_notices_each_platform_before_signing(self):
        source = workflow("release.yml")
        for platform, target, destination in (
            ("linux", "x86_64-unknown-linux-gnu", '$plugins/dstreamy-ui/THIRD_PARTY_NOTICES.txt'),
            ("macos", "aarch64-apple-darwin", 'Resources/THIRD_PARTY_NOTICES.txt'),
            ("windows", "x86_64-pc-windows-msvc", '$bin/dstreamy-ui/THIRD_PARTY_NOTICES.txt')):
            section = re.split(r"\n  [a-z-]+:\n", source.split(f"\n  obs-{platform}:\n", 1)[1], maxsplit=1)[0]
            with self.subTest(platform=platform):
                self.assertIn("third-party-notices.py", section)
                self.assertIn("--package dstreamy-obs", section)
                self.assertIn("rust/crates/dstreamy-obs/Cargo.toml", section)
                self.assertIn("--target " + target, section)
                self.assertIn('--append "' + destination + '"', section)
                if platform == "macos":
                    self.assertIn("--target x86_64-apple-darwin", section)
                    self.assertLess(section.index("third-party-notices.py"), section.index("codesign --force"))

    def test_runs_the_release_gate_waits_for_are_never_cancelled_on_main(self):
        for name in ("ci.yml", "rust-ci.yml", "fuzz.yml", "obs-ci.yml"):
            with self.subTest(workflow=name):
                self.assertIn("\nconcurrency:\n", workflow(name))
                cancel = workflow(name).split("\nconcurrency:\n", 1)[1].split("cancel-in-progress:", 1)[1]
                self.assertEqual(cancel.split("\n", 1)[0].strip(), "${{ github.ref != 'refs/heads/main' }}")
                group = workflow(name).split("\nconcurrency:\n", 1)[1].split("group:", 1)[1].split("\n", 1)[0]
                self.assertIn("${{ github.ref == 'refs/heads/main' && github.sha || github.ref }}", group,
                              "a queued main run would replace the release commit's pending run")

    FORK_GUARD = ("if: github.event_name != 'pull_request' || "
                  "github.event.pull_request.head.repo.full_name == github.repository")

    def test_fork_pull_requests_never_reach_the_signing_runner(self):
        """xcode-27 also holds the Developer ID material during releases, so fork code must not run there."""
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            source = path.read_text()
            if "pull_request" not in source:
                continue
            for job in re.split(r"\n(?=  [a-z][a-z-]*:\n)", source.split("\njobs:\n", 1)[1]):
                if "xcode-27" not in job:
                    continue
                with self.subTest(workflow=path.name, job=job.split(":", 1)[0].strip()):
                    self.assertRegex(job, r"^  [a-z][a-z-]*:\n    runs-on: xcode-27\n    " + re.escape(self.FORK_GUARD))


    def test_fork_pull_requests_never_reach_a_self_hosted_matrix_runner(self):
        same_repo = "github.event.pull_request.head.repo.full_name == github.repository"
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            source = path.read_text()
            if "pull_request" not in source:
                continue
            for number, line in enumerate(source.splitlines(), 1):
                if "ubuntu-26.04" in line and not line.lstrip().startswith("#"):
                    with self.subTest(workflow=path.name, line=number):
                        self.assertIn(same_repo, line)


class TagReleaseTests(unittest.TestCase):
    RELEASE_FILES = ["obs/buildspec.json", "rust/Cargo.lock", "rust/Cargo.toml",
                     "rust/crates/dstreamy-obs/Cargo.lock", "rust/crates/dstreamy-obs/Cargo.toml",
                     "rust/fuzz/Cargo.lock"]

    def set_version(self, version):
        """Runs tag-release.sh's version-setting block on a git copy of the release files, with cargo stubbed."""
        source = (ROOT / "scripts/tag-release.sh").read_text()
        before = source.split('\nset_version "${VERSION#v}"\n', 1)[0]
        block = before.rsplit("\nesac\n", 1)[1]
        directory = tempfile.TemporaryDirectory(prefix="tag-release-fixture-")
        self.addCleanup(directory.cleanup)
        repo = Path(directory.name) / "repo"
        for name in self.RELEASE_FILES:
            (repo / name).parent.mkdir(parents=True, exist_ok=True)
            (repo / name).write_bytes((ROOT / name).read_bytes())
        git = lambda *args: subprocess.run(["git", "-C", str(repo), *args], check=True,
                                           capture_output=True, text=True, env=dict(os.environ, **ISOLATED_GIT)).stdout
        git("init", "-q")
        git("add", ".")
        git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "base")
        stubs = Path(directory.name) / "bin"
        stubs.mkdir()
        log = Path(directory.name) / "cargo.log"
        (stubs / "cargo").write_text(f'#!/bin/bash\necho "${{PWD#{repo}}} $*" >> {log}\n'
                                     'if [[ "$1" == update ]]; then echo "# relocked" >> Cargo.lock; fi\n')
        (stubs / "cargo").chmod(0o755)
        env = dict(os.environ, PATH=f"{stubs}:{os.environ['PATH']}", **ISOLATED_GIT)
        result = subprocess.run(["bash", "-euo", "pipefail", "-c", block + f"\nset_version {version}\n"],
                                cwd=repo, env=env, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return repo, git, log.read_text().splitlines()

    def test_set_version_relocks_and_stages_every_lockfile_pinning_the_workspace(self):
        _, git, cargo = self.set_version("0.2.0")
        self.assertEqual(cargo, ["/rust update --workspace --quiet",
                                 "/rust/crates/dstreamy-obs update --workspace --quiet",
                                 "/rust/fuzz update --workspace --quiet"])
        self.assertEqual(git("diff", "--name-only"), "", "set_version left changes unstaged")
        self.assertEqual(git("diff", "--cached", "--name-only").split(), self.RELEASE_FILES)

    def test_set_version_passes_the_ci_version_checks(self):
        repo, _, _ = self.set_version("0.2.0")
        result = subprocess.run(["bash", "-c", step("obs-ci.yml", "Plugin workspace settings match rust/Cargo.toml")],
                                cwd=repo, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        main = tomllib.loads((repo / "rust/Cargo.toml").read_text())["workspace"]["package"]["version"]
        self.assertEqual(main, "0.2.0")
        self.assertEqual(json.loads((repo / "obs/buildspec.json").read_text())["version"], main)

    def release_repo(self):
        """A clone of the release files whose origin is a local bare repository, with cargo stubbed."""
        directory = tempfile.TemporaryDirectory(prefix="tag-release-push-")
        self.addCleanup(directory.cleanup)
        base = Path(directory.name)
        repo, remote = base / "repo", base / "remote.git"
        for name in [*self.RELEASE_FILES, "scripts/tag-release.sh"]:
            (repo / name).parent.mkdir(parents=True, exist_ok=True)
            (repo / name).write_bytes((ROOT / name).read_bytes())
        env = dict(os.environ, **ISOLATED_GIT, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t",
                   GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@t")
        def git(*args, cwd=repo):
            return subprocess.run(["git", "-C", str(cwd), *args], check=True, capture_output=True, text=True,
                                  env=env).stdout.strip()
        git("init", "-q", "--bare", "-b", "main", str(remote), cwd=base)
        git("init", "-q", "-b", "main")
        git("add", ".")
        git("commit", "-qm", "base")
        git("tag", "-a", "v0.1.0", "-m", "Release v0.1.0")
        git("remote", "add", "origin", str(remote))
        git("push", "-q", "origin", "main", "v0.1.0")
        stubs = base / "bin"
        stubs.mkdir()
        (stubs / "cargo").write_text("#!/bin/bash\nif [[ \"$1\" == update ]]; then echo \"# $PWD\" >> Cargo.lock; fi\n")
        (stubs / "cargo").chmod(0o755)
        env["PATH"] = f"{stubs}:{os.environ['PATH']}"
        self.stubs = stubs
        run = lambda: subprocess.run(["bash", "scripts/tag-release.sh", "patch"], cwd=repo, env=env, input="y\n",
                                     text=True, capture_output=True)
        return remote, git, run

    def test_push_that_landed_but_reported_failure_counts_as_pushed(self):
        remote, git, run = self.release_repo()
        real_git = shutil.which("git")
        (self.stubs / "git").write_text(f'#!/bin/bash\n{real_git} "$@"; status=$?\n'
                                        '[ "$1" = push ] && [ "$2" = --atomic ] && exit 1\nexit $status\n')
        (self.stubs / "git").chmod(0o755)
        result = run()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("treating it as pushed", result.stderr)
        self.assertNotEqual(git("tag", "-l", "v0.1.1"), "", "local tag deleted although origin has it")
        tagged = git("rev-parse", "v0.1.1^{commit}", cwd=remote)
        self.assertEqual(git("rev-parse", "main", cwd=remote), tagged)

    def test_rejected_branch_push_leaves_no_remote_tag_and_a_rerun_succeeds(self):
        remote, git, run = self.release_repo()
        hook = remote / "hooks/update"
        hook.write_text('#!/bin/sh\n[ "$1" != refs/heads/main ] || { echo "protected branch" >&2; exit 1; }\n')
        hook.chmod(0o755)
        result = run()
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(git("ls-remote", "--tags", str(remote), "refs/tags/v0.1.1"), "",
                         "tag reached origin without its release commit")
        self.assertEqual(git("tag", "-l", "v0.1.1"), "", "failed push left the local tag behind")
        self.assertIn("rerun", result.stderr)
        hook.unlink()
        result = run()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        tagged = git("rev-parse", "v0.1.1^{commit}", cwd=remote)
        self.assertEqual(git("rev-parse", "main", cwd=remote), tagged)
        self.assertEqual(git("log", "-1", "--format=%s", tagged, cwd=remote), "chore: release v0.1.1")


FAKE_SECURITY = r"""#!PYTHON
import os, shlex, sys
state = os.environ["FAKE_SECURITY_STATE"]
def log(name, text):
    with open(os.path.join(state, name), "a") as f:
        print(text, file=f)
def search():
    with open(os.path.join(state, "search")) as f:
        return [line.rstrip("\n") for line in f if line.strip()]
def run(args):
    command, rest = args[0], args[1:]
    if command == "list-keychains":
        if "-s" in rest:
            with open(os.path.join(state, "search"), "w") as f:
                f.writelines(k + "\n" for k in rest[rest.index("-s") + 1:])
        else:
            for k in search():
                print(f'    "{k}"')
    elif command == "create-keychain":
        open(rest[-1], "w").close()
    elif command == "delete-keychain":
        os.remove(rest[-1])
        remaining = [k for k in search() if k != rest[-1]]
        with open(os.path.join(state, "search"), "w") as f:
            f.writelines(k + "\n" for k in remaining)
    elif command == "import":
        log("imported", rest[rest.index("-P") + 1])
    elif command == "find-identity":
        print(f"     {os.environ.get('FAKE_IDENTITIES', '1')} valid identities found")
    elif command == "find-certificate":
        sys.exit(0 if os.environ.get("FAKE_INTERMEDIATE", "1") == "1" else 44)
log("argv", shlex.join(sys.argv[1:]))
if sys.argv[1:] == ["-i"]:
    for line in sys.stdin:
        log("stdin", line.rstrip("\n"))
        run(shlex.split(line))
else:
    run(sys.argv[1:])
"""


@unittest.skipUnless(shutil.which("node"), "node runs the action's entry points")
class SigningKeychainTests(unittest.TestCase):
    ACTION = ROOT / ".github/actions/macos-signing-keychain"
    CERT_PASSWORD = 'p"a ss\\word'
    LOGIN = "/Users/runner/Library/Keychains/login.keychain-db"

    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix="keychain-fixture-")
        self.addCleanup(directory.cleanup)
        base = Path(directory.name)
        self.state, self.temp, stubs = base / "state", base / "runner temp", base / "bin"
        for path in (self.state, self.temp, stubs):
            path.mkdir()
        (self.state / "search").write_text(self.LOGIN + "\n")
        (stubs / "security").write_text(FAKE_SECURITY.replace("PYTHON", sys.executable))
        (stubs / "security").chmod(0o755)
        self.github_env = base / "github env"
        self.env = dict(os.environ, PATH=f"{stubs}:{os.environ['PATH']}", RUNNER_TEMP=str(self.temp),
                        GITHUB_ENV=str(self.github_env),
                        FAKE_SECURITY_STATE=str(self.state), **{"INPUT_CERT-P12": "cDEyLWJ5dGVz",
                                                                "INPUT_CERT-PASSWORD": self.CERT_PASSWORD})

    def node(self, script, **env):
        return subprocess.run(["node", str(self.ACTION / script)], env=dict(self.env, **env), text=True,
                              capture_output=True)

    def search(self):
        return (self.state / "search").read_text().split("\n")[:-1]

    def test_post_deletes_the_keychain_and_never_touches_the_search_list(self):
        action = (self.ACTION / "action.yml").read_text()
        self.assertIn("post: post.cjs", action)
        result = self.node("main.cjs")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        keychain = str(self.temp / "signing-keychain/signing.keychain-db")
        self.assertEqual(self.search(), [self.LOGIN])
        self.assertEqual(self.github_env.read_text(), f"SIGNING_KEYCHAIN={keychain}\n")
        self.assertEqual((self.state / "imported").read_text(), self.CERT_PASSWORD + "\n")
        self.assertFalse((self.temp / "signing-keychain/cert.p12").exists())
        argv = (self.state / "argv").read_text()
        self.assertNotIn("ss\\word", argv)
        for command in ("create-keychain", "unlock-keychain", "import", "set-key-partition-list"):
            self.assertNotIn(command, argv, f"{command} carries a password and must not be on argv")
        result = self.node("post.cjs")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.search(), [self.LOGIN])
        self.assertFalse((self.temp / "signing-keychain").exists())
        self.assertNotIn("list-keychains -d user -s", (self.state / "argv").read_text())

    def test_password_with_a_line_break_is_rejected_before_security_runs(self):
        for password in ("secret\n", "sec\r\nret"):
            with self.subTest(password=password):
                result = self.node("main.cjs", **{"INPUT_CERT-PASSWORD": password})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("contains a line break", result.stderr)
                self.assertFalse((self.state / "argv").exists(), "security ran with a broken password")

    def test_missing_intermediate_fails_with_an_actionable_error(self):
        result = self.node("main.cjs", FAKE_INTERMEDIATE="0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Developer ID G2 intermediate", result.stderr)

    def test_every_codesign_uses_the_imported_keychain(self):
        sources = {"release.yml": workflow("release.yml"), "release.sh": (ROOT / "scripts/release.sh").read_text()}
        for name, source in sources.items():
            calls = re.findall(r"codesign [^\n]*", re.sub(r"\\\n\s*", " ", source))
            signing = [call for call in calls if "--sign" in call]
            self.assertTrue(signing, name)
            for call in signing:
                with self.subTest(source=name, call=call):
                    expected = '${SIGNING_KEYCHAIN:+--keychain "$SIGNING_KEYCHAIN"}' if name == "release.sh" \
                        else '--keychain "$SIGNING_KEYCHAIN"'
                    self.assertIn(expected, call)

    def test_failed_import_still_cleans_up(self):
        result = self.node("main.cjs", FAKE_IDENTITIES="0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No valid codesigning identity", result.stderr)
        self.assertFalse((self.temp / "signing-keychain/cert.p12").exists())
        result = self.node("post.cjs")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.search(), [self.LOGIN])
        self.assertFalse((self.temp / "signing-keychain").exists())

    def test_notary_key_is_removed_on_every_exit(self):
        script = (ROOT / ".github/actions/macos-notarize/action.yml").read_text()
        self.assertRegex(script, r"key=\"\$RUNNER_TEMP/notary.p8\"\n\s+trap 'rm -f \"\$key\"' EXIT\n")


FAKE_GPG = r"""#!/bin/bash
printf '%s\n' "$(basename "$0") $*" >> "$STUB_LOG/argv"
printf '%s\n' "$GNUPGHOME" >> "$STUB_LOG/gnupghome"
case "$(basename "$0") $*" in
  "gpg --batch --quiet --import") cat > "$STUB_LOG/imported" ;;
  "gpg --batch --with-colons --list-secret-keys") echo "fpr:::::::::$STUB_FINGERPRINT:" ;;
  *--export*) echo "PUBLIC KEY" ;;
  *--detach-sign*) touch "${@: -1}.sig" ;;
  repo-add*) exit "${STUB_REPO_ADD_STATUS:-0}" ;;
esac
"""


class ArchRepoTests(unittest.TestCase):
    KEY = "-----BEGIN PGP PRIVATE KEY BLOCK-----\nsecret key material\n-----END PGP PRIVATE KEY BLOCK-----"
    FINGERPRINT = "0123456789ABCDEF0123456789ABCDEF01234567"
    PKGBUILDS = {"dstreamy-bin": {"x86_64": "x86_64-unknown-linux-gnu", "aarch64": "aarch64-unknown-linux-gnu"},
                 "obs-dstreamy-bin": {"": "linux-x86_64"}}

    def sign(self, repo_add_status=0, key=KEY):
        """Runs make-arch-repo.sh --sign on a two-architecture repo with gpg, gpgconf and repo-add stubbed."""
        directory = tempfile.TemporaryDirectory(prefix="arch-repo-fixture-")
        self.addCleanup(directory.cleanup)
        base = Path(directory.name)
        stubs, log, tmp, repo = base / "bin", base / "log", base / "tmp", base / "repo"
        for path in (stubs, log, tmp, repo / "x86_64", repo / "aarch64"):
            path.mkdir(parents=True)
        for name in ("gpg", "gpgconf", "repo-add"):
            (stubs / name).write_text(FAKE_GPG)
            (stubs / name).chmod(0o755)
        (repo / "x86_64/dstreamy-bin-0.1.0-1-x86_64.pkg.tar.zst").write_text("x")
        (repo / "aarch64/dstreamy-bin-0.1.0-1-aarch64.pkg.tar.zst").write_text("a")
        env = dict(os.environ, PATH=f"{stubs}:{os.environ['PATH']}", TMPDIR=str(tmp), STUB_LOG=str(log),
                   STUB_FINGERPRINT=self.FINGERPRINT, STUB_REPO_ADD_STATUS=str(repo_add_status),
                   PACMAN_SIGNING_KEY=key)
        result = subprocess.run([str(ROOT / "scripts/make-arch-repo.sh"), str(repo), "--sign"], env=env, text=True,
                                capture_output=True)
        return result, base

    def test_signing_key_reaches_gpg_only_on_stdin(self):
        result, base = self.sign()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((base / "log/imported").read_text(), self.KEY + "\n")
        argv = (base / "log/argv").read_text()
        self.assertNotIn("secret key material", argv)
        self.assertNotIn("PRIVATE KEY", argv)
        self.assertNotIn("secret key material", result.stdout + result.stderr)
        for arch in ("x86_64", "aarch64"):
            with self.subTest(arch=arch):
                self.assertIn(f"--detach-sign --no-armor --local-user {self.FINGERPRINT} {base}/repo/{arch}/", argv)
                self.assertIn(f"repo-add --sign --key {self.FINGERPRINT} {base}/repo/{arch}/d-streamy.db.tar.zst", argv)
                self.assertTrue((base / f"repo/{arch}/dstreamy-bin-0.1.0-1-{arch}.pkg.tar.zst.sig").exists())
        self.assertEqual((base / "repo/d-streamy.asc").read_text(), "PUBLIC KEY\n")

    def test_gnupghome_is_removed_after_success_and_failure(self):
        for status in (0, 1):
            with self.subTest(repo_add_status=status):
                result, base = self.sign(repo_add_status=status)
                self.assertEqual(result.returncode == 0, status == 0, result.stdout + result.stderr)
                homes = set((base / "log/gnupghome").read_text().split())
                self.assertEqual(len(homes), 1, homes)
                self.assertTrue(homes.pop().startswith(str(base / "tmp") + "/"), "GNUPGHOME is not a fresh temp dir")
                self.assertEqual(list((base / "tmp").iterdir()), [], "GNUPGHOME survived the run")

    def test_signing_without_a_key_fails_before_gpg_runs(self):
        result, base = self.sign(key="")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("PACMAN_SIGNING_KEY is not set", result.stderr)
        self.assertFalse((base / "log/argv").exists())

    def test_workflow_hands_the_key_to_the_script_only_through_env(self):
        source = workflow("release.yml")
        self.assertEqual(source.count("secrets.PACMAN_SIGNING_KEY"), 1)
        pages = job_section("pages")
        self.assertIn("        env:\n          PACMAN_SIGNING_KEY: ${{ secrets.PACMAN_SIGNING_KEY }}\n"
                      "        run: scripts/make-arch-repo.sh site/arch --sign\n", pages)
        self.assertNotIn("PACMAN_SIGNING_KEY", job_section("arch"))

    def test_rehearsals_never_deploy_pages(self):
        source = workflow("release.yml")
        top = source.split("\njobs:\n", 1)[0]
        self.assertNotIn("pages:", top)
        jobs = re.findall(r"\n  ([a-z][a-z-]*):\n", source.split("\njobs:\n", 1)[1])
        for job in jobs:
            section = job_section(job)
            if job == "pages":
                continue
            with self.subTest(job=job):
                for marker in ("deploy-pages", "upload-pages-artifact", "pages: write", "github-pages"):
                    self.assertNotIn(marker, section)
        pages = job_section("pages")
        self.assertRegex(pages, r"^    needs: \[[^\]]*\brelease\b")
        self.assertIn("\n    if: needs.version.outputs.publish == 'true'\n", pages)
        self.assertIn("\n      pages: write\n", pages)
        self.assertIn("actions/deploy-pages@", pages)
        self.assertRegex(job_section("rehearse"), r"needs: \[[^\]]*\barch\b")
        self.assertNotIn("\n    if:", job_section("arch"))
        self.assertNotIn("--sign", job_section("arch"))

    def assets(self, dist, version="0.1.0"):
        sums = {}
        for package, targets in self.PKGBUILDS.items():
            for arch, target in targets.items():
                name = f"{package.removesuffix('-bin')}-{version}-{target}.tar.gz"
                sums[package, arch] = hashlib.sha256(name.encode()).hexdigest()
                (dist / f"{name}.sha256").write_text(f"{sums[package, arch]}  {name}\n")
        return sums

    def fill(self, package, dist, version="0.1.0"):
        return subprocess.run([str(ROOT / "scripts/fill-pkgbuild.sh"), version, str(dist),
                               str(ROOT / "packaging/arch" / package / "PKGBUILD")], text=True, capture_output=True)

    def test_fill_pkgbuild_puts_each_assets_sha256_in_its_array(self):
        with tempfile.TemporaryDirectory(prefix="pkgbuild-fixture-") as directory:
            dist = Path(directory)
            sums = self.assets(dist)
            for package, targets in self.PKGBUILDS.items():
                with self.subTest(package=package):
                    result = self.fill(package, dist)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    filled = result.stdout
                    self.assertIn("\npkgver=0.1.0\n", filled)
                    self.assertIn("\npkgrel=1\n", filled)
                    for arch in targets:
                        key = f"sha256sums_{arch}" if arch else "sha256sums"
                        self.assertIn(f"\n{key}=('{sums[package, arch]}')\n", filled)
                    self.assertNotRegex(filled, r"0{64}|1{64}")
                    template = (ROOT / "packaging/arch" / package / "PKGBUILD").read_text()
                    changed = [line for line in filled.splitlines() if line not in template.splitlines()]
                    self.assertEqual(len(changed), 1 + len(targets), changed)

    def test_fill_pkgbuild_refuses_missing_or_bad_checksums(self):
        with tempfile.TemporaryDirectory(prefix="pkgbuild-fixture-") as directory:
            dist = Path(directory)
            self.assets(dist)
            (dist / "dstreamy-0.1.0-aarch64-unknown-linux-gnu.tar.gz.sha256").unlink()
            result = self.fill("dstreamy-bin", dist)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("aarch64-unknown-linux-gnu.tar.gz.sha256", result.stderr)
            self.assertEqual(result.stdout, "")
            (dist / "obs-dstreamy-0.1.0-linux-x86_64.tar.gz.sha256").write_text("nope  obs\n")
            result = self.fill("obs-dstreamy-bin", dist)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("holds no sha256", result.stderr)
            result = self.fill("obs-dstreamy-bin", dist, version="v0.1.0")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("X.Y.Z", result.stderr)

    def test_srcinfo_matches_its_pkgbuild(self):
        for package in self.PKGBUILDS:
            with self.subTest(package=package):
                directory = ROOT / "packaging/arch" / package
                fields = subprocess.run(["bash", "-c", 'source PKGBUILD; printf "pkgver = %s\\n" "$pkgver"; '
                                         'for d in "${depends[@]}"; do printf "depends = %s\\n" "$d"; done; '
                                         'for p in "${provides[@]}"; do printf "provides = %s\\n" "$p"; done; '
                                         'for c in "${conflicts[@]}"; do printf "conflicts = %s\\n" "$c"; done'],
                                        cwd=directory, text=True, capture_output=True, check=True).stdout
                srcinfo = [line.strip() for line in (directory / ".SRCINFO").read_text().splitlines()]
                for line in fields.splitlines():
                    self.assertIn(line, srcinfo)
                name = package.removesuffix("-bin")
                self.assertIn(f"provides = {name}=", "\n".join(srcinfo))
                self.assertIn(f"conflicts = {name}", srcinfo)
        self.assertIn("depends = dstreamy", (ROOT / "packaging/arch/obs-dstreamy-bin/.SRCINFO").read_text())

    @unittest.skipUnless(shutil.which("makepkg") and shutil.which("bsdtar") and os.geteuid() != 0,
                         "needs makepkg as a non-root user")
    def test_built_packages_hold_the_assets_for_their_architecture(self):
        with tempfile.TemporaryDirectory(prefix="arch-build-fixture-") as directory:
            base = Path(directory)
            dist, out = base / "dist", base / "out"
            dist.mkdir()
            def archive(name, files):
                path = dist / name
                with tarfile.open(path, "w:gz") as t:
                    for entry, data in files.items():
                        info = tarfile.TarInfo(entry)
                        info.size, info.mode, info.mtime = len(data), 0o755, 1700000000
                        t.addfile(info, io.BytesIO(data))
                (dist / f"{name}.sha256").write_text(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {name}\n")
            for target, machine in (("x86_64-unknown-linux-gnu", b"\x3e\x00"),
                                    ("aarch64-unknown-linux-gnu", b"\xb7\x00")):
                stem = f"dstreamy-0.1.0-{target}"
                archive(f"{stem}.tar.gz", {f"{stem}/dstreamy": b"\x7fELF" + bytes(14) + machine,
                                           **{f"{stem}/{e}": b"x" for e in ("README.md", "LICENSE",
                                                                             "THIRD_PARTY_NOTICES.txt")}})
            plugins = "./lib/x86_64-linux-gnu/obs-plugins"
            archive("obs-dstreamy-0.1.0-linux-x86_64.tar.gz", {
                "./LICENSE": b"x", f"{plugins}/obs-dstreamy.so": b"x",
                f"{plugins}/dstreamy-ui/obs-dstreamy-ui.so": b"x",
                f"{plugins}/dstreamy-ui/THIRD_PARTY_NOTICES.txt": b"x"})
            env = dict(os.environ, SOURCE_DATE_EPOCH="1700000000")
            build = lambda: subprocess.run([str(ROOT / "scripts/build-arch-packages.sh"), "0.1.0", str(dist), str(out)],
                                           env=env, text=True, capture_output=True)
            result = build()
            self.assertEqual(result.returncode, 0, result.stdout[-3000:] + result.stderr[-3000:])
            listing = lambda pkg: subprocess.run(["bsdtar", "-tf", str(out / pkg)], text=True, capture_output=True,
                                                 check=True).stdout.split()
            self.assertIn("usr/bin/dstreamy", listing("aarch64/dstreamy-bin-0.1.0-1-aarch64.pkg.tar.zst"))
            self.assertIn("usr/lib/obs-plugins/dstreamy-ui/obs-dstreamy-ui.so",
                          listing("x86_64/obs-dstreamy-bin-0.1.0-1-x86_64.pkg.tar.zst"))
            shutil.rmtree(out)
            stem = "dstreamy-0.1.0-aarch64-unknown-linux-gnu"
            archive(f"{stem}.tar.gz", {f"{stem}/dstreamy": b"\x7fELF" + bytes(14) + b"\x3e\x00",
                                       **{f"{stem}/{e}": b"x" for e in ("README.md", "LICENSE",
                                                                         "THIRD_PARTY_NOTICES.txt")}})
            result = build()
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("dstreamy-bin-0.1.0-1-aarch64.pkg.tar.zst holds a dstreamy for ELF machine 3e00",
                          result.stderr)


class InstallTests(unittest.TestCase):
    TOOLS = ("bash", "dirname", "basename", "sed")
    CONFINED_TOOLS = ("cp", "rm")

    def install(self, *args, relative=False):
        """Runs install.sh from a copy into a temp destination, with PATH limited to stubs and TOOLS."""
        directory = tempfile.TemporaryDirectory(prefix="install-fixture-")
        self.addCleanup(directory.cleanup)
        base = Path(directory.name)
        (base / "scripts").mkdir()
        (base / "scripts/install.sh").write_text((ROOT / "scripts/install.sh").read_text())
        (base / "bin/D-Streamy.app").mkdir(parents=True)
        (base / "Applications").mkdir()
        log = base / "calls.log"
        stubs = base / "stubs"
        stubs.mkdir()
        for name in ("build.sh", "release.sh"):
            (base / "scripts" / name).write_text(
                f'#!/bin/bash\necho {name} >> {log}\necho {name} > "{base}/bin/D-Streamy.app/built-by"\n')
            (base / "scripts" / name).chmod(0o755)
        for name in ("pkill", "xattr"):
            (stubs / name).write_text(f'#!/bin/bash\necho "{name} $*" >> {log}\n')
            (stubs / name).chmod(0o755)
        for name in self.TOOLS:
            tool = shutil.which(name)
            assert tool is not None, name
            (stubs / name).symlink_to(tool)
        for name in self.CONFINED_TOOLS:
            tool = shutil.which(name)
            assert tool is not None, name
            (stubs / name).write_text(f"""#!{sys.executable}
import os, sys
base = os.path.realpath({str(base)!r})
for arg in sys.argv[1:]:
    if not arg.startswith("-") and not os.path.realpath(arg).startswith(base + os.sep):
        message = f"{name} refused {{arg}}"
        print(message, file=sys.stderr)
        with open({str(log)!r}, "a") as log:
            print(message, file=log)
        sys.exit(1)
os.execv({tool!r}, [{tool!r}, *sys.argv[1:]])
""")
            (stubs / name).chmod(0o755)
        env = dict(os.environ, PATH=str(stubs),
                   DSTREAMY_INSTALL_DIR="Applications" if relative else str(base / "Applications"))
        result = subprocess.run([str(stubs / "bash"), str(base / "scripts/install.sh"), *args], env=env,
                                cwd=base, text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        built_by = (base / "Applications/D-Streamy.app/built-by").read_text().strip()
        self.pkill = [line.split(" ", 1)[1] for line in log.read_text().splitlines() if line.startswith("pkill ")]
        self.dest = base / "Applications/D-Streamy.app"
        return [line.split()[0] for line in log.read_text().splitlines()], built_by

    def test_install_always_builds_before_copying_an_existing_bundle(self):
        for args, build in (((), "build.sh"), (("--release",), "release.sh")):
            with self.subTest(args=args):
                calls, built_by = self.install(*args)
                self.assertEqual(calls[0], build)
                self.assertEqual(built_by, build)


    def test_install_stops_only_the_installed_app(self):
        self.install()
        self.check_pkill()

    def test_relative_install_dir_still_stops_the_running_app(self):
        self.install(relative=True)
        self.check_pkill()

    def check_pkill(self):
        self.assertEqual(len(self.pkill), 1, self.pkill)
        flag, pattern = self.pkill[0].split(" ", 1)
        self.assertEqual(flag, "-f")
        dest = str(self.dest)
        for command, killed in ((f"{dest}/Contents/MacOS/D-Streamy", True),
                                (f"{dest}/Contents/MacOS/dstreamy daemon", True),
                                ("vim /Users/me/D-Streamy/notes.md", False),
                                ("bash /Users/me/D-Streamy/scripts/install.sh", False),
                                (f"{dest.replace('D-Streamy.app', 'D-StreamyXapp')}/Contents/MacOS/D-Streamy", False),
                                (f"/usr/bin/open {dest}/Contents/MacOS/D-Streamy", False)):
            with self.subTest(command=command):
                self.assertEqual(bool(re.search(pattern, command)), killed)


class NoticeTests(unittest.TestCase):
    def test_metadata_selects_manifest_target_features(self):
        with patch.object(notices.subprocess, "run") as run:
            run.side_effect = [subprocess.CompletedProcess([], 0, stdout=json.dumps({
                "packages": [dict(id="root", name="dstreamy-obs", version="0.1.0")],
                "resolve": {"nodes": [dict(id="root", deps=[])]}})),
                subprocess.CompletedProcess([], 0, stdout="dstreamy-obs v0.1.0\n")]
            notices.metadata(Path("plugin/Cargo.toml"), "target-triple", "dstreamy-obs", ["browser-login"])
            for call in run.call_args_list:
                argv = call.args[0]
                self.assertIn("plugin/Cargo.toml", argv)
                self.assertIn("target-triple", argv)
                self.assertIn("dstreamy-obs/browser-login", argv)
                self.assertIn("--locked", argv)

    def test_root_normal_and_build_not_dev(self):
        meta = {"packages": [{"id": "root", "name": "dstreamy-obs"}], "workspace_members": ["root"],
                "resolve": {"nodes": [{"id": "root", "deps": [
                    {"pkg": name, "dep_kinds": [{"kind": kind}]} for name, kind in
                    (("tokio", None), ("bindgen", "build"), ("dev", "dev"))]},
                    *({"id": name, "deps": []} for name in ("tokio", "bindgen", "dev"))]}}
        self.assertEqual(notices.shipped(meta, "dstreamy-obs"), {"root", "tokio", "bindgen"})
        with self.assertRaisesRegex(ValueError, "workspace package"):
            notices.shipped(meta, "missing")

    def fixture(self, directory: Path) -> MetadataFixture:
        packages: list[MetadataPackage] = []
        nodes: list[MetadataNode] = []
        for name in ("dstreamy-cli", "tokio", "tao", "openh264-sys2"):
            crate = directory / name
            crate.mkdir()
            (crate / "LICENSE").write_text(name + " license text")
            packages.append({"id": name, "name": name, "version": "0.1.0", "license": "MIT", "authors": [],
                             "manifest_path": str(crate / "Cargo.toml")})
            nodes.append({"id": name, "deps": []})
        nodes[0]["deps"] = [{"pkg": "tokio", "dep_kinds": [{"kind": None}]}]
        return {"packages": packages, "workspace_members": ["dstreamy-cli"], "resolve": {"nodes": nodes}}

    def test_feature_notices_and_append_are_graph_scoped(self):
        with tempfile.TemporaryDirectory(prefix="notice-fixture-") as tmp:
            base = Path(tmp)
            default = self.fixture(base)
            browser = json.loads(json.dumps(default))
            browser["resolve"]["nodes"][0]["deps"].append(dict(pkg="tao", dep_kinds=[dict(kind=None)]))
            extra = base / "tabler.txt"
            extra.write_text("Tabler Icons MIT license text")
            output = base / "notices.txt"
            manifest = base / "Cargo.toml"
            manifest.touch()
            for features, meta in (([], default), (["--features", "browser-login"], browser)):
                argv = ["notices", "--manifest", str(manifest), "--target", "fixture-target", "--output", str(output),
                        "--append", str(extra), *features]
                with patch.object(notices, "metadata", return_value=meta), patch.object(notices.sys, "argv", argv):
                    self.assertEqual(notices.main(), 0)
                text = output.read_text()
                self.assertIn("tokio license text", text)
                self.assertIn("Tabler Icons MIT license text", text)
                self.assertEqual("tao license text" in text, bool(features))
                self.assertNotIn("OpenH264", text)
                self.assertNotIn("libopus", text)
                self.assertNotIn("discord-stream-rs", text)
                self.assertIn("GPL-3.0-or-later", text)

    def test_metadata_excludes_workspace_only_optional_dependencies(self):
        meta = {"packages": [dict(id=name, name=name, version="0.1.0")
                             for name in ("dstreamy-cli", "tao", "tokio")],
                "workspace_members": ["dstreamy-cli"], "resolve": {"nodes": [
                    dict(id="dstreamy-cli", deps=[dict(pkg=name, dep_kinds=[dict(kind=None)])
                                                for name in ("tao", "tokio")]),
                    dict(id="tao", deps=[]), dict(id="tokio", deps=[])]}}
        def run(command, **_):
            output = json.dumps(meta) if command[1] == "metadata" else "dstreamy-cli v0.1.0\ntokio v0.1.0\n"
            return subprocess.CompletedProcess(command, 0, stdout=output)
        with patch.object(notices.subprocess, "run", side_effect=run):
            selected = notices.metadata(Path("Cargo.toml"), "fixture", "dstreamy-cli", [])
        self.assertEqual(notices.shipped(selected, "dstreamy-cli"), {"dstreamy-cli", "tokio"})

    def test_invalid_notice_inputs_fail_explicitly(self):
        with tempfile.TemporaryDirectory(prefix="notice-fixture-") as tmp:
            base = Path(tmp)
            manifest = base / "Cargo.toml"
            manifest.touch()
            output = base / "notices.txt"
            argv = ["notices", "--manifest", str(manifest), "--target", "invalid-target", "--output", str(output)]
            for error in (ValueError("expected one workspace package named 'missing'"),
                          subprocess.CalledProcessError(101, ["cargo", "metadata"], stderr="invalid feature or target")):
                with self.subTest(error=type(error).__name__), patch.object(notices, "metadata", side_effect=error), \
                     patch.object(notices.sys, "argv", argv), contextlib.redirect_stderr(io.StringIO()) as stderr:
                    self.assertEqual(notices.main(), 1)
                    self.assertIn("cannot generate notices", stderr.getvalue())
                    self.assertFalse(output.exists())
            with patch.object(notices.sys, "argv", argv + ["--manifest", str(base / "missing")]), \
                 contextlib.redirect_stderr(io.StringIO()) as stderr, self.assertRaises(SystemExit) as result:
                notices.main()
            self.assertEqual(result.exception.code, 2)
            self.assertIn("manifest does not exist", stderr.getvalue())

    def test_empty_append_fails_without_output(self):
        with tempfile.TemporaryDirectory(prefix="notice-fixture-") as tmp:
            base = Path(tmp)
            meta = self.fixture(base)
            manifest, empty, output = base / "Cargo.toml", base / "empty", base / "notices"
            manifest.touch()
            empty.touch()
            with patch.object(notices, "metadata", return_value=meta), patch.object(notices.sys, "argv",
                 ["notices", "--manifest", str(manifest), "--target", "fixture", "--append", str(empty),
                  "--output", str(output)]), contextlib.redirect_stderr(io.StringIO()) as stderr:
                self.assertEqual(notices.main(), 1)
                self.assertIn("must not be empty", stderr.getvalue())
            self.assertFalse(output.exists())

    def test_missing_license_fails_without_output(self):
        with tempfile.TemporaryDirectory(prefix="notice-fixture-") as tmp:
            base = Path(tmp)
            meta = self.fixture(base)
            meta["packages"][1]["manifest_path"] = str(base / "empty/Cargo.toml")
            (base / "empty").mkdir()
            meta["packages"][1]["license"] = None
            manifest = base / "Cargo.toml"
            manifest.touch()
            output = base / "notices.txt"
            with patch.object(notices, "metadata", return_value=meta), patch.object(notices.sys, "argv",
                ["notices", "--manifest", str(manifest), "--target", "fixture", "--output", str(output)]):
                self.assertNotEqual(notices.main(), 0)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
