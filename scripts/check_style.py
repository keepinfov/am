"""Check selected source files; only --fix permits formatting writes.

Use the Node version in .node-version and Python 3.11 or newer.
Install: npm ci --ignore-scripts; python3 -m pip install -r requirements-dev.txt
Install actionlint separately with scripts/install_actionlint.py --bin-dir DIR.
Override executable paths with NODE, RUFF, ACTIONLINT or AM_NODE_MODULES.
With no paths, select tracked and nonignored untracked Markdown, JSON, YAML,
Python, TOML and JavaScript modules. Conversations and generated trees are excluded.
--fix applies formatters and Ruff's import sorting only, never other lint fixes.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXCLUDED = {".git", "node_modules", "target", ".venv", "__pycache__"}
SUFFIXES = {".md", ".json", ".jsonc", ".yaml", ".yml", ".py", ".toml", ".mjs"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fix", action="store_true")
    parser.add_argument("paths", nargs="*")
    args = parser.parse_args()
    if args.paths:
        names = args.paths
    else:
        names = (
            subprocess.check_output(
                ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
                cwd=ROOT,
            )
            .decode()
            .split("\0")
        )
    files = []
    for name in names:
        if not name:
            continue
        path = Path(name)
        if path.is_absolute() or ".." in path.parts:
            parser.error(f"use repository-relative file paths: {name}")
        if (
            path.suffix.lower() in SUFFIXES
            and not EXCLUDED.intersection(path.parts)
            and path.name != "package-lock.json"
        ):
            if any(
                (ROOT / Path(*path.parts[:end])).is_symlink()
                for end in range(1, len(path.parts) + 1)
            ):
                parser.error(f"symlinks are not source files: {name}")
            source = ROOT / path
            if not args.paths and not source.exists():
                continue
            if not source.is_file():
                parser.error(f"not a regular source file: {name}")
            files.append(path.as_posix())
    files = sorted(set(files))
    if not files:
        if args.paths:
            parser.error("no applicable source files selected")
        print("No applicable source files selected")
        return 0

    modules = Path(os.environ.get("AM_NODE_MODULES", ROOT / "node_modules")).resolve()
    node = os.environ.get("NODE", "node")
    ruff = os.environ.get("RUFF", "ruff")
    actionlint = os.environ.get("ACTIONLINT", "actionlint")
    for command in (node, ruff, actionlint):
        if shutil.which(command) is None:
            parser.error(
                f"missing tool: {command}; see installation instructions above"
            )
    node_version = (ROOT / ".node-version").read_text().strip()
    if (
        subprocess.check_output([node, "--version"], text=True).strip()
        != f"v{node_version}"
    ):
        parser.error(f"node must be version {node_version}")
    pins = json.loads((ROOT / "package.json").read_text())["devDependencies"]
    for package, version in pins.items():
        manifest = modules / package / "package.json"
        if not manifest.is_file():
            parser.error(f"missing {package}; run npm ci --ignore-scripts")
        if json.loads(manifest.read_text())["version"] != version:
            parser.error(
                f"{package} must be version {version}; run npm ci --ignore-scripts"
            )
    ruff_version = (ROOT / "requirements-dev.txt").read_text().strip().split("==")[1]
    if (
        subprocess.check_output([ruff, "--version"], text=True).strip()
        != f"ruff {ruff_version}"
    ):
        parser.error(f"ruff must be version {ruff_version}")
    actionlint_version = json.loads((ROOT / "tools/actionlint.json").read_text())[
        "version"
    ]
    if (
        subprocess.check_output([actionlint, "-version"], text=True).splitlines()[0]
        != actionlint_version
    ):
        parser.error(f"actionlint must be version {actionlint_version}")

    prettier = [node, str(modules / "prettier/bin/prettier.cjs")]
    markdownlint = [node, str(ROOT / "scripts/lint_markdown.mjs")]
    taplo = [node, str(modules / "@taplo/cli/dist/cli.js")]
    groups = {
        "prettier": [
            f
            for f in files
            if Path(f).suffix.lower()
            in {".md", ".json", ".jsonc", ".yml", ".yaml", ".mjs"}
        ],
        "markdown": [f for f in files if f.lower().endswith(".md")],
        "python": [f for f in files if f.lower().endswith(".py")],
        "toml": [f for f in files if f.lower().endswith(".toml")],
        "workflow": [
            f
            for f in files
            if f.startswith(".github/workflows/")
            and Path(f).suffix.lower() in {".yml", ".yaml"}
        ],
    }
    # Existing, explicit ./ paths cannot be options. Prettier prefers existing
    # literal paths over glob interpretation; Markdownlint's API never globs.
    groups = {key: [f"./{name}" for name in names] for key, names in groups.items()}
    commands = []
    if groups["prettier"]:
        commands.append(
            prettier + ["--write" if args.fix else "--check", "--"] + groups["prettier"]
        )
    if groups["markdown"]:
        commands.append(markdownlint + groups["markdown"])
    if groups["python"]:
        if args.fix:
            commands.append(
                [ruff, "check", "--select", "I", "--fix", "--", *groups["python"]]
            )
        commands.append(
            [
                ruff,
                "format",
                *([] if args.fix else ["--check"]),
                "--",
                *groups["python"],
            ]
        )
        commands.append([ruff, "check", "--", *groups["python"]])
    if groups["workflow"]:
        # Auxiliary shellcheck/pyflakes are not part of this pinned toolchain.
        commands.append([actionlint, "-shellcheck=", "-pyflakes=", *groups["workflow"]])
    failed = False
    for command in commands:
        print("+ " + " ".join(command), flush=True)
        failed |= (
            subprocess.run(
                command,
                cwd=ROOT,
                check=False,
                env={**os.environ, "AM_NODE_MODULES": str(modules)},
            ).returncode
            != 0
        )
    # Taplo treats even existing filenames as globs. Stdin avoids expansion and
    # ensures only selected files are checked or written, including names with [].
    for name in groups["toml"]:
        source = (ROOT / name).read_bytes()
        command = taplo + ["lint", "--no-schema", "--", "-"]
        print(f"+ {' '.join(command)} < {name}", flush=True)
        if subprocess.run(command, cwd=ROOT, input=source, check=False).returncode:
            failed = True
            continue
        command = taplo + [
            "format",
            *([] if args.fix else ["--check"]),
            "--stdin-filepath",
            name,
            "--",
            "-",
        ]
        print(f"+ {' '.join(command)} < {name}", flush=True)
        result = subprocess.run(
            command,
            cwd=ROOT,
            input=source,
            stdout=subprocess.PIPE if args.fix else None,
            check=False,
        )
        failed |= result.returncode != 0
        if args.fix and result.returncode == 0:
            (ROOT / name).write_bytes(result.stdout)
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
