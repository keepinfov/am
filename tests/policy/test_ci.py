"""Offline CI fixtures: python3 -m unittest discover -s tests/policy -v."""

import copy
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import urllib.error
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import Mock, call, patch

ROOT = Path(__file__).resolve().parents[2]
with patch.object(sys, "path", [str(ROOT / "scripts"), *sys.path]):
    SPEC = importlib.util.spec_from_file_location(
        "check_ci", ROOT / "scripts/check_ci.py"
    )
    ci = importlib.util.module_from_spec(SPEC)
    SPEC.loader.exec_module(ci)
REAL_GITHUB = ci.GitHub

BASE = "a" * 40
HEAD = "b" * 40
MERGE = "c" * 40
TOKEN = "fixture-token-never-log"


class CITests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.directory = Path(directory.name)
        self.event_path = self.directory / "event.json"
        self.summary = self.directory / "summary.md"
        self.event = {
            "repository": {"full_name": "trusted/am"},
            "before": BASE,
            "after": HEAD,
        }
        self.env = {
            "GITHUB_EVENT_NAME": "push",
            "GITHUB_EVENT_PATH": str(self.event_path),
            "GITHUB_REPOSITORY": "trusted/am",
            "GITHUB_SHA": HEAD,
            "GITHUB_STEP_SUMMARY": str(self.summary),
            "GH_TOKEN": TOKEN,
        }
        self.git = self.enterContext(
            patch.object(ci.core, "git", return_value=str(ROOT).encode())
        )
        self.revision = self.enterContext(
            patch.object(ci.core, "revision", side_effect=lambda root, value: value)
        )
        self.ids = self.enterContext(
            patch.object(ci.core, "commit_ids", return_value=[HEAD])
        )
        self.checker = self.enterContext(patch.object(ci.core, "main", return_value=0))
        self.api = Mock()
        self.api.request.side_effect = self.response
        self.client = self.enterContext(
            patch.object(ci, "GitHub", return_value=self.api)
        )

    def response(self, method, path):
        self.assertEqual(method, "GET")
        self.assertTrue(path.startswith("/repos/trusted/am/commits/"))
        return {
            "sha": path.rsplit("/", 1)[1],
            "commit": {"verification": {"verified": True, "reason": "valid"}},
        }

    def pull_request(self):
        self.env["GITHUB_EVENT_NAME"] = "pull_request"
        self.env["GITHUB_SHA"] = MERGE
        self.event = {
            "repository": {"full_name": "trusted/am"},
            "number": 17,
            "pull_request": {
                "number": 17,
                "base": {
                    "sha": BASE,
                    "ref": "master",
                    "repo": {"full_name": "trusted/am"},
                },
                "head": {
                    "sha": HEAD,
                    "ref": "ci/repository-policy",
                    "repo": {"full_name": "untrusted/fork"},
                },
                "title": "ci(repo): check repository policy",
                "body": "",
            },
        }

    def run_ci(self, raw=None):
        self.event_path.write_bytes(
            json.dumps(self.event).encode() if raw is None else raw
        )
        output, errors = io.StringIO(), io.StringIO()
        with (
            patch.dict(os.environ, self.env, clear=True),
            redirect_stdout(output),
            redirect_stderr(errors),
        ):
            status = ci.main()
        self.assertNotIn(TOKEN, output.getvalue() + errors.getvalue())
        return status, output.getvalue(), errors.getvalue()

    def test_valid_signatures_for_every_commit_in_actual_range(self):
        self.ids.return_value = [BASE, HEAD]
        status, output, errors = self.run_ci()
        self.assertEqual((status, errors), (0, ""))
        self.assertIn("GitHub verified 2 commit signatures", output)
        self.assertIn("not maintainer approval", output)
        self.ids.assert_called_once_with(ROOT, BASE, HEAD)
        self.assertEqual(
            self.api.request.call_args_list,
            [call("GET", f"/repos/trusted/am/commits/{oid}") for oid in (BASE, HEAD)],
        )
        self.assertEqual(
            self.checker.call_args_list,
            [call(["files"]), call(["commits", "--base", BASE, "--head", HEAD])],
        )

    def test_unsigned_bad_reason_and_truthy_nonboolean_rejected(self):
        for verified, reason in (
            (False, "unsigned"),
            (False, "bad_signature"),
            (True, "unknown_key"),
            (True, "expired_key"),
            (True, "unsigned"),
            (1, "valid"),
            ("true", "valid"),
            (None, "valid"),
        ):
            with self.subTest(verified=verified, reason=reason):
                self.api.request.side_effect = None
                self.api.request.return_value = {
                    "sha": HEAD,
                    "commit": {
                        "verification": {"verified": verified, "reason": reason}
                    },
                }
                status, output, errors = self.run_ci()
                self.assertEqual(status, 1)
                self.assertNotIn("GitHub verified", output)
                self.assertIn("not valid", errors)

    def test_wrong_sha_and_malformed_api_metadata_rejected(self):
        for response in (
            None,
            [],
            {"sha": BASE},
            {"sha": HEAD},
            {"sha": HEAD, "commit": None},
            {"sha": HEAD, "commit": {"verification": {}}},
            {"sha": HEAD, "commit": {"verification": None}},
        ):
            with self.subTest(response=response):
                self.api.request.side_effect = None
                self.api.request.return_value = response
                self.assertEqual(self.run_ci()[0], 1)

    def test_missing_token_fails_even_when_range_is_empty(self):
        for token in ("", "   "):
            with self.subTest(token=token):
                self.env["GH_TOKEN"] = token
                self.ids.return_value = []
                status, _, errors = self.run_ci()
                self.assertEqual(status, 1)
                self.assertIn("GH_TOKEN is required", errors)
                self.client.assert_not_called()

    def test_api_error_fails_without_logging_response_body(self):
        # Exercise the shared authenticated transport, mocking only network I/O.
        with patch.object(ci, "GitHub", REAL_GITHUB):
            for code in (401, 403, 404, 429, 500):
                with self.subTest(code=code):
                    error = urllib.error.HTTPError(
                        "https://api.github.com/",
                        code,
                        TOKEN,
                        {},
                        io.BytesIO(TOKEN.encode()),
                    )
                    with patch("urllib.request.OpenerDirector.open", side_effect=error):
                        status, _, errors = self.run_ci()
                    self.assertEqual(status, 1)
                    self.assertIn(f"HTTP {code}", errors)

    def test_shared_transport_uses_authenticated_get_only(self):
        response = self.response("GET", f"/repos/trusted/am/commits/{HEAD}")

        def open_request(request, timeout):
            self.assertEqual(request.get_method(), "GET")
            self.assertEqual(
                request.full_url,
                f"https://api.github.com/repos/trusted/am/commits/{HEAD}",
            )
            self.assertEqual(request.get_header("Authorization"), f"Bearer {TOKEN}")
            self.assertIsNone(request.data)
            self.assertEqual(timeout, 30)
            return io.BytesIO(json.dumps(response).encode())

        with (
            patch.object(ci, "GitHub", REAL_GITHUB),
            patch(
                "urllib.request.OpenerDirector.open", side_effect=open_request
            ) as transport,
        ):
            self.assertEqual(self.run_ci()[0], 0)
        transport.assert_called_once()

    def test_repository_injection_rejected_before_checker_or_api(self):
        for repo in (
            "owner/repo/commits",
            "../repo",
            "owner/..",
            "x/y?z",
            "x/y\n",
            "x/y%2fz",
        ):
            with self.subTest(repo=repo):
                self.env["GITHUB_REPOSITORY"] = repo
                self.event["repository"]["full_name"] = repo
                self.assertEqual(self.run_ci()[0], 1)
                self.checker.assert_not_called()
                self.client.assert_not_called()

    def test_wrong_event_or_pr_base_repository_rejected(self):
        self.event["repository"]["full_name"] = "untrusted/fork"
        self.assertEqual(self.run_ci()[0], 1)
        self.pull_request()
        self.event["pull_request"]["base"]["repo"]["full_name"] = "untrusted/fork"
        self.assertEqual(self.run_ci()[0], 1)
        self.checker.assert_not_called()
        self.client.assert_not_called()

    def test_event_limit_exact_boundary(self):
        raw = json.dumps(self.event).encode()
        raw += b" " * (ci.EVENT_LIMIT - len(raw))
        self.assertEqual(self.run_ci(raw)[0], 0)
        self.client.reset_mock()
        self.checker.reset_mock()
        status, _, errors = self.run_ci(raw + b" ")
        self.assertEqual(status, 1)
        self.assertIn("1 MiB", errors)
        self.checker.assert_not_called()
        self.client.assert_not_called()

    def test_invalid_json_shapes_and_constants_fail_closed(self):
        for raw in (
            b"[]",
            b"null",
            b"true",
            b"{}",
            b'{"repository":[]}',
            b"NaN",
            b"{",
            b"\xff",
        ):
            with self.subTest(raw=raw):
                self.assertEqual(self.run_ci(raw)[0], 1)
                self.checker.assert_not_called()
                self.client.assert_not_called()

    def test_missing_or_unsupported_ci_environment_fails(self):
        for name in ("", "pull_request_target", "workflow_dispatch"):
            self.env["GITHUB_EVENT_NAME"] = name
            status, _, errors = self.run_ci()
            self.assertEqual(status, 1)
            self.assertIn("GITHUB_EVENT_NAME", errors)
        self.env["GITHUB_EVENT_NAME"] = "push"
        del self.env["GITHUB_EVENT_PATH"]
        self.assertIn("GITHUB_EVENT_PATH", self.run_ci()[2])
        self.client.assert_not_called()

    def test_invalid_pr_shapes_numbers_and_zero_base_rejected(self):
        self.pull_request()
        original = copy.deepcopy(self.event)
        for field, value in (
            ("base", None),
            ("head", []),
            ("number", True),
            ("number", 18),
        ):
            with self.subTest(field=field, value=value):
                self.event = copy.deepcopy(original)
                self.event["pull_request"][field] = value
                self.assertEqual(self.run_ci()[0], 1)
        self.event = copy.deepcopy(original)
        self.event["pull_request"]["base"]["sha"] = "0" * 40
        self.assertEqual(self.run_ci()[0], 1)
        self.checker.assert_not_called()
        self.client.assert_not_called()

    def test_missing_invalid_and_zero_push_revisions_never_use_head_parent(self):
        original = copy.deepcopy(self.event)
        for field in ("before", "after"):
            for value in (None, "", "HEAD", "a" * 7, "g" * 40, "0" * 39, [], "0" * 40):
                if field == "before" and value == "0" * 40:
                    continue
                with self.subTest(field=field, value=value):
                    self.event = copy.deepcopy(original)
                    self.event[field] = value
                    self.assertEqual(self.run_ci()[0], 1)
            self.event = copy.deepcopy(original)
            del self.event[field]
            self.assertEqual(self.run_ci()[0], 1)
        self.client.assert_not_called()
        self.checker.assert_not_called()

    def test_initial_push_passes_zero_base_to_core(self):
        for length in (40, 64):
            self.event["before"] = "0" * length
            self.assertEqual(self.run_ci()[0], 0)
            self.ids.assert_called_with(ROOT, "0" * length, HEAD)
            self.assertNotIn(call(ROOT, "0" * length), self.revision.call_args_list)

    def test_push_github_sha_mismatch_rejected_but_absence_allowed(self):
        self.env["GITHUB_SHA"] = MERGE
        self.assertEqual(self.run_ci()[0], 1)
        self.client.assert_not_called()
        del self.env["GITHUB_SHA"]
        self.assertEqual(self.run_ci()[0], 0)

    def test_missing_git_objects_and_noncanonical_resolution_fail(self):
        self.revision.side_effect = ci.core.PolicyError("missing commit object")
        self.assertEqual(self.run_ci()[0], 1)
        self.revision.side_effect = lambda root, value: MERGE
        self.assertEqual(self.run_ci()[0], 1)
        self.client.assert_not_called()

    def test_bad_range_ids_never_reach_api_path(self):
        for oid in ("HEAD", "a" * 7, "../labels", "0" * 40, None):
            with self.subTest(oid=oid):
                self.ids.return_value = [HEAD, oid]
                self.assertEqual(self.run_ci()[0], 1)
                self.client.assert_not_called()

    def test_pr_dispatch_uses_proposed_head_and_summary_before_pr_failure(self):
        self.pull_request()
        injection = "$(touch /tmp/not-executed);`id` ::error::untrusted"
        self.event["pull_request"]["title"] = injection

        def check(args):
            if args[0] == "size":
                self.summary.write_text("## Change Size: XS\n", encoding="utf-8")
            if args[0] == "pr":
                self.assertTrue(self.summary.exists())
                return 1
            return 0

        self.checker.side_effect = check
        self.assertEqual(self.run_ci()[0], 1)
        options = ["--base", BASE, "--head", HEAD, "--summary", str(self.summary)]
        self.assertEqual(
            self.checker.call_args_list,
            [
                call(["files"]),
                call(["commits", "--base", BASE, "--head", HEAD]),
                call(["size", *options]),
                call(["pr", *options, "--event", str(self.event_path)]),
            ],
        )
        self.assertNotIn(injection, self.summary.read_text())
        self.assertNotIn(MERGE, repr(self.checker.call_args_list))
        self.ids.assert_called_once_with(ROOT, BASE, HEAD)

    def test_every_checker_failure_is_preserved_without_skipping_later_checks(self):
        self.pull_request()
        del self.env["GITHUB_STEP_SUMMARY"]
        for failed in ("files", "commits", "size", "pr"):
            with self.subTest(failed=failed):
                self.checker.reset_mock()
                self.checker.side_effect = lambda args: int(args[0] == failed)
                self.assertEqual(self.run_ci()[0], 1)
                self.assertEqual(self.checker.call_count, 4)
                self.assertNotIn("--summary", repr(self.checker.call_args_list))


class RealDispatchTests(unittest.TestCase):
    def test_normal_script_invocation_without_ci_environment_fails_clearly(self):
        result = subprocess.run(
            [sys.executable, "scripts/check_ci.py"],
            cwd=ROOT,
            env={
                key: value
                for key, value in os.environ.items()
                if not key.startswith("GITHUB_")
            },
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("GITHUB_EVENT_NAME", result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_real_core_history_commands_and_summary_survive_invalid_pr(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args):
                return (
                    subprocess.check_output(["git", *args], cwd=root).decode().strip()
                )

            git("init", "-q")
            (root / ".repo-policy.toml").write_bytes(
                (ROOT / ".repo-policy.toml").read_bytes()
            )
            (root / "fixture.txt").write_text("base\n")
            env = {
                **os.environ,
                "GIT_AUTHOR_NAME": "Fixture",
                "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
                "GIT_COMMITTER_NAME": "Fixture",
                "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_CONFIG_COUNT": "2",
                "GIT_CONFIG_KEY_0": "core.hooksPath",
                "GIT_CONFIG_VALUE_0": os.devnull,
                "GIT_CONFIG_KEY_1": "commit.gpgSign",
                "GIT_CONFIG_VALUE_1": "false",
            }
            with patch.dict(os.environ, env, clear=True):
                git("add", ".")
                git("commit", "-qm", "ci(repo): add fixture")
                base = git("rev-parse", "HEAD")
                (root / "fixture.txt").write_text("head\n")
                git("add", ".")
                git("commit", "-qm", "ci(repo): update fixture")
                head = git("rev-parse", "HEAD")
                self.assertEqual(ci.core.commit_ids(root, "0" * 40, head), [base, head])
                self.assertEqual(ci.core.commit_ids(root, base, head), [head])
                event = root / "event.json"
                event.write_text(
                    json.dumps(
                        {
                            "repository": {"full_name": "trusted/am"},
                            "number": 1,
                            "pull_request": {
                                "number": 1,
                                "base": {
                                    "sha": base,
                                    "ref": "not-master",
                                    "repo": {"full_name": "trusted/am"},
                                },
                                "head": {"sha": head, "ref": "ci/fixture"},
                                "title": "$(untrusted)",
                                "body": "",
                            },
                        }
                    )
                )
                summary = root / "summary.md"
                api = Mock()
                api.request.return_value = {
                    "sha": head,
                    "commit": {"verification": {"verified": True, "reason": "valid"}},
                }
                output, errors = io.StringIO(), io.StringIO()
                with (
                    patch.dict(
                        os.environ,
                        {
                            "GITHUB_EVENT_NAME": "pull_request",
                            "GITHUB_EVENT_PATH": str(event),
                            "GITHUB_REPOSITORY": "trusted/am",
                            "GITHUB_SHA": MERGE,
                            "GITHUB_STEP_SUMMARY": str(summary),
                            "GH_TOKEN": TOKEN,
                        },
                    ),
                    patch.object(Path, "cwd", return_value=root),
                    patch.object(ci, "GitHub", return_value=api),
                    redirect_stdout(output),
                    redirect_stderr(errors),
                ):
                    self.assertEqual(ci.main(), 1)
                    self.assertIn("missing SSH/GPG signature armor", errors.getvalue())
                    self.assertIn("PR must target master", errors.getvalue())
                    self.assertIn("## Change Size: XS", summary.read_text())
                    self.assertNotIn("$(untrusted)", summary.read_text())
                    api.request.assert_called_once_with(
                        "GET", f"/repos/trusted/am/commits/{head}"
                    )
                    api.reset_mock()
                    api.request.side_effect = lambda method, path: {
                        "sha": path.rsplit("/", 1)[1],
                        "commit": {
                            "verification": {"verified": True, "reason": "valid"}
                        },
                    }
                    event.write_text(
                        json.dumps(
                            {
                                "repository": {"full_name": "trusted/am"},
                                "before": "0" * 40,
                                "after": head,
                            }
                        )
                    )
                    with patch.dict(
                        os.environ, {"GITHUB_EVENT_NAME": "push", "GITHUB_SHA": head}
                    ):
                        # Unsigned fixtures still fail core policy, but the API must
                        # independently check the entire initial-push history.
                        self.assertEqual(ci.main(), 1)
                    self.assertEqual(
                        api.request.call_args_list,
                        [
                            call("GET", f"/repos/trusted/am/commits/{oid}")
                            for oid in (base, head)
                        ],
                    )


if __name__ == "__main__":
    unittest.main()
