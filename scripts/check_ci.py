#!/usr/bin/env python3
"""Read-only policy CI for Python 3.11+: python3 scripts/check_ci.py.

Requires GITHUB_EVENT_NAME (push or pull_request), GITHUB_EVENT_PATH,
GITHUB_REPOSITORY and GH_TOKEN with contents: read, not label/write permissions.
Use a full-history checkout containing the event's base and proposed head.
GITHUB_STEP_SUMMARY is optional. No hooks or Git configuration are installed.
GitHub signature verification is not maintainer approval.
"""

import json
import os
import re
import sys
from pathlib import Path

import repo_policy as core
from label_pr import GitHub, LabelError

EVENT_LIMIT = 1024 * 1024
OID = r"(?:[0-9a-f]{40}|[0-9a-f]{64})"


def event_range(root):
    name = os.environ.get("GITHUB_EVENT_NAME", "")
    if name not in ("push", "pull_request"):
        raise core.PolicyError("GITHUB_EVENT_NAME must be push or pull_request")
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo) or any(
        part in (".", "..") for part in repo.split("/")
    ):
        raise core.PolicyError("GITHUB_REPOSITORY must be an owner/repository name")
    path = os.environ.get("GITHUB_EVENT_PATH", "")
    if not path:
        raise core.PolicyError("GITHUB_EVENT_PATH is required")
    with Path(path).open("rb") as source:
        raw = source.read(EVENT_LIMIT + 1)
    if len(raw) > EVENT_LIMIT:
        raise core.PolicyError("GitHub event exceeds 1 MiB limit")
    event = json.loads(raw, parse_constant=core.reject_json_constant)
    if not isinstance(event, dict) or event["repository"]["full_name"] != repo:
        raise core.PolicyError("event repository does not match GITHUB_REPOSITORY")
    if name == "pull_request":
        pr = event["pull_request"]
        if pr["base"]["repo"]["full_name"] != repo:
            raise core.PolicyError(
                "PR base repository does not match GITHUB_REPOSITORY"
            )
        if (
            type(event["number"]) is not int
            or event["number"] <= 0
            or type(pr["number"]) is not int
            or pr["number"] != event["number"]
        ):
            raise core.PolicyError("event PR numbers must match")
        base, head = pr["base"]["sha"], pr["head"]["sha"]
    else:
        base, head = event["before"], event["after"]
        if os.environ.get("GITHUB_SHA") and os.environ["GITHUB_SHA"] != head:
            raise core.PolicyError("push after SHA does not match GITHUB_SHA")
    for value, initial_allowed in ((base, name == "push"), (head, False)):
        if not isinstance(value, str) or not re.fullmatch(OID, value):
            raise core.PolicyError("event base and head must be full hexadecimal SHAs")
        if initial_allowed and set(value) == {"0"}:
            continue  # The core includes all ancestors for an initial push.
        if set(value) == {"0"}:
            raise core.PolicyError("event head and PR base must not be all-zero")
        if core.revision(root, value) != value:
            raise core.PolicyError("event SHA does not match a local commit object")
    return name, repo, path, base, head


def verify_signatures(root, repo, base, head):
    ids = core.commit_ids(root, base, head)
    if any(
        not isinstance(oid, str) or not re.fullmatch(OID, oid) or set(oid) == {"0"}
        for oid in ids
    ):
        raise core.PolicyError("commit range contains an invalid commit ID")
    if not os.environ.get("GH_TOKEN", "").strip():
        raise core.PolicyError(
            "GH_TOKEN is required for read-only signature verification"
        )
    api = GitHub()
    for oid in ids:
        response = api.request("GET", f"/repos/{repo}/commits/{oid}")
        if not isinstance(response, dict) or response.get("sha") != oid:
            raise core.PolicyError("GitHub commit SHA does not match requested commit")
        verification = response["commit"]["verification"]
        if (
            not isinstance(verification, dict)
            or verification.get("verified") is not True
            or verification.get("reason") != "valid"
        ):
            raise core.PolicyError(f"{oid}: GitHub signature verification is not valid")
    print(
        f"GitHub verified {len(ids)} commit signatures; "
        "signature verification is not maintainer approval."
    )


def main():
    try:
        root = Path(
            os.fsdecode(
                core.git(Path.cwd(), "rev-parse", "--show-toplevel")
            ).removesuffix("\n")
        )
        name, repo, event, base, head = event_range(root)
        commands = [["files"], ["commits", "--base", base, "--head", head]]
        if name == "pull_request":
            options = ["--base", base, "--head", head]
            if os.environ.get("GITHUB_STEP_SUMMARY"):
                options += ["--summary", os.environ["GITHUB_STEP_SUMMARY"]]
            # Core PR validation may raise before writing its summary.
            commands += [["size", *options], ["pr", *options, "--event", event]]
        failed = False
        for command in commands:
            failed |= core.main(command) != 0
        verify_signatures(root, repo, base, head)
        return int(failed)
    except (core.PolicyError, LabelError) as error:
        print(f"check_ci: {error}", file=sys.stderr)
    except (OSError, ValueError, KeyError, TypeError, RecursionError):
        # Do not echo untrusted event/API bodies or credentials in diagnostics.
        print(
            "check_ci: invalid or unreadable event, Git, or API metadata",
            file=sys.stderr,
        )
    return 1


if __name__ == "__main__":
    sys.exit(main())
