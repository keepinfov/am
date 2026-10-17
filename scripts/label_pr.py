#!/usr/bin/env python3
"""Label PRs using metadata only. Run exclusively from a trusted base checkout."""

import argparse
import http.client
import json
import os
import re
import sys
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

COLORS = ("3CBF00", "5D9801", "7F7203", "A14C05", "C32607")
SIZES = ["XS", "S", "M", "L", "XL"]
EVENT_LIMIT = 1024 * 1024
RESPONSE_LIMIT = 16 * 1024 * 1024


class LabelError(Exception):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        fp.close()
        raise LabelError("GitHub API redirects are not permitted")


class GitHub:
    def __init__(self):
        self.token = os.environ.get("GH_TOKEN", "")
        if not self.token.strip():
            raise LabelError(
                "GH_TOKEN is required; label permissions must be available"
            )
        # GitHub supplies this trusted environment value, including on Enterprise.
        self.url = os.environ.get("GITHUB_API_URL", "https://api.github.com").rstrip(
            "/"
        )
        url = urllib.parse.urlsplit(self.url)
        if (
            url.scheme != "https"
            or not url.hostname
            or url.username is not None
            or url.password is not None
            or url.query
            or url.fragment
            or any(c.isspace() for c in self.url)
        ):
            raise LabelError("GITHUB_API_URL must be a trusted HTTPS API endpoint")
        self.opener = urllib.request.build_opener(NoRedirect())

    def request(self, method, path, data=None, missing=False):
        request = urllib.request.Request(
            self.url + path,
            data=json.dumps(data).encode() if data is not None else None,
            headers={
                "Authorization": f"Bearer {self.token}",
                "Accept": "application/vnd.github+json",
                "Content-Type": "application/json",
                "X-GitHub-Api-Version": "2022-11-28",
            },
            method=method,
        )
        try:
            with self.opener.open(request, timeout=30) as response:
                raw = response.read(RESPONSE_LIMIT + 1)
        except urllib.error.HTTPError as error:
            error.close()
            if missing and method == "GET" and error.code == 404:
                return None
            # Never print response bodies, headers, URLs or credentials.
            raise LabelError(
                f"GitHub API {method} failed (HTTP {error.code})"
            ) from None
        except (OSError, http.client.HTTPException):
            raise LabelError("GitHub API connection failed") from None
        if len(raw) > RESPONSE_LIMIT:
            raise LabelError("GitHub API response exceeds metadata limit")
        if not raw and method == "DELETE":
            return None
        result = json.loads(raw)
        if result is None:
            raise LabelError("GitHub API returned null metadata")
        return result


def count(value):
    if type(value) is not int or value < 0:
        raise LabelError("GitHub metadata must contain nonnegative integer counts")
    return value


def label_pr(event, rules, api):
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise LabelError("GITHUB_REPOSITORY must identify the trusted base repository")
    pr = event["pull_request"]
    number = count(event["number"])
    if (
        not number
        or pr["number"] != number
        or event["repository"]["full_name"] != repo
        or pr["base"]["repo"]["full_name"] != repo
    ):
        raise LabelError("event does not match the trusted base repository and PR")
    head = pr["head"]["sha"]
    if not isinstance(head, str) or not re.fullmatch(
        r"[0-9a-f]{40}|[0-9a-f]{64}", head
    ):
        raise LabelError("event must contain a valid head SHA")
    bounds = rules["upper_bounds"]
    if (
        rules["labels"] != SIZES
        or len(bounds) != len(SIZES) - 1
        or any(type(b) is not int or b <= 0 for b in bounds)
        or bounds != sorted(set(bounds))
        or not isinstance(rules["generated"], list)
        or not all(isinstance(p, str) for p in rules["generated"])
    ):
        raise LabelError("invalid trusted size policy")
    root = f"/repos/{repo}"
    pull = f"{root}/pulls/{number}"
    current = api.request("GET", pull)
    expected = count(current["changed_files"])

    def check_head():
        latest = api.request("GET", pull)
        if latest["head"]["sha"] != head:
            raise LabelError("stale PR head; refusing label mutations")
        if count(latest["changed_files"]) != expected:
            raise LabelError("PR file count changed; refusing label mutations")
        return latest

    if current["head"]["sha"] != head:
        raise LabelError("stale PR head; refusing label mutations")
    if expected > 3000:
        raise LabelError("PR exceeds GitHub's 3000-file metadata limit")
    files, churn = set(), 0
    for page in range(1, 31):
        batch = api.request("GET", f"{pull}/files?per_page=100&page={page}")
        if not isinstance(batch, list) or len(batch) > 100:
            raise LabelError("invalid PR file metadata page")
        for entry in batch:
            name = entry["filename"]
            old = entry.get("previous_filename", name)
            if (
                not isinstance(name, str)
                or not name
                or not isinstance(old, str)
                or not old
                or name in files
                or (
                    entry["status"] in ("renamed", "copied")
                    and "previous_filename" not in entry
                )
            ):
                raise LabelError("invalid or repeated PR file metadata")
            files.add(name)
            lines = count(entry["additions"]) + count(entry["deletions"])
            if not (name in rules["generated"] and old in rules["generated"]):
                churn += lines
        if len(files) > expected:
            raise LabelError("PR file metadata count mismatch")
        if len(batch) < 100:
            break
    if len(files) != expected:
        raise LabelError("PR file metadata is truncated or count mismatched")
    managed = [f"size/{size}" for size in SIZES]
    target = managed[sum(churn >= bound for bound in bounds)]
    missing = []
    for name, color in zip(managed, COLORS, strict=True):
        definition = api.request(
            "GET", f"{root}/labels/{urllib.parse.quote(name, safe='')}", missing=True
        )
        if definition is None:
            missing.append(
                {
                    "name": name,
                    "color": color,
                    "description": "PR API line churn: " + name,
                }
            )
        elif definition["name"] != name:
            raise LabelError("unexpected repository label definition")
    latest = check_head()
    labels = {label["name"] for label in latest["labels"]}
    if not all(isinstance(label, str) for label in labels):
        raise LabelError("invalid PR labels")

    def write(method, path, data=None):
        # GitHub offers no atomic head-conditional label write. Recheck each time.
        check_head()
        api.request(method, path, data)

    for definition in missing:
        write("POST", f"{root}/labels", definition)
    issue = f"{root}/issues/{number}/labels"
    if target not in labels:
        write("POST", issue, {"labels": [target]})
    for name in sorted((labels & set(managed)) - {target}):
        write("DELETE", f"{issue}/{urllib.parse.quote(name, safe='')}")
    # Zero API churn does not prove a binary, an empty file, or a raw diff count.
    print(f"{target}: {churn} reviewable API additions + deletions; binaries unknown")
    return target


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event", required=True, type=Path)
    args = parser.parse_args()
    try:
        api = GitHub()
        with args.event.open("rb") as source:
            raw = source.read(EVENT_LIMIT + 1)
        if len(raw) > EVENT_LIMIT:
            raise LabelError("event exceeds 1 MiB limit")
        root = Path(__file__).resolve().parents[1]
        with (root / ".repo-policy.toml").open("rb") as source:
            policy = tomllib.load(source)
        if policy["version"] != 1:
            raise LabelError("unsupported trusted policy version")
        label_pr(json.loads(raw), policy["size"], api)
    except LabelError as error:
        print(f"label_pr: {error}", file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, TypeError):
        print("label_pr: invalid event, policy, or API metadata", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
