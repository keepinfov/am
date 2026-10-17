"""Install a checksum-pinned release, never a downloaded installer script.

Usage: python3 scripts/install_actionlint.py --bin-dir /path/to/bin
Run only from a trusted checkout, without elevated privileges or secrets.
"""

import argparse
import hashlib
import io
import json
import os
import platform
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    args = parser.parse_args()
    pins = json.loads(
        (Path(__file__).resolve().parents[1] / "tools/actionlint.json").read_text()
    )
    arch = {"x86_64": "amd64", "aarch64": "arm64", "arm64": "arm64"}.get(
        platform.machine().lower()
    )
    key = f"{platform.system().lower()}_{arch}"
    if key not in pins["sha256"]:
        parser.error(f"unsupported platform: {key}")
    version = pins["version"]
    url = (
        f"https://github.com/rhysd/actionlint/releases/download/v{version}/"
        f"actionlint_{version}_{key}.tar.gz"
    )
    with urllib.request.urlopen(url, timeout=60) as response:
        archive = response.read(16 * 1024 * 1024 + 1)
    if len(archive) > 16 * 1024 * 1024:
        raise ValueError("actionlint archive exceeds 16 MiB")
    if hashlib.sha256(archive).hexdigest() != pins["sha256"][key]:
        raise ValueError("actionlint archive SHA-256 mismatch")
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as bundle:
        member = bundle.getmember("actionlint")
        if not member.isfile() or member.size > 32 * 1024 * 1024:
            raise ValueError("invalid actionlint archive member")
        with bundle.extractfile(member) as source:
            executable = source.read()
    args.bin_dir.mkdir(parents=True, exist_ok=True)
    destination = args.bin_dir / "actionlint"
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=args.bin_dir, delete=False) as output:
            temporary = Path(output.name)
            output.write(executable)
            os.fchmod(output.fileno(), 0o755)
        temporary.replace(destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    print(f"Installed actionlint {version}: {destination}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, tarfile.TarError) as error:
        sys.exit(str(error))
