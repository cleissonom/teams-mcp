#!/usr/bin/env python3
"""Create the reviewed source ZIP using Python 3.11+ standard-library tools."""

import hashlib
import re
import sys
import tempfile
import tomllib
import zipfile
from pathlib import Path, PurePosixPath


FORBIDDEN_DIRECTORIES = {
    ".private", ".git", "target", "dist", "build", "cache", ".cache",
    "__pycache__", "node_modules", "private", "profile", "profiles",
    "browser", "browser-profile", "archives", "backups", "default",
    "sessions", "credentials", "secrets", "tokens", "snapshots", "web-assets",
}
FORBIDDEN_NAMES = {
    "credentials.json", "secrets.json", "tokens.json", "auth.json",
    "session-state.json", "token-cache.json", "storage-state.json",
    "cookies", "local state", "login data", "web data", "devtoolsactiveport",
    "singletonlock", "singletoncookie", "singletonsocket",
}
SOURCE_SUFFIXES = {
    ".rs", ".js", ".cjs", ".py", ".md", ".json", ".toml", ".lock",
    ".yml", ".yaml", ".txt",
}
ROOT_FILES = {".gitignore", "LICENSE", "NOTICE"}


def validate_entry(entry):
    parts = entry.split("/")
    if (not entry or entry != entry.strip() or "\\" in entry or ":" in entry
            or any(ord(character) < 32 or ord(character) == 127 for character in entry)
            or any(part in {"", ".", ".."} for part in parts)
            or PurePosixPath(entry).is_absolute()):
        raise ValueError("Manifest paths must be canonical relative POSIX paths.")
    lowered = [part.casefold() for part in parts]
    credential_data = (PurePosixPath(entry).suffix in {".json", ".toml", ".yaml", ".yml", ".txt"}
                       and re.search(r"(?:^|[-_.])(credentials?|secrets?|tokens?|passwords?)(?:$|[-_.])", lowered[-1]))
    if (any(part in FORBIDDEN_DIRECTORIES or part.startswith((".env", "profile ")) for part in lowered)
            or any(part.startswith(".") and part not in {".github", ".gitignore"}
                   for part in lowered)
            or lowered[-1] in FORBIDDEN_NAMES
            or credential_data
            or (entry not in ROOT_FILES and PurePosixPath(entry).suffix not in SOURCE_SUFFIXES)):
        raise ValueError("Manifest contains a forbidden private, credential, or generated artifact.")
    return PurePosixPath(entry)


def checked_source(root, entry):
    relative = validate_entry(entry)
    path = root
    for part in relative.parts:
        path = path / part
        if path.is_symlink():
            raise ValueError("Source files and parent directories must not be symlinks.")
    if not path.is_file():
        raise ValueError("A manifest entry is missing or is not a regular source file.")
    return path


def manifest_files(root):
    manifest = checked_source(root, "release-files.txt")
    entries = []
    seen = set()
    for line in manifest.read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        validate_entry(line)
        key = line.casefold()
        if key in seen:
            raise ValueError("Manifest contains duplicate or case-colliding entries.")
        seen.add(key)
        entries.append(line)
    if not entries:
        raise ValueError("Manifest must contain at least one reviewed source file.")
    return [(entry, checked_source(root, entry)) for entry in entries]


def create_archive(root):
    root = Path(root).absolute()
    if root.is_symlink() or not root.is_dir():
        raise ValueError("The source root must be a regular directory, not a symlink.")
    sources = manifest_files(root)
    cargo = tomllib.loads(checked_source(root, "Cargo.toml").read_text(encoding="utf-8"))
    version = cargo.get("package", {}).get("version", "")
    if (cargo.get("package", {}).get("name") != "teams-mcp"
            or not isinstance(version, str)
            or not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", version)):
        raise ValueError("Cargo.toml must declare the teams-mcp package and a valid release version.")
    directory = root / "dist"
    destination = directory / f"teams-mcp-{version}-source.zip"
    if directory.is_symlink() or destination.is_symlink():
        raise ValueError("The archive destination must not contain symlinks.")
    directory.mkdir(mode=0o700, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=directory, prefix=".source-", suffix=".zip", delete=False) as output:
            temporary = Path(output.name)
        with zipfile.ZipFile(temporary, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for entry, source in sources:
                info = zipfile.ZipInfo(f"teams-mcp/{entry}", date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                archive.writestr(info, source.read_bytes())
        temporary.replace(destination)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return destination


if __name__ == "__main__":
    if len(sys.argv) != 1:
        raise SystemExit("Usage: python3 scripts/package-release.py")
    try:
        result = create_archive(Path(__file__).resolve().parents[1])
    except (ValueError, OSError) as error:
        raise SystemExit(f"Source package rejected: {error}") from None
    digest = hashlib.sha256(result.read_bytes()).hexdigest()
    print(f"Created {result}\nSHA-256: {digest}")
