"""Style regressions (install npm tools first): python3 -m unittest discover -s tools."""

import hashlib
import io
import json
import os
import runpy
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
INSTALL = runpy.run_path(str(ROOT / "scripts/install_actionlint.py"))["main"]
CHECK = runpy.run_path(str(ROOT / "scripts/check_style.py"))["main"]


class StyleTests(unittest.TestCase):
    def install(self, directory, archive, digest):
        pins = {"version": "1.7.12", "sha256": {"linux_amd64": digest}}
        with (
            mock.patch("sys.argv", ["install_actionlint.py", "--bin-dir", directory]),
            mock.patch("platform.system", return_value="Linux"),
            mock.patch("platform.machine", return_value="x86_64"),
            mock.patch("json.loads", return_value=pins),
            mock.patch("urllib.request.urlopen", return_value=io.BytesIO(archive)),
        ):
            INSTALL()

    def archive(self, symlink=False):
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w:gz") as bundle:
            member = tarfile.TarInfo("actionlint")
            if symlink:
                member.type = tarfile.SYMTYPE
                member.linkname = "outside"
                bundle.addfile(member)
            else:
                member.size = 6
                bundle.addfile(member, io.BytesIO(b"binary"))
        return output.getvalue()

    def test_installs_only_verified_regular_member(self):
        archive = self.archive()
        with tempfile.TemporaryDirectory() as directory:
            self.install(directory, archive, hashlib.sha256(archive).hexdigest())
            executable = Path(directory) / "actionlint"
            self.assertEqual(executable.read_bytes(), b"binary")
            self.assertEqual(executable.stat().st_mode & 0o777, 0o755)
            self.assertEqual(list(Path(directory).iterdir()), [executable])

    def test_bad_checksum_does_not_replace_existing_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "actionlint"
            executable.write_bytes(b"keep")
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                self.install(directory, self.archive(), "0" * 64)
            self.assertEqual(executable.read_bytes(), b"keep")
            self.assertEqual(list(Path(directory).iterdir()), [executable])

    def test_rejects_symlink_archive_member(self):
        archive = self.archive(symlink=True)
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(
                ValueError, "invalid actionlint archive member"
            ):
                self.install(directory, archive, hashlib.sha256(archive).hexdigest())
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_checker_fails_missing_tool_without_editing(self):
        source = ROOT / "README.md"
        before = source.read_bytes()
        with mock.patch.dict("os.environ", {"NODE": "/nonexistent/am-style-node"}):
            result = subprocess.run(
                [sys.executable, str(ROOT / "scripts/check_style.py"), "README.md"],
                cwd=ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing tool: /nonexistent/am-style-node", result.stderr)
        self.assertEqual(source.read_bytes(), before)


class SelectionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        for name in (
            ".node-version",
            "package.json",
            "requirements-dev.txt",
            "tools/actionlint.json",
        ):
            target = self.root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes((ROOT / name).read_bytes())
        pins = json.loads((self.root / "package.json").read_text())["devDependencies"]
        for package, version in pins.items():
            manifest = self.root / "node_modules" / package / "package.json"
            manifest.parent.mkdir(parents=True, exist_ok=True)
            manifest.write_text(json.dumps({"version": version}))

    def check(self, *arguments, git_files=(), node_version=None):
        versions = {
            "node": "v" + (ROOT / ".node-version").read_text().strip(),
            "ruff": "ruff "
            + (ROOT / "requirements-dev.txt").read_text().strip().split("==")[1],
            "actionlint": json.loads((ROOT / "tools/actionlint.json").read_text())[
                "version"
            ],
        }
        if node_version is not None:
            versions["node"] = node_version

        def output(command, **_kwargs):
            if command[0] == "git":
                return "\0".join(git_files).encode()
            return versions[command[0]] + "\n"

        with (
            mock.patch.dict(CHECK.__globals__, {"ROOT": self.root}),
            mock.patch("sys.argv", ["check_style.py", *arguments]),
            mock.patch.dict(
                "os.environ",
                {
                    "NODE": "node",
                    "RUFF": "ruff",
                    "ACTIONLINT": "actionlint",
                    "AM_NODE_MODULES": str(self.root / "node_modules"),
                },
            ),
            mock.patch("shutil.which", return_value="/tool"),
            mock.patch("subprocess.check_output", side_effect=output),
            mock.patch(
                "subprocess.run",
                return_value=subprocess.CompletedProcess([], 0, b"x = 1\n"),
            ) as run,
            mock.patch("sys.stdout", new_callable=io.StringIO),
            mock.patch("sys.stderr", new_callable=io.StringIO),
        ):
            self.command_run = run
            return CHECK()

    def test_default_skips_deleted_files(self):
        (self.root / "kept.md").write_text("# Kept\n")
        self.assertEqual(self.check(git_files=("deleted.md", "kept.md")), 0)
        self.assertEqual(self.command_run.call_count, 2)
        for call in self.command_run.call_args_list:
            self.assertEqual(call.args[0][-1], "./kept.md")
            self.assertNotIn("./deleted.md", call.args[0])

    def test_default_with_only_deletions_succeeds(self):
        self.assertEqual(self.check(git_files=("deleted.md",)), 0)
        self.command_run.assert_not_called()

    def test_explicit_missing_file_fails(self):
        with self.assertRaises(SystemExit) as error:
            self.check("deleted.md")
        self.assertEqual(error.exception.code, 2)
        self.command_run.assert_not_called()

    def test_rejects_symlink_parent_escape(self):
        with tempfile.TemporaryDirectory() as outside:
            (Path(outside) / "external.md").write_text("# External\n")
            (self.root / "linked").symlink_to(outside, target_is_directory=True)
            with self.assertRaises(SystemExit) as error:
                self.check("--fix", "linked/external.md")
            self.assertEqual(error.exception.code, 2)
            self.command_run.assert_not_called()

    def test_option_and_glob_names_are_literal(self):
        names = ("--write.md", "a[bc]*?.md", "--fix.py")
        for name in names:
            (self.root / name).write_text("# Fixture\n")
        self.assertEqual(self.check("--", *names), 0)
        for call in self.command_run.call_args_list:
            command = call.args[0]
            if command[0] == "ruff":
                self.assertEqual(command[-2:], ["--", "./--fix.py"])
                self.assertNotIn("--fix", command)
            else:
                self.assertEqual(command[-2:], ["./--write.md", "./a[bc]*?.md"])
                if "--check" in command:
                    self.assertEqual(command[-3], "--")

    def test_taplo_stdin_formats_only_selected_file(self):
        selected = self.root / "--flag[*].toml"
        selected.write_text("x=1\n")
        decoy = self.root / "--flagx.toml"
        decoy.write_text("keep=2\n")
        self.assertEqual(self.check("--fix", "--", selected.name), 0)
        self.assertEqual(self.command_run.call_count, 2)
        for call in self.command_run.call_args_list:
            self.assertEqual(call.args[0][-2:], ["--", "-"])
            self.assertEqual(call.kwargs["input"], b"x=1\n")
        self.assertEqual(selected.read_bytes(), b"x = 1\n")
        self.assertEqual(decoy.read_bytes(), b"keep=2\n")

    def test_wrong_node_version_fails_before_linting(self):
        (self.root / "example.md").write_text("# Example\n")
        with self.assertRaises(SystemExit) as error:
            self.check("example.md", node_version="v22.0.0")
        self.assertEqual(error.exception.code, 2)
        self.command_run.assert_not_called()


class MarkdownTests(unittest.TestCase):
    def node(self, *arguments, env=None):
        return subprocess.run(
            [os.environ.get("NODE", "node"), *arguments],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
            env=env,
        )

    def test_literal_files_and_only_two_rule_exceptions(self):
        config = json.loads((ROOT / ".markdownlint.json").read_text())
        self.assertEqual(config, {"default": True, "MD013": False, "MD024": False})
        with tempfile.TemporaryDirectory() as directory:
            selected = Path(directory) / "notes[a].md"
            selected.write_text(
                "# Document\n\n## Repeated\n\n"
                + "x" * 300
                + "\n\n## Repeated\n\n$x^2$\n"
            )
            (Path(directory) / "notesa.md").write_text("missing heading\n")
            result = self.node(str(ROOT / "scripts/lint_markdown.mjs"), str(selected))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 files, no issues", result.stdout)

    def test_fence_language_and_html_rules_still_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            selected = Path(directory) / "bad.md"
            selected.write_text("# Document\n\n```\ncode\n```\n\n<div>HTML</div>\n")
            result = self.node(str(ROOT / "scripts/lint_markdown.mjs"), str(selected))
        self.assertEqual(result.returncode, 1)
        self.assertIn("MD033", result.stderr)
        self.assertIn("MD040", result.stderr)

    def test_only_direct_changelog_fragments_may_omit_heading(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / "scripts/lint_markdown.mjs"
            script.parent.mkdir()
            script.write_bytes((ROOT / "scripts/lint_markdown.mjs").read_bytes())
            (root / ".markdownlint.json").write_bytes(
                (ROOT / ".markdownlint.json").read_bytes()
            )
            for name, expected in (
                ("changelog.d/queued.fixed.md", 0),
                ("notes.md", 1),
                ("changelog.d/README.md", 1),
                ("changelog.d/nested/queued.fixed.md", 1),
            ):
                selected = root / name
                selected.parent.mkdir(parents=True, exist_ok=True)
                selected.write_text("Preserve queued requests during cancellation.\n")
                result = self.node(
                    str(script),
                    str(selected),
                    env={
                        **os.environ,
                        "AM_NODE_MODULES": os.environ.get(
                            "AM_NODE_MODULES", str(ROOT / "node_modules")
                        ),
                    },
                )
                with self.subTest(name=name):
                    self.assertEqual(result.returncode, expected, result.stderr)
                    if expected:
                        self.assertIn("MD041", result.stderr)

    def test_patched_katex_rendering_api_is_compatible(self):
        result = self.node(
            "--input-type=module",
            "-e",
            """
            import assert from 'node:assert/strict';
            import { micromark } from 'micromark';
            import { math, mathHtml } from 'micromark-extension-math';
            const html = micromark('$x^2$', {
                extensions: [math()], htmlExtensions: [mathHtml()]
            });
            assert.match(html, /class="katex"/);
        """,
        )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
