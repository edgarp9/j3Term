#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import platform
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path
from typing import Sequence


class BuildReleaseError(Exception):
    """User-facing release build failure."""


PACKAGE_NAME = "j3term"
RELEASE_NOTICE_FILES = ("LICENSE", "NOTICE", "THIRD_PARTY_NOTICES.txt", "about.txt")
RELEASE_NOTICE_DIRECTORIES = ("LICENSES",)
OPTIONAL_BINARY_FILES = ("icon.svg", "icon.png")
SOURCE_EXCLUDED_DIRECTORIES = {
    ".git",
    ".my",
    ".idea",
    ".vscode",
    "target",
    "dist",
    "coverage",
    "criterion",
    "codex-target",
    ".codex-candidate2-build",
    ".codex-cargo-candidate4",
    ".codex-cargo-check",
    ".codex-target",
}
SOURCE_EXCLUDED_FILE_NAMES = {
    "tarpaulin-report.html",
    "cargo-tarpaulin-report.xml",
    "flamegraph.svg",
}
SOURCE_EXCLUDED_SUFFIXES = (
    ".rlib",
    ".rmeta",
    ".profraw",
    ".profdata",
    ".pdb",
    ".ilk",
    ".log",
    ".tmp",
    ".bak",
    ".swp",
    ".swo",
)


def main(argv: Sequence[str]) -> int:
    project_root = Path(__file__).resolve().parent
    args = parse_args(argv)

    try:
        ensure_project_root(project_root)
        cargo = require_executable("cargo")
        target = args.target or default_target_for_host()
        target_dir = cargo_target_dir(cargo, project_root)

        command = [cargo, "build", "--release"]
        if target is not None:
            command.extend(["--target", target])

        print(f"project: {project_root}")
        print_command(command)
        run(command, project_root)

        binary_dir = release_binary_dir(target_dir, target)
        print(f"binary folder: {binary_dir}")
        binary_path = release_binary_path(binary_dir, target)
        print_binary_artifact(binary_path)
        release_artifacts = package_release_artifacts(project_root, binary_path, target)
        print_release_artifacts(release_artifacts)

        if not args.no_open:
            open_binary_folder(binary_dir)

        return 0
    except BuildReleaseError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build the j3Term release binary and open the binary folder."
    )
    parser.add_argument(
        "--target",
        help="Cargo target triple. Omit it to build for the current host.",
    )
    parser.add_argument(
        "--no-open",
        action="store_true",
        help="Build only; do not open the binary folder.",
    )
    return parser.parse_args(argv)


def ensure_project_root(project_root: Path) -> None:
    cargo_toml = project_root / "Cargo.toml"
    if not cargo_toml.is_file():
        raise BuildReleaseError(f"Cargo.toml was not found at {cargo_toml}")


def require_executable(name: str) -> str:
    executable = shutil.which(name)
    if executable is None:
        raise BuildReleaseError(f"'{name}' was not found in PATH")
    return executable


def default_target_for_host() -> str | None:
    return None


def cargo_target_dir(cargo: str, project_root: Path) -> Path:
    command = [cargo, "metadata", "--no-deps", "--format-version", "1"]
    result = subprocess.run(
        command,
        cwd=project_root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        raise BuildReleaseError(
            "failed to resolve Cargo target directory\n"
            f"command: {format_command(command)}\n"
            f"{result.stderr.strip()}"
        )

    try:
        metadata = json.loads(result.stdout)
        target_directory = metadata["target_directory"]
    except (KeyError, json.JSONDecodeError, TypeError) as error:
        raise BuildReleaseError(f"failed to parse cargo metadata: {error}") from error

    return Path(target_directory)


def release_binary_dir(target_dir: Path, target: str | None) -> Path:
    if target is None:
        return target_dir / "release"
    return target_dir / target / "release"


def run(command: Sequence[str], cwd: Path) -> None:
    result = subprocess.run(command, cwd=cwd)
    if result.returncode != 0:
        raise BuildReleaseError(f"release build failed with exit code {result.returncode}")


def print_command(command: Sequence[str]) -> None:
    print(f"command: {format_command(command)}")


def format_command(command: Sequence[str]) -> str:
    return " ".join(quote_argument(argument) for argument in command)


def quote_argument(argument: str) -> str:
    if not argument or any(character.isspace() for character in argument):
        return f'"{argument}"'
    return argument


def release_binary_path(binary_dir: Path, target: str | None) -> Path:
    executable_name = f"{PACKAGE_NAME}.exe" if target_is_windows(target) else PACKAGE_NAME
    binary_path = binary_dir / executable_name
    if not binary_path.is_file():
        raise BuildReleaseError(f"release binary was not found at {binary_path}")
    return binary_path


def print_binary_artifact(binary_path: Path) -> None:
    print(f"binary: {binary_path}")


def package_release_artifacts(
    project_root: Path,
    binary_path: Path,
    target: str | None,
) -> list[Path]:
    version = cargo_package_version(project_root)
    release_target = target or default_package_target_for_host()
    artifact_dir = binary_path.parent
    package_prefix = f"{PACKAGE_NAME}-{version}"

    source_files = source_distribution_files(project_root)
    source_zip = artifact_dir / f"{package_prefix}-source.zip"
    source_tar_gz = artifact_dir / f"{package_prefix}-source.tar.gz"
    binary_zip = artifact_dir / f"{package_prefix}-{release_target}-binary.zip"

    create_source_zip(project_root, source_files, package_prefix, source_zip)
    create_source_tar_gz(project_root, source_files, package_prefix, source_tar_gz)
    create_binary_zip(project_root, binary_path, package_prefix, binary_zip)
    return [source_zip, source_tar_gz, binary_zip]


def cargo_package_version(project_root: Path) -> str:
    cargo_toml = project_root / "Cargo.toml"
    in_package = False
    for line in cargo_toml.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped == "[package]":
            in_package = True
            continue
        if stripped.startswith("[") and stripped.endswith("]"):
            in_package = False
        if in_package and stripped.startswith("version"):
            _key, separator, value = stripped.partition("=")
            if separator:
                version = value.strip().strip('"')
                if version:
                    return version
    raise BuildReleaseError(f"package version was not found in {cargo_toml}")


def default_package_target_for_host() -> str:
    host = platform.system()
    machine = platform.machine().lower()
    arch = "x86_64" if machine in {"amd64", "x86_64"} else machine or "unknown"
    if host == "Windows":
        return f"{arch}-pc-windows-msvc"
    if host == "Linux":
        return f"{arch}-unknown-linux-gnu"
    if host == "Darwin":
        return f"{arch}-apple-darwin"
    return f"{arch}-{host.lower()}"


def source_distribution_files(project_root: Path) -> list[Path]:
    files = git_tracked_files(project_root)
    if files is None:
        files = recursively_discovered_source_files(project_root)

    required_files = [Path(file_name) for file_name in RELEASE_NOTICE_FILES]
    for directory in RELEASE_NOTICE_DIRECTORIES:
        required_files.extend(relative_files_under(project_root, Path(directory)))

    unique_files = {path for path in files if source_file_allowed(path)}
    unique_files.update(path for path in required_files if (project_root / path).is_file())
    return sorted(unique_files, key=lambda path: path.as_posix())


def git_tracked_files(project_root: Path) -> list[Path] | None:
    git = shutil.which("git")
    if git is None:
        return None

    result = subprocess.run(
        [git, "ls-files", "--cached", "--others", "--exclude-standard"],
        cwd=project_root,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    if result.returncode != 0:
        return None

    return [Path(line) for line in result.stdout.splitlines() if line.strip()]


def recursively_discovered_source_files(project_root: Path) -> list[Path]:
    files: list[Path] = []
    for path in project_root.rglob("*"):
        if not path.is_file():
            continue
        relative = path.relative_to(project_root)
        if source_file_allowed(relative):
            files.append(relative)
    return files


def relative_files_under(project_root: Path, relative_directory: Path) -> list[Path]:
    root = project_root / relative_directory
    if not root.is_dir():
        return []
    return [
        path.relative_to(project_root)
        for path in root.rglob("*")
        if path.is_file() and source_file_allowed(path.relative_to(project_root))
    ]


def source_file_allowed(relative_path: Path) -> bool:
    parts = relative_path.parts
    if any(part in SOURCE_EXCLUDED_DIRECTORIES for part in parts):
        return False
    name = relative_path.name
    if name in SOURCE_EXCLUDED_FILE_NAMES:
        return False
    if name in {".DS_Store", "Thumbs.db", "Desktop.ini"}:
        return False
    if name.endswith("~"):
        return False
    return not name.endswith(SOURCE_EXCLUDED_SUFFIXES)


def create_source_zip(
    project_root: Path,
    source_files: Sequence[Path],
    package_prefix: str,
    destination: Path,
) -> None:
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for relative_path in source_files:
            archive.write(
                project_root / relative_path,
                arcname=archive_name(package_prefix, relative_path),
            )


def create_source_tar_gz(
    project_root: Path,
    source_files: Sequence[Path],
    package_prefix: str,
    destination: Path,
) -> None:
    with tarfile.open(destination, "w:gz") as archive:
        for relative_path in source_files:
            archive.add(
                project_root / relative_path,
                arcname=archive_name(package_prefix, relative_path),
            )


def create_binary_zip(
    project_root: Path,
    binary_path: Path,
    package_prefix: str,
    destination: Path,
) -> None:
    with zipfile.ZipFile(destination, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.write(binary_path, archive_name(package_prefix, Path(binary_path.name)))
        for relative_path in binary_distribution_files(project_root):
            archive.write(
                project_root / relative_path,
                arcname=archive_name(package_prefix, relative_path),
            )


def binary_distribution_files(project_root: Path) -> list[Path]:
    files = [Path(file_name) for file_name in RELEASE_NOTICE_FILES]
    files.extend(Path(file_name) for file_name in OPTIONAL_BINARY_FILES)
    for directory in RELEASE_NOTICE_DIRECTORIES:
        files.extend(relative_files_under(project_root, Path(directory)))

    unique_files = {path for path in files if (project_root / path).is_file()}
    return sorted(unique_files, key=lambda path: path.as_posix())


def archive_name(package_prefix: str, relative_path: Path) -> str:
    return f"{package_prefix}/{relative_path.as_posix()}"


def print_release_artifacts(paths: Sequence[Path]) -> None:
    for path in paths:
        print(f"release artifact: {path}")


def target_is_windows(target: str | None) -> bool:
    if target is not None:
        return "windows" in target
    return platform.system() == "Windows"


def open_binary_folder(binary_dir: Path) -> None:
    if not binary_dir.is_dir():
        raise BuildReleaseError(f"binary folder does not exist: {binary_dir}")

    opener = folder_opener()
    if opener is None:
        print(f"open skipped: no supported file manager opener was found for {binary_dir}")
        return

    subprocess.Popen(
        [*opener, str(binary_dir)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def folder_opener() -> list[str] | None:
    host = platform.system()
    if host == "Windows":
        return ["explorer"]
    if host == "Linux":
        if shutil.which("xdg-open") is not None:
            return ["xdg-open"]
        if shutil.which("gio") is not None:
            return ["gio", "open"]
        return None
    if host == "Darwin" and shutil.which("open") is not None:
        return ["open"]
    return None


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
