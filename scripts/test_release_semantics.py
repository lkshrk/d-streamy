#!/usr/bin/env python3
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from typing import TypedDict
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
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
        self.assertIn("No GitHub release or Homebrew update was published", source)
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

    def gate(self, obs_runs, change_obs, failed=""):
        """Runs the CI gate against a two-commit repo; obs_runs maps 'base'/'tip' to whether OBS ran there."""
        script = step("release.yml", "Wait for CI on the tagged commit")
        with tempfile.TemporaryDirectory(prefix="gate-fixture-") as directory:
            repo = Path(directory) / "repo"
            (repo / ".github/workflows").mkdir(parents=True)
            (repo / "obs").mkdir()
            (repo / ".github/workflows/obs-ci.yml").write_text((ROOT / ".github/workflows/obs-ci.yml").read_text())
            (repo / "obs/plugin.c").write_text("1\n")
            git = lambda *args: subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True, text=True).stdout.strip()
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
                       GITHUB_REPOSITORY="o/r")
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
        def run(command, **kwargs):
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
