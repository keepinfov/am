#!/usr/bin/env python3
"""Mechanical repository policy, Python 3.11+, with no third-party dependencies.

Signature armor is required, not authenticated by header inspection. Use
commits --verify-signatures to delegate cryptographic verification and trust to
existing Git configuration; GitHub's signing ruleset remains authoritative in CI.
"""

import argparse
import fnmatch
import json
import os
import re
import stat
import subprocess
import sys
import tomllib
from pathlib import Path


class PolicyError(Exception):
    pass


def git(root, *args):
    result = subprocess.run(
        ["git", "--no-pager", "--no-replace-objects", *args],
        cwd=root,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        raise PolicyError(f"git {args[0]}: {os.fsdecode(result.stderr).strip()}")
    return result.stdout


def load_policy(root):
    path = root / ".repo-policy.toml"
    if path.is_symlink() or not path.is_file():
        raise PolicyError(".repo-policy.toml must be a regular non-symlink file")
    try:
        policy = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise PolicyError(f".repo-policy.toml: {error}") from error
    if type(policy.get("version")) is not int or policy["version"] != 1:
        raise PolicyError(".repo-policy.toml: version must be integer 1")
    schema = {
        "commit": {
            "types": list,
            "scopes": list,
            "body_required": list,
            "subject_target": int,
            "subject_limit": int,
            "body_line_limit": int,
            "body_paragraph_limit": int,
            "prose_min_chars": int,
            "prose_min_words": int,
            "require_signature": bool,
            "nonimperative_start": str,
            "forbidden": list,
            "sync_merge": str,
        },
        "branch": {"pattern": str, "max_length": int},
        "pr": {
            "base": str,
            "event_max_bytes": int,
            "required_sections": list,
            "large_sections": list,
            "large_labels": list,
            "explanation_min_chars": int,
            "explanation_min_words": int,
            "placeholder": str,
        },
        "changelog": {
            "directory": str,
            "kinds": list,
            "slug_pattern": str,
            "exclude": list,
        },
        "files": {
            "require_lf": bool,
            "require_final_newline": bool,
            "reject_trailing_whitespace": bool,
            "reject_symlinks": bool,
            "structured_toml": list,
            "structured_json": list,
            "markdown": list,
            "conflict_marker": str,
        },
        "size": {
            "labels": list,
            "upper_bounds": list,
            "generated": list,
            "source": list,
            "tests": list,
            "docs": list,
            "tooling": list,
        },
    }
    expected = {"version", *schema}
    if set(policy) != expected:
        raise PolicyError(
            f".repo-policy.toml sections: missing {sorted(expected - set(policy))}; unknown {sorted(set(policy) - expected)}"
        )
    for section, fields in schema.items():
        table = policy[section]
        if not isinstance(table, dict):
            raise PolicyError(f".repo-policy.toml: {section} must be a table")
        if set(table) != set(fields):
            raise PolicyError(
                f".repo-policy.toml [{section}]: missing {sorted(set(fields) - set(table))}; unknown {sorted(set(table) - set(fields))}"
            )
        for key, expected_type in fields.items():
            value = table[key]
            name = f".repo-policy.toml: {section}.{key}"
            if type(value) is not expected_type:
                raise PolicyError(f"{name} must be {expected_type.__name__}")
            if expected_type is int and value <= 0:
                raise PolicyError(f"{name} must be positive")
            if expected_type is str and (not value.strip() or "\0" in value):
                raise PolicyError(f"{name} must be a nonempty string without NUL")
            if expected_type is list:
                item_type = int if (section, key) == ("size", "upper_bounds") else str
                if not value or any(type(item) is not item_type for item in value):
                    raise PolicyError(
                        f"{name} must be a nonempty list of {item_type.__name__}"
                    )
                if item_type is str and any(
                    not item.strip() or "\0" in item for item in value
                ):
                    raise PolicyError(
                        f"{name} entries must be nonempty strings without NUL"
                    )
                if len(value) != len(set(value)):
                    raise PolicyError(f"{name} entries must be unique")
    commit, size = policy["commit"], policy["size"]
    if commit["subject_target"] > commit["subject_limit"]:
        raise PolicyError(
            ".repo-policy.toml: commit.subject_target exceeds subject_limit"
        )
    if not set(commit["body_required"]) <= set(commit["types"]):
        raise PolicyError(
            ".repo-policy.toml: commit.body_required must be a subset of commit.types"
        )
    bounds = size["upper_bounds"]
    if bounds != sorted(bounds) or any(bound <= 0 for bound in bounds):
        raise PolicyError(
            ".repo-policy.toml: size.upper_bounds must be strictly increasing positive integers"
        )
    if len(size["labels"]) != len(bounds) + 1:
        raise PolicyError(
            ".repo-policy.toml: size.labels must have one more entry than upper_bounds"
        )
    if not set(policy["pr"]["large_labels"]) <= set(size["labels"]):
        raise PolicyError(
            ".repo-policy.toml: pr.large_labels must be a subset of size.labels"
        )
    for section, key, pattern in (
        ("commit", "types", r"[a-z]+"),
        ("commit", "scopes", r"[a-z]+"),
        ("changelog", "kinds", r"[a-z]+"),
        ("size", "labels", r"[A-Z][A-Z0-9_-]*"),
    ):
        if any(not re.fullmatch(pattern, item) for item in policy[section][key]):
            raise PolicyError(
                f".repo-policy.toml: {section}.{key} entries must match {pattern}"
            )
    for section, key in (
        ("commit", "nonimperative_start"),
        ("commit", "sync_merge"),
        ("commit", "forbidden"),
        ("branch", "pattern"),
        ("pr", "placeholder"),
        ("changelog", "slug_pattern"),
        ("files", "conflict_marker"),
    ):
        patterns = policy[section][key]
        for pattern in patterns if isinstance(patterns, list) else [patterns]:
            try:
                re.compile(f"(?:{pattern})" if section == "changelog" else pattern)
            except (re.error, OverflowError, RecursionError) as error:
                raise PolicyError(
                    f".repo-policy.toml: {section}.{key}: invalid regex: {error}"
                ) from error
    paths = [policy["changelog"]["directory"], *policy["changelog"]["exclude"]]
    for section, keys in (
        ("files", ("structured_toml", "structured_json", "markdown")),
        ("size", ("generated", "source", "tests", "docs", "tooling")),
    ):
        paths.extend(item for key in keys for item in policy[section][key])
    if any(p.startswith("/") or ".." in p.split("/") for p in paths):
        raise PolicyError(
            ".repo-policy.toml: file paths/patterns must be repository-relative without '..'"
        )
    directory = policy["changelog"]["directory"]
    if any(part in ("", ".") for part in directory.split("/")) or any(
        c in directory for c in "*?[]"
    ):
        raise PolicyError(
            ".repo-policy.toml: changelog.directory must be a literal relative directory"
        )
    if policy["changelog"]["exclude"] != [f"{directory}/README.md"]:
        raise PolicyError(
            ".repo-policy.toml: changelog.exclude must exempt only the directory's README.md"
        )
    return policy


def matches(path, patterns):
    return any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns)


def reject_json_constant(value):
    raise ValueError(f"invalid JSON constant: {value}")


def revision(root, value):
    if value != "HEAD" and not re.fullmatch(r"[0-9a-fA-F]{7,64}", value):
        raise PolicyError("revision must be HEAD or a hexadecimal commit ID")
    if set(value) == {"0"}:
        raise PolicyError("all-zero revision has no tree")
    return git(root, "rev-parse", "--verify", f"{value}^{{commit}}").decode().strip()


def commit_ids(root, base, head):
    # An initial push has no base commit; include the root and all ancestors.
    if base is not None and not re.fullmatch(r"(?:0{40}|0{64})", base):
        base = revision(root, base)
    else:
        base = None
    return (
        git(root, "rev-list", "--reverse", f"{base}..{head}" if base else head)
        .decode()
        .splitlines()
    )


def signature_present(raw):
    headers = raw.split(b"\n\n", 1)[0]
    match = re.search(rb"(?:^|\n)gpgsig (.*(?:\n .*)*)", headers)
    if not match:
        return False
    armor = match[1].replace(b"\n ", b"\n").strip()
    return bool(
        re.fullmatch(
            rb"-----BEGIN (SSH|PGP) SIGNATURE-----\n.+\n-----END \1 SIGNATURE-----",
            armor,
            re.DOTALL,
        )
    )


def subject_errors(subject, policy, warnings=None):
    rules = policy["commit"]
    errors = []
    if len(subject) > rules["subject_limit"]:
        errors.append(f"subject exceeds {rules['subject_limit']} characters")
    elif len(subject) > rules["subject_target"] and warnings is not None:
        warnings.append(
            f"subject exceeds target of {rules['subject_target']} characters"
        )
    match = re.fullmatch(r"([a-z]+)\(([a-z]+)\)(!)?: ([a-z].*)", subject)
    if not match:
        return errors + [
            "use type(scope)[!]: lowercase-start English imperative subject"
        ]
    kind, scope, _, description = match.groups()
    if kind not in rules["types"] or scope not in rules["scopes"]:
        errors.append(
            "unknown commit type or scope; update policy deliberately if needed"
        )
    if not description.isascii() or any(ord(c) < 32 for c in description):
        errors.append("subject description must use printable English ASCII")
    if description.endswith("."):
        errors.append("subject must not end with a period")
    if subject != subject.rstrip() or "\x7f" in subject:
        errors.append(
            "subject must not contain trailing whitespace or control characters"
        )
    if re.search(rules["nonimperative_start"], description):
        errors.append("start the subject with an imperative verb (mechanical guidance)")
    return errors


def message_errors(message, parents, policy, warnings=None):
    rules = policy["commit"]
    errors = []
    # Run on the whole message, including subject-only and merge messages.
    if any(re.search(pattern, message) for pattern in rules["forbidden"]):
        errors.append("message contains bullets, fences, recap or attribution language")
    lines = message.rstrip("\n").split("\n")
    subject = lines[0]
    sync_merge = len(parents) >= 2 and re.fullmatch(rules["sync_merge"], subject)
    if not sync_merge:
        checked_subject = (
            re.sub(r" \(#[1-9][0-9]*\)$", "", subject) if len(parents) >= 2 else subject
        )
        errors.extend(subject_errors(checked_subject, policy, warnings))
    body = "\n".join(lines[1:]).strip("\n")
    if len(lines) > 1 and lines[1] != "":
        errors.append("separate subject and body with a blank line")
    if len(parents) >= 2 and not sync_merge:
        if body:
            errors.append("conventional PR merge commits must have an empty body")
        return errors
    prose = []
    breaking = []
    in_trailers = False
    for line in body.splitlines():
        if len(line) > rules["body_line_limit"]:
            errors.append(f"body line exceeds {rules['body_line_limit']} characters")
        if line != line.rstrip():
            errors.append("body has trailing whitespace")
        if re.fullmatch(r"(?:Fixes|Refs) #[1-9][0-9]*", line):
            in_trailers = True
        elif re.match(r"(?:Fixes|Refs) #", line):
            errors.append("reference trailers must be exactly Fixes #N or Refs #N")
        elif line.startswith("BREAKING CHANGE:"):
            in_trailers = True
            breaking.append(line.removeprefix("BREAKING CHANGE:").strip())
        elif re.match(r"[A-Za-z][A-Za-z -]*:", line):
            errors.append(
                "only Fixes #N, Refs #N and BREAKING CHANGE trailers are allowed"
            )
        elif in_trailers and line.strip():
            errors.append("trailers must follow prose and occupy their own lines")
        elif not in_trailers:
            prose.append(line)
    prose_text = "\n".join(prose).strip()
    paragraphs = [p for p in prose_text.split("\n\n") if p.strip()]
    if len(paragraphs) > rules["body_paragraph_limit"]:
        errors.append(f"body exceeds {rules['body_paragraph_limit']} prose paragraphs")
    kind = subject.split("(", 1)[0]
    needs_prose = kind in rules["body_required"] and len(parents) < 2
    if (paragraphs or needs_prose) and (
        len(prose_text) < rules["prose_min_chars"]
        or len(re.findall(r"\b\w+\b", prose_text)) < rules["prose_min_words"]
    ):
        errors.append(f"{kind} commits require a useful prose body, not a stub")
    bang = bool(re.match(r"[a-z]+\([a-z]+\)!:", subject))
    if bang != bool(breaking) or len(breaking) > 1 or any(not b for b in breaking):
        errors.append(
            "breaking changes require synchronized ! and one explained BREAKING CHANGE footer"
        )
    return errors


def read_commit(root, oid):
    raw = git(root, "cat-file", "commit", oid)
    headers, message = raw.split(b"\n\n", 1)
    parents = re.findall(rb"^parent ([0-9a-f]+)$", headers, re.MULTILINE)
    return raw, message.decode("utf-8"), parents


def check_commits(root, policy, base, head, verify=False):
    errors, warnings = [], []
    ids = commit_ids(root, base, head)
    for oid in ids:
        raw, message, parents = read_commit(root, oid)
        errors.extend(
            f"{oid[:12]}: {error}"
            for error in message_errors(message, parents, policy, warnings)
        )
        if policy["commit"]["require_signature"] and not signature_present(raw):
            errors.append(
                f"{oid[:12]}: missing SSH/GPG signature armor in commit headers"
            )
        if verify:
            git(root, "verify-commit", oid)
    warnings.append(
        f"Cryptographically verified {len(ids)} commit signatures using configured Git."
        if verify
        else "Signature presence only; cryptographic verification/trust deferred to Git configuration or GitHub ruleset."
    )
    return errors, warnings


def unfenced_lines(text):
    fence = None
    for number, line in enumerate(text.splitlines(), 1):
        marker = re.match(r"^ {0,3}(`{3,}|~{3,})(.*)$", line)
        if marker:
            token, rest = marker.groups()
            if fence is None:
                fence = token
            elif token[0] == fence[0] and len(token) >= len(fence) and not rest.strip():
                fence = None
            continue
        if fence is None:
            yield number, line


def check_files(root, policy):
    rules = policy["files"]
    paths = set(
        git(root, "ls-files", "-z", "--cached", "--others", "--exclude-standard").split(
            b"\0"
        )
    ) - {b""}
    errors, notices = [], []
    for raw in sorted(paths):
        name = os.fsdecode(raw)
        path = root / name
        # Refuse links in every component before opening, even links inside root.
        parts = [
            root.joinpath(*Path(name).parts[:i])
            for i in range(1, len(Path(name).parts) + 1)
        ]
        if any(p.is_symlink() for p in parts):
            if rules["reject_symlinks"]:
                errors.append(f"{name!r}: symlinks are not permitted")
            continue
        if not path.exists():
            continue  # A tracked deletion is not file content.
        if not stat.S_ISREG(path.stat().st_mode):
            errors.append(f"{name!r}: expected a regular file (submodules unsupported)")
            continue
        fragment = (
            name.startswith(policy["changelog"]["directory"] + "/")
            and name not in policy["changelog"]["exclude"]
        )
        if fragment and not fragment_kind(name, policy):
            errors.append(
                f"{name!r}: invalid changelog fragment name; use <slug>.<kind>.md with kinds {', '.join(policy['changelog']['kinds'])}"
            )
        data = path.read_bytes()
        structured = matches(name, rules["structured_toml"] + rules["structured_json"])
        try:
            if b"\0" in data:
                raise UnicodeError("NUL byte")
            text = data.decode("utf-8")
        except UnicodeError:
            if fragment:
                errors.append(f"{name!r}: changelog fragment must be UTF-8 without NUL")
            elif structured:
                errors.append(f"{name!r}: structured text must be UTF-8 without NUL")
            else:
                notices.append(f"{name!r}: binary/non-UTF-8 content skipped")
            continue
        if fragment and not meaningful(text, policy):
            errors.append(
                f"{name!r}: changelog fragment requires substantive text, not a placeholder"
            )
        if rules["require_lf"] and "\r" in text:
            errors.append(f"{name!r}: use LF line endings")
        if rules["require_final_newline"] and text and not text.endswith("\n"):
            errors.append(f"{name!r}: missing final newline")
        for number, line in enumerate(text.splitlines(), 1):
            if rules["reject_trailing_whitespace"] and line.rstrip(" \t") != line:
                errors.append(f"{name!r}:{number}: trailing whitespace")
        visible = (
            unfenced_lines(text)
            if matches(name, rules["markdown"])
            else enumerate(text.splitlines(), 1)
        )
        for number, line in visible:
            if re.fullmatch(rules["conflict_marker"], line):
                errors.append(f"{name!r}:{number}: unresolved conflict marker")
        try:
            if matches(name, rules["structured_toml"]):
                tomllib.loads(text)
            if matches(name, rules["structured_json"]):
                json.loads(text, parse_constant=reject_json_constant)
        except ValueError as error:
            errors.append(f"{name!r}: invalid structured text: {error}")
    return errors, notices


def tree(root, oid):
    result = {}
    for record in git(root, "ls-tree", "-rz", "--full-tree", oid).split(b"\0"):
        if record:
            metadata, path = record.split(b"\t", 1)
            mode, kind, blob = metadata.split()
            result[os.fsdecode(path)] = (mode, kind, blob.decode())
    return result


def diff_entries(root, base, head):
    records = iter(
        git(
            root,
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--numstat",
            "-z",
            base,
            head,
            "--",
        ).split(b"\0")
    )
    entries = []
    for record in records:
        if not record:
            continue
        additions, deletions, path = record.split(b"\t", 2)
        old = new = path
        if not path:
            old, new = next(records), next(records)
        entries.append(
            {
                "old": os.fsdecode(old),
                "path": os.fsdecode(new),
                "additions": int(additions) if additions != b"-" else 0,
                "deletions": int(deletions) if deletions != b"-" else 0,
                "binary": additions == b"-",
            }
        )
    return entries


def measure_size(root, policy, base, head):
    base = revision(root, base)
    bases = git(root, "merge-base", "--all", base, head).decode().splitlines()
    if len(bases) != 1:
        raise PolicyError("size requires one unambiguous merge base")
    base = bases[0]
    entries = diff_entries(root, base, head)
    rules = policy["size"]
    groups = {
        name: dict(additions=0, deletions=0, files=0, binary_files=0)
        for name in ("source", "tests", "docs", "generated", "tooling", "other")
    }
    totals = dict(additions=0, deletions=0, files=0, binary_files=0)
    reviewable = 0
    for entry in entries:
        name = entry["path"]
        group = (
            "generated"
            if name in rules["generated"] and entry["old"] in rules["generated"]
            else next(
                (
                    g
                    for g in ("source", "tests", "tooling", "docs")
                    if matches(name, rules[g])
                ),
                "other",
            )
        )
        entry["group"] = group
        for counts in (groups[group], totals):
            counts["additions"] += entry["additions"]
            counts["deletions"] += entry["deletions"]
            counts["files"] += 1
            counts["binary_files"] += int(entry["binary"])
        if group != "generated":
            reviewable += entry["additions"] + entry["deletions"]
    label = rules["labels"][sum(reviewable >= bound for bound in rules["upper_bounds"])]
    before, after = tree(root, base), tree(root, head)
    source_before = {
        p
        for p, info in before.items()
        if matches(p, rules["source"]) and info[0] in (b"100644", b"100755")
    }
    cache = {}

    def lines(info):
        if info[0] not in (b"100644", b"100755"):
            return 0
        blob = info[2]
        if blob not in cache:
            data = git(root, "cat-file", "blob", blob)
            cache[blob] = (
                0
                if b"\0" in data
                else data.count(b"\n") + int(bool(data) and not data.endswith(b"\n"))
            )
        return cache[blob]

    base_lines = sum(lines(before[p]) for p in source_before)
    modified, deleted, added = set(), set(), set()
    source_churn = 0
    for entry in entries:
        old, new = entry["old"], entry["path"]
        old_source = old in source_before
        new_source = (
            new in after
            and matches(new, rules["source"])
            and after[new][0] in (b"100644", b"100755")
        )
        if old_source or new_source:
            source_churn += entry["additions"] + entry["deletions"]
        if old_source:
            (modified if new_source else deleted).add(old)
        if new_source and not old_source:
            added.add(new)
    return {
        "base": base,
        "head": head,
        "label": label,
        "totals": totals,
        "groups": groups,
        "reviewable_churn": reviewable,
        "source": {
            "base_files": len(source_before),
            "base_lines": base_lines,
            "modified_base_files": len(modified),
            "deleted_base_files": len(deleted),
            "affected_base_files": len(modified | deleted),
            "affected_base_fraction": len(modified | deleted) / len(source_before)
            if source_before
            else None,
            "new_files": len(added),
            "new_lines": sum(lines(after[p]) for p in added),
            "churn": source_churn,
            "churn_to_base_lines": source_churn / base_lines if base_lines else None,
        },
        "changes": entries,
    }


def meaningful(text, policy):
    text = re.sub(r"<!--.*?(?:-->|\Z)", "", text, flags=re.DOTALL).strip()
    text = "\n".join(line for _, line in unfenced_lines(text))
    text = re.sub(r"(?m)^ {0,3}#{1,6} .*", "", text)
    text = re.sub(r"(?m)^\s*[-*+]\s*(?:\[[ xX]\]\s*)?", "", text)
    text = text.replace("**", "").strip()
    rules = policy["pr"]
    return (
        len(text) >= rules["explanation_min_chars"]
        and len(re.findall(r"\b\w+\b", text)) >= rules["explanation_min_words"]
        and not any(
            re.search(rules["placeholder"], line.strip())
            for line in text.splitlines()
            if line.strip()
        )
    )


def fragment_kind(path, policy):
    rules = policy["changelog"]
    kinds = "|".join(map(re.escape, rules["kinds"]))
    pattern = (
        rf"{re.escape(rules['directory'])}/(?:{rules['slug_pattern']})\.({kinds})\.md"
    )
    match = re.fullmatch(pattern, path)
    return match.groups()[-1] if match else None


def pr_errors(root, policy, event, base, head, size):
    pr = event.get("pull_request")
    if not isinstance(pr, dict):
        raise PolicyError("event must contain a pull_request object")
    errors = []
    metadata = pr.get("head", {})
    if not isinstance(metadata, dict) or metadata.get("sha") != head:
        raise PolicyError("event head SHA does not match supplied head commit")
    branch = metadata.get("ref")
    if (
        not isinstance(branch, str)
        or len(branch) > policy["branch"]["max_length"]
        or not re.fullmatch(policy["branch"]["pattern"], branch)
    ):
        errors.append(
            f"PR head branch must match policy and not exceed {policy['branch']['max_length']} characters"
        )
    target = pr.get("base", {})
    if not isinstance(target, dict) or target.get("ref") != policy["pr"]["base"]:
        errors.append(f"PR must target {policy['pr']['base']}")
    title, body = pr.get("title"), pr.get("body")
    if not isinstance(title, str) or "\n" in title or "\r" in title:
        errors.append("PR title must be a single-line conventional subject")
        title = ""
    else:
        errors.extend(subject_errors(title, policy))
        if any(re.search(pattern, title) for pattern in policy["commit"]["forbidden"]):
            errors.append("PR title contains prohibited recap or attribution language")
    if not isinstance(body, str):
        body = ""
    # Commented template instructions and fenced examples are not explanations.
    body = re.sub(r"<!--.*?(?:-->|\Z)", "", body, flags=re.DOTALL)
    body = "\n".join(line for _, line in unfenced_lines(body))
    sections = {}
    heading = None
    for line in body.splitlines():
        match = re.fullmatch(r"## ([^#].*?)\s*", line)
        if match:
            heading = match[1]
            if heading in sections:
                errors.append(f"duplicate PR section: {heading}")
            sections[heading] = ""
        elif heading:
            sections[heading] += line + "\n"
    for name in policy["pr"]["required_sections"]:
        explanation = sections.get(name, "")
        if name == "Change Impact":
            explanation = re.sub(
                r"(?m)^Impact: (?:user-facing|internal)\s*$", "", explanation
            )
        if not meaningful(explanation, policy):
            errors.append(
                f"## {name} requires an actual explanation, not a placeholder"
            )
    impacts = re.findall(
        r"(?m)^Impact: (user-facing|internal)$", sections.get("Change Impact", "")
    )
    if len(impacts) != 1 or len(re.findall(r"(?m)^\s*Impact:", body)) != 1:
        errors.append(
            "Change Impact requires exactly one Impact: user-facing or Impact: internal line"
        )
    if size["label"] in policy["pr"]["large_labels"]:
        large = sections.get("Large Change", "")
        fields = {}
        current = None
        labels = "|".join(map(re.escape, policy["pr"]["large_sections"]))
        for line in large.splitlines():
            clean = line.replace("**", "").strip()
            if clean.startswith("### "):
                clean = clean[4:].rstrip(":") + ":"
            match = re.fullmatch(rf"({labels}):[ \t]*(.*)", clean)
            if match:
                current = match[1]
                if current in fields:
                    errors.append(f"duplicate Large Change field: {current}")
                fields[current] = match[2] + "\n"
            elif current:
                fields[current] += line + "\n"
        for name in policy["pr"]["large_sections"]:
            if not meaningful(sections.get(name, fields.get(name, "")), policy):
                errors.append(f"{size['label']} change requires explained {name}")
    fragments = []
    head_tree = tree(root, head)
    for change in size["changes"]:
        path = change["path"]
        kind = fragment_kind(path, policy)
        if (
            kind
            and path not in policy["changelog"]["exclude"]
            and change["additions"] + change["deletions"] > 0
            and not change["binary"]
        ):
            info = head_tree.get(path)
            if info and info[0] in (b"100644", b"100755"):
                text = git(root, "cat-file", "blob", info[2]).decode("utf-8")
                if meaningful(text, policy):
                    fragments.append(kind)
    breaking = bool(re.match(r"[a-z]+\([a-z]+\)!:", title))
    for oid in commit_ids(root, base, head):
        _, message, _ = read_commit(root, oid)
        breaking |= bool(
            re.search(r"(?m)^(?:[a-z]+\([a-z]+\)!:|BREAKING CHANGE:)", message)
        )
    if impacts == ["user-facing"] and not fragments:
        errors.append(
            "user-facing PR requires a new/changed substantive changelog fragment"
        )
    if breaking and (impacts != ["user-facing"] or "breaking" not in fragments):
        errors.append(
            "breaking changes require user-facing impact and a breaking changelog fragment"
        )
    return errors


def summary(size):
    lines = [
        f"## Change Size: {size['label']}",
        "",
        f"Reviewable churn: {size['reviewable_churn']} lines (only generated lockfiles excluded).",
        "Binary files have no Git line counts; additions/deletions report text lines only.",
        "",
        "| Group | Additions | Deletions | Files | Binary Files |",
        "| --- | ---: | ---: | ---: | ---: |",
    ]
    for name, counts in [*size["groups"].items(), ("total", size["totals"])]:
        lines.append(
            f"| {name} | {counts['additions']} | {counts['deletions']} | {counts['files']} | {counts['binary_files']} |"
        )
    source = size["source"]
    fraction = source["affected_base_fraction"]
    churn = source["churn_to_base_lines"]
    lines += [
        "",
        f"Source base-file footprint: {source['affected_base_files']}/{source['base_files']} "
        + (f"({fraction:.1%})." if fraction is not None else "(no base source files)."),
        f"Modified: {source['modified_base_files']}; deleted: {source['deleted_base_files']}; new separately: {source['new_files']} files / {source['new_lines']} lines.",
        f"Source churn / base source lines: {source['churn']}/{source['base_lines']} "
        + (
            f"({churn:.1%}; churn can exceed 100%)."
            if churn is not None
            else "(no base source lines)."
        ),
    ]
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser(
        "files", help="check tracked and nonignored new working-tree files"
    )
    commits = commands.add_parser(
        "commits", help="check added commits, including roots on initial pushes"
    )
    commits.add_argument("--base")
    commits.add_argument("--head", required=True)
    commits.add_argument(
        "--verify-signatures",
        action="store_true",
        help="also run git verify-commit using existing trust configuration",
    )
    for name in ("size", "pr"):
        command = commands.add_parser(name)
        command.add_argument("--base", required=True)
        command.add_argument("--head", required=True)
        command.add_argument("--summary", type=Path)
        if name == "size":
            command.add_argument("--json", action="store_true")
        else:
            command.add_argument("--event", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        root = Path(
            os.fsdecode(git(Path.cwd(), "rev-parse", "--show-toplevel")).removesuffix(
                "\n"
            )
        )
        policy = load_policy(root)
        errors, notices = [], []
        if args.command == "files":
            errors, notices = check_files(root, policy)
        else:
            head = revision(root, args.head)
            if args.command == "commits":
                errors, notices = check_commits(
                    root, policy, args.base, head, args.verify_signatures
                )
            else:
                size = measure_size(root, policy, args.base, head)
                report = summary(size)
                if args.command == "pr":
                    with args.event.open("rb") as stream:
                        data = stream.read(policy["pr"]["event_max_bytes"] + 1)
                    if len(data) > policy["pr"]["event_max_bytes"]:
                        raise PolicyError("PR event exceeds configured byte limit")
                    event = json.loads(data, parse_constant=reject_json_constant)
                    if not isinstance(event, dict):
                        raise PolicyError("PR event must be a JSON object")
                    errors = pr_errors(root, policy, event, args.base, head, size)
                if args.summary:
                    args.summary.write_text(report, encoding="utf-8", newline="\n")
                print(
                    json.dumps(size, ensure_ascii=True, indent=2)
                    if args.command == "size" and args.json
                    else report,
                    end="\n" if args.command == "size" and args.json else "",
                )
        for notice in notices:
            print(f"note: {notice}", file=sys.stderr)
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return int(bool(errors))
    except (
        PolicyError,
        OSError,
        ValueError,
        KeyError,
        TypeError,
        RecursionError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
