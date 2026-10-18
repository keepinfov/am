"""Local fixtures only: python3 -m unittest discover -s tests/policy -v."""

import copy
import http.client
import importlib.util
import io
import json
import os
import re
import tempfile
import unittest
import urllib.error
import urllib.parse
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("label_pr", ROOT / "scripts/label_pr.py")
labeler = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(labeler)
HEAD = "a" * 40
REPO = "/repos/trusted/am"
PULL = REPO + "/pulls/17"
ISSUE = REPO + "/issues/17/labels"
TOKEN = "fixture-token-never-log"
INJECTION = "$(touch /tmp/not-executed);`id`\n::error::untrusted ${GH_TOKEN}"


def file_metadata(name="src/main.rs", additions=1, deletions=0, **extra):
    return dict(
        filename=name,
        additions=additions,
        deletions=deletions,
        status="modified",
        **extra,
    )


class LabelTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.event_path = Path(self.directory.name) / "event.json"
        self.event = {
            "action": "opened",
            "number": 17,
            "repository": {"full_name": "trusted/am", "default_branch": "master"},
            "pull_request": {
                "number": 17,
                "head": {"sha": HEAD, "repo": {"full_name": "attacker/fork"}},
                "base": {"repo": {"full_name": "trusted/am"}},
                "title": INJECTION,
                "body": INJECTION,
                "draft": True,
            },
        }
        self.env = {
            "GH_TOKEN": TOKEN,
            "GITHUB_API_URL": "https://api.github.com",
            "GITHUB_REPOSITORY": "trusted/am",
        }
        self.files = [file_metadata()]
        self.expected = None
        self.labels = {"bug", "size/XL", "size/custom"}
        self.definitions = {f"size/{size}" for size in labeler.SIZES}
        self.calls = []
        self.pr_reads = 0
        self.stale_on = None
        self.count_change_on = None
        self.stale_after_write = False
        self.error = None
        self.raw = {}
        self.pages = {}

    @property
    def writes(self):
        return [call for call in self.calls if call[0] != "GET"]

    def open(self, request, timeout):
        self.assertEqual(timeout, 30)
        self.assertEqual(request.get_header("Authorization"), f"Bearer {TOKEN}")
        self.assertEqual(request.get_header("Accept"), "application/vnd.github+json")
        prefix = self.env["GITHUB_API_URL"].rstrip("/")
        self.assertTrue(request.full_url.startswith(prefix + REPO + "/"))
        path = request.full_url.removeprefix(prefix)
        method = request.get_method()
        data = json.loads(request.data) if request.data is not None else None
        self.calls.append((method, path, data))
        if self.error and (method, path) == self.error[:2]:
            raise urllib.error.HTTPError(
                request.full_url,
                self.error[2],
                TOKEN,
                {"secret": TOKEN},
                io.BytesIO((TOKEN + INJECTION).encode()),
            )
        if (method, path) in self.raw:
            return io.BytesIO(self.raw[method, path])
        if method == "GET" and path == PULL:
            self.pr_reads += 1
            stale = self.stale_on is not None and self.pr_reads >= self.stale_on
            changed = (
                self.count_change_on is not None
                and self.pr_reads >= self.count_change_on
            )
            result = {
                "head": {"sha": "b" * 40 if stale else HEAD},
                "changed_files": (
                    len(self.files) if self.expected is None else self.expected
                )
                + int(changed),
                "labels": [{"name": name} for name in sorted(self.labels)],
            }
        elif method == "GET" and path.startswith(PULL + "/files?"):
            query = urllib.parse.parse_qs(urllib.parse.urlsplit(path).query)
            self.assertEqual(query["per_page"], ["100"])
            page = int(query["page"][0])
            result = self.pages.get(page, self.files[(page - 1) * 100 : page * 100])
        elif method == "GET" and path.startswith(REPO + "/labels/"):
            name = urllib.parse.unquote(path.removeprefix(REPO + "/labels/"))
            if name not in self.definitions:
                raise urllib.error.HTTPError(request.full_url, 404, "", {}, None)
            result = {"name": name}
        elif method == "POST" and path == REPO + "/labels":
            self.definitions.add(data["name"])
            result = data
        elif method == "POST" and path == ISSUE:
            self.labels.update(data["labels"])
            result = []
        elif method == "DELETE" and path.startswith(ISSUE + "/"):
            self.labels.remove(urllib.parse.unquote(path.removeprefix(ISSUE + "/")))
            result = None
        else:
            self.fail(f"unexpected API call: {method} {path}")
        if method != "GET" and self.stale_after_write:
            self.stale_on = self.pr_reads + 1
        return io.BytesIO(b"" if method == "DELETE" else json.dumps(result).encode())

    def run_cli(self, raw=None):
        self.event_path.write_bytes(
            json.dumps(self.event).encode() if raw is None else raw
        )
        output, errors = io.StringIO(), io.StringIO()
        with (
            patch.dict(os.environ, self.env, clear=True),
            patch.object(
                labeler.sys, "argv", ["label_pr.py", "--event", str(self.event_path)]
            ),
            patch.object(
                labeler.urllib.request,
                "build_opener",
                return_value=Mock(open=self.open),
            ) as opener,
            redirect_stdout(output),
            redirect_stderr(errors),
        ):
            status = labeler.main()
        if opener.called:
            self.assertIsInstance(opener.call_args.args[0], labeler.NoRedirect)
        else:
            self.assertEqual(status, 1)
            self.assertEqual(self.calls, [])
        self.assertNotIn(TOKEN, output.getvalue() + errors.getvalue())
        return status, output.getvalue(), errors.getvalue()

    def assert_rejected(self, message=None):
        status, output, errors = self.run_cli()
        self.assertEqual(status, 1)
        self.assertEqual(output, "")
        self.assertTrue(errors)
        if message:
            self.assertIn(message, errors)
        self.assertEqual(self.writes, [])

    def test_fork_draft_uses_base_repo_and_preserves_unrelated_labels(self):
        status, output, errors = self.run_cli()
        self.assertEqual((status, errors), (0, ""))
        self.assertIn("size/XS", output)
        self.assertEqual(self.labels, {"bug", "size/custom", "size/XS"})
        self.assertEqual(
            self.writes,
            [
                ("POST", ISSUE, {"labels": ["size/XS"]}),
                ("DELETE", ISSUE + "/size%2FXL", None),
            ],
        )

    def test_exclusive_policy_boundaries_and_both_line_counters(self):
        for lines, size in (
            (0, "XS"),
            (49, "XS"),
            (50, "S"),
            (199, "S"),
            (200, "M"),
            (499, "M"),
            (500, "L"),
            (999, "L"),
            (1000, "XL"),
        ):
            with self.subTest(lines=lines):
                self.files = [
                    file_metadata(additions=lines // 2, deletions=lines - lines // 2)
                ]
                self.assertEqual(self.run_cli()[0], 0)
                self.assertEqual(
                    self.labels & {f"size/{s}" for s in labeler.SIZES}, {f"size/{size}"}
                )

    def test_generated_rename_excludes_only_both_generated_paths(self):
        self.files = [
            file_metadata("Cargo.lock", 9999),
            file_metadata("package-lock.json", 9999),
            file_metadata("Cargo.lock", 9999, previous_filename="package-lock.json"),
        ]
        # Use separate runs: each API filename is unique within a PR.
        for entry, expected in (
            (self.files[0], "XS"),
            (self.files[1], "XS"),
            (self.files[2], "XS"),
            (file_metadata("Cargo.lock", 250, previous_filename="src/real.rs"), "M"),
            (file_metadata("src/real.rs", 250, previous_filename="Cargo.lock"), "M"),
            (file_metadata("nested/Cargo.lock", 250), "M"),
        ):
            with self.subTest(entry=entry):
                if "previous_filename" in entry:
                    entry["status"] = "renamed"
                self.files = [entry]
                self.assertEqual(self.run_cli()[0], 0)
                self.assertIn(f"size/{expected}", self.labels)

    def test_filename_and_patch_injection_are_only_data(self):
        self.files = [
            file_metadata("../../" + INJECTION, 50, patch=INJECTION, raw_url=INJECTION)
        ]
        self.assertEqual(self.run_cli()[0], 0)
        self.assertIn("size/S", self.labels)
        self.assertNotIn(INJECTION, repr(self.calls))

    def test_zero_churn_does_not_claim_binary_counts(self):
        self.files = [file_metadata("image.png", 0, changes=0)]
        status, output, _ = self.run_cli()
        self.assertEqual(status, 0)
        self.assertIn(
            "0 reviewable API additions + deletions; binaries unknown", output
        )

    def test_all_missing_definitions_created_with_color_and_description(self):
        self.definitions.clear()
        self.assertEqual(self.run_cli()[0], 0)
        definitions = [
            data for method, path, data in self.writes if path == REPO + "/labels"
        ]
        self.assertEqual(len(definitions), 5)
        for definition in definitions:
            self.assertRegex(definition["color"], r"^[0-9A-F]{6}$")
            self.assertTrue(definition["description"])
        self.assertEqual(self.definitions, {f"size/{s}" for s in labeler.SIZES})

    def test_already_correct_labels_need_no_writes(self):
        self.labels = {"bug", "size/XS", "size/custom"}
        self.assertEqual(self.run_cli()[0], 0)
        self.assertEqual(self.writes, [])

    def test_pagination_and_exact_3000_file_limit(self):
        for total in (0, 100, 201, 3000):
            with self.subTest(total=total):
                self.calls.clear()
                self.files = [file_metadata(f"src/{i}.rs") for i in range(total)]
                self.assertEqual(self.run_cli()[0], 0)
                pages = [path for _, path, _ in self.calls if "/files?" in path]
                self.assertEqual(len(pages), min(total // 100 + 1, 30))

    def test_rejects_over_3000_before_fetching_files(self):
        self.expected = 3001
        self.assert_rejected("3000-file")
        self.assertEqual(len(self.calls), 1)

    def test_truncation_count_mismatch_and_repeated_pages(self):
        self.expected = 2
        self.assert_rejected("truncated")
        self.expected = 0
        self.assert_rejected("count mismatch")
        self.expected = 200
        self.files = [file_metadata(f"file-{i}") for i in range(200)]
        self.pages[2] = self.files[:100]
        self.assert_rejected("repeated")

    def test_malformed_file_pages_and_counts_fail_closed(self):
        for page in ({"message": "bad"}, [file_metadata()] * 101):
            with self.subTest(page_type=type(page)):
                self.pages[1] = page
                self.assert_rejected()
        self.pages.clear()
        for field in ("additions", "deletions"):
            for value in (-1, True, "1", None, 1.5):
                with self.subTest(field=field, value=value):
                    self.files = [file_metadata()]
                    self.files[0][field] = value
                    self.assert_rejected("integer counts")

    def test_missing_previous_filename_on_rename_is_rejected(self):
        for status in ("renamed", "copied"):
            with self.subTest(status=status):
                self.files[0]["status"] = status
                self.assert_rejected("file metadata")

    def test_stale_initial_or_final_head_causes_no_mutations(self):
        for read in (1, 2):
            with self.subTest(read=read):
                self.pr_reads = 0
                self.stale_on = read
                self.definitions.clear()
                self.assert_rejected("stale PR head")

    def test_changed_file_count_before_write_is_rejected(self):
        self.count_change_on = 2
        self.assert_rejected("file count changed")

    def test_head_rechecked_before_each_definition_and_label_write(self):
        self.definitions.clear()
        self.assertEqual(self.run_cli()[0], 0)
        for index, call in enumerate(self.calls):
            if call[0] != "GET":
                self.assertEqual(self.calls[index - 1], ("GET", PULL, None))

    def test_head_changing_between_writes_stops_remaining_mutations(self):
        self.stale_after_write = True
        status, _, errors = self.run_cli()
        self.assertEqual(status, 1)
        self.assertIn("stale PR head", errors)
        self.assertEqual(self.writes, [("POST", ISSUE, {"labels": ["size/XS"]})])
        self.assertIn("size/XL", self.labels)

    def test_api_read_errors_fail_without_logging_raw_error(self):
        for code in (401, 403, 404, 429, 500):
            with self.subTest(code=code):
                self.error = ("GET", PULL + "/files?per_page=100&page=1", code)
                self.assert_rejected(f"HTTP {code}")

    def test_label_permissions_failure_fails_job_without_deleting_old_size(self):
        self.error = ("POST", ISSUE, 403)
        status, _, errors = self.run_cli()
        self.assertEqual(status, 1)
        self.assertIn("HTTP 403", errors)
        self.assertEqual(self.labels, {"bug", "size/XL", "size/custom"})
        self.assertEqual(len(self.writes), 1)

    def test_definition_get_permission_errors_are_not_treated_as_missing(self):
        self.error = ("GET", REPO + "/labels/size%2FXS", 403)
        self.assert_rejected("HTTP 403")

    def test_missing_token_fails_visibly_without_requests(self):
        del self.env["GH_TOKEN"]
        status, _, errors = self.run_cli()
        self.assertEqual(status, 1)
        self.assertIn("GH_TOKEN is required", errors)
        self.assertEqual(self.calls, [])

    def test_invalid_api_urls_fail_before_requests(self):
        for url in (
            "http://api.github.com",
            "https://user:pass@api.github.com",
            "https://api.github.com?token=x",
            "https://api.github.com#fragment",
            "https:///missing-host",
            "https://api.github.com/ bad",
        ):
            with self.subTest(url=url):
                self.env["GITHUB_API_URL"] = url
                self.assert_rejected("HTTPS")
                self.assertEqual(self.calls, [])

    def test_trusted_enterprise_api_url_is_supported(self):
        self.env["GITHUB_API_URL"] = "https://github.enterprise.example/api/v3/"
        self.assertEqual(self.run_cli()[0], 0)

    def test_redirects_are_rejected_including_cross_host_and_downgrade(self):
        for url in ("https://attacker.example/", "http://api.github.com/", "/repos/x"):
            with self.subTest(url=url), self.assertRaises(labeler.LabelError):
                labeler.NoRedirect().redirect_request(
                    None, io.BytesIO(), 302, "", {}, url
                )

    def test_api_json_and_response_limits_fail_without_echoing_body(self):
        for raw in (TOKEN.encode(), b"x" * (labeler.RESPONSE_LIMIT + 1)):
            with self.subTest(length=len(raw)):
                self.raw["GET", PULL] = raw
                self.assert_rejected()

    def test_network_error_is_sanitized(self):
        for error in (urllib.error.URLError(TOKEN), http.client.BadStatusLine(TOKEN)):
            with self.subTest(error=type(error)):
                with patch.object(self, "open", side_effect=error):
                    self.assert_rejected("connection failed")

    def test_null_definition_is_not_treated_as_missing(self):
        self.raw["GET", REPO + "/labels/size%2FXS"] = b"null"
        self.assert_rejected("null metadata")

    def test_event_limit_accepts_1_mib_and_rejects_larger_before_requests(self):
        event = json.dumps(self.event).encode()
        raw = event + b" " * (labeler.EVENT_LIMIT - len(event))
        self.assertEqual(self.run_cli(raw)[0], 0)
        self.calls.clear()
        status, _, errors = self.run_cli(raw + b" ")
        self.assertEqual(status, 1)
        self.assertIn("1 MiB", errors)
        self.assertEqual(self.calls, [])

    def test_malformed_events_and_wrong_base_repo_are_rejected(self):
        original = copy.deepcopy(self.event)
        for field in ("repository", "base", "number", "sha"):
            with self.subTest(field=field):
                self.event = copy.deepcopy(original)
                if field == "repository":
                    self.event["repository"]["full_name"] = "attacker/fork"
                elif field == "base":
                    self.event["pull_request"]["base"]["repo"]["full_name"] = (
                        "attacker/fork"
                    )
                elif field == "number":
                    self.event["number"] = "17/../../labels"
                else:
                    self.event["pull_request"]["head"]["sha"] = INJECTION
                self.assert_rejected()
                self.assertEqual(self.calls, [])


class WorkflowTests(unittest.TestCase):
    def test_workflow_trust_boundary_and_isolated_permissions(self):
        workflow = (ROOT / ".github/workflows/pr-labels.yml").read_text()
        self.assertIn("pull_request_target:", workflow)
        self.assertIn("types: [opened, synchronize, reopened]", workflow)
        self.assertIn("contents: read", workflow)
        self.assertIn("issues: write", workflow)
        self.assertIn("pull-requests: write", workflow)
        self.assertIn("cancel-in-progress: true", workflow)
        self.assertIn(
            "group: pr-size-labels-${{ github.event.pull_request.number }}", workflow
        )
        self.assertIn(
            "github.event.pull_request.base.ref == github.event.repository.default_branch",
            workflow,
        )
        self.assertIn("ref: ${{ github.event.pull_request.base.sha }}", workflow)
        self.assertIn("repository: ${{ github.repository }}", workflow)
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn(
            'run: python3 -I scripts/label_pr.py --event "$GITHUB_EVENT_PATH"', workflow
        )
        self.assertIn("GH_TOKEN: ${{ github.token }}", workflow)
        for action in re.findall(r"uses: (\S+)", workflow):
            self.assertRegex(action, r"^[\w/-]+@[0-9a-f]{40}$")
        for forbidden in (
            "pull-requests: read",
            "contents: write",
            "actions: write",
            "write-all",
            "read-all",
            "secrets.",
            "head.sha",
            "head.ref",
            "pull_request:",
            "pull_request.title",
            "pull_request.body",
            "download-artifact",
            "pip install",
            "continue-on-error",
        ):
            self.assertNotIn(forbidden, workflow)


if __name__ == "__main__":
    unittest.main()
