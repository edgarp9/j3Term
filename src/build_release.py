#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import platform
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Sequence


class BuildReleaseError(Exception):
    """User-facing release build failure."""


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
        print_binary_artifacts(binary_dir, target)

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


def print_binary_artifacts(binary_dir: Path, target: str | None) -> None:
    package_name = "j3term"
    executable_name = f"{package_name}.exe" if target_is_windows(target) else package_name
    binary_path = binary_dir / executable_name

    if binary_path.is_file():
        print(f"binary: {binary_path}")
        return

    print("binary: not found at the expected path; check Cargo output for details")


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
