"""Validate release metadata and extract the dated changelog section."""
import datetime
import pathlib
import re
import sys
import tomllib


def release_notes(root: pathlib.Path, version: str) -> str:
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("Use a stable version such as 0.3.0, without v")
    package = tomllib.loads((root / "Cargo.toml").read_text())["package"]
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    locked = [p for p in lock["package"] if p["name"] == package["name"] and "source" not in p]
    if package["version"] != version or len(locked) != 1 or locked[0]["version"] != version:
        raise ValueError("Cargo.toml and Cargo.lock must match the release version")
    sections = re.split(r"^## ", (root / "CHANGELOG.md").read_text(), flags=re.MULTILINE)[1:]
    matches = []
    for section in sections:
        heading, _, body = section.partition("\n")
        match = re.fullmatch(re.escape(version) + r" - (\d{4}-\d{2}-\d{2})", heading)
        if match:
            datetime.date.fromisoformat(match[1])
            matches.append(body.strip())
    if len(matches) != 1 or not matches[0]:
        raise ValueError("CHANGELOG.md needs one nonempty dated section for this version")
    return matches[0] + "\n"


if __name__ == "__main__":
    pathlib.Path(sys.argv[2]).write_text(release_notes(pathlib.Path.cwd(), sys.argv[1]))
