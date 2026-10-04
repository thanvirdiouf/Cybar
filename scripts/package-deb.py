#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Package an existing Cybar Linux binary without cargo-deb or extra Python deps."""
from __future__ import annotations

import argparse
import gzip
import hashlib
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parent.parent


def run(*command: str, env: dict[str, str] | None = None) -> str:
    try:
        result = subprocess.run(command, check=True, capture_output=True, text=True, env=env)
    except FileNotFoundError as error:
        raise RuntimeError(f"required command is unavailable: {command[0]}") from error
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "").strip()
        raise RuntimeError(f"{' '.join(command)} failed: {detail}") from error
    return result.stdout


def architecture() -> str:
    if shutil.which("dpkg"):
        result = run("dpkg", "--print-architecture").strip()
        if result:
            return result
    machine = platform.machine().lower()
    names = {"x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64",
             "arm64": "arm64", "i386": "i386", "i686": "i386", "armv7l": "armhf",
             "ppc64le": "ppc64el", "riscv64": "riscv64", "s390x": "s390x"}
    if machine not in names:
        raise RuntimeError(f"cannot determine a Debian architecture for {machine!r}")
    return names[machine]


def dependencies(binary: Path) -> str:
    """Read the ELF's required symbol versions instead of guessing the build host's ABI."""
    versions = run("readelf", "--version-info", str(binary))
    required = {tuple(map(int, value.split(".")))
                for value in re.findall(r"\bGLIBC_(\d+\.\d+(?:\.\d+)?)\b", versions)}
    if not required:
        raise RuntimeError("binary declares no GLIBC symbol requirements; this helper targets native Linux/glibc builds")
    minimum = ".".join(map(str, max(required)))
    dynamic = run("readelf", "--dynamic", str(binary))
    libraries = set(re.findall(r"\(NEEDED\).*?\[([^]]+)\]", dynamic))
    supported = {"libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0",
                 "librt.so.1", "libgcc_s.so.1", "ld-linux-x86-64.so.2",
                 "ld-linux-aarch64.so.1", "ld-linux.so.2", "ld-linux-armhf.so.3",
                 "ld-linux-riscv64-lp64d.so.1", "ld64.so.1", "ld64.so.2", "ld.so.1"}
    unexpected = sorted(libraries - supported)
    if unexpected:
        raise RuntimeError("binary links additional libraries needing explicit Debian dependencies: " + ", ".join(unexpected))
    return f"libc6 (>= {minimum}), libgcc-s1"


def copy(source: Path, target: Path, mode: int = 0o644) -> None:
    if not source.is_file():
        raise RuntimeError(f"required package asset is missing: {source}")
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, target)
    target.chmod(mode)


def copy_tree(source: Path, target: Path) -> None:
    if not source.is_dir():
        raise RuntimeError(f"required package directory is missing: {source}")
    for path in sorted(source.rglob("*")):
        if path.is_file():
            copy(path, target / path.relative_to(source))


def one_line(value: object, field: str) -> str:
    text = str(value)
    if "\n" in text or "\r" in text:
        raise RuntimeError(f"{field} must contain one control-file line")
    return text


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def package(runtime: str, binary: Path, output: Path) -> Path:
    with (ROOT / "Cargo.toml").open("rb") as stream:
        manifest = tomllib.load(stream)["package"]
    metadata = dict(manifest.get("metadata", {}).get("deb", {}))
    if runtime == "lua54":
        variant = metadata.get("variants", {}).get("lua54")
        if not variant:
            raise RuntimeError("Cargo.toml does not define the Debian lua54 variant")
        metadata.update(variant)
    name = one_line(metadata.get("name", manifest["name"]), "Package")
    if not re.fullmatch(r"[a-z0-9][a-z0-9+.-]+", name):
        raise RuntimeError(f"invalid Debian package name: {name!r}")
    # Debian's tilde preserves Cargo prerelease ordering before the final version.
    version = one_line(manifest["version"], "Version").replace("-", "~", 1)
    arch = architecture()
    inferred = dependencies(binary)
    depends = one_line(metadata.get("depends", "$auto"), "Depends").replace("$auto", inferred)
    maintainer = one_line(metadata.get("maintainer", "Cybar contributors"), "Maintainer")
    section = one_line(metadata.get("section", "x11"), "Section")
    description = one_line(manifest["description"], "Description")
    try:
        epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    except ValueError as error:
        raise RuntimeError("SOURCE_DATE_EPOCH must be an integer Unix timestamp") from error
    if epoch < 0:
        raise RuntimeError("SOURCE_DATE_EPOCH must be nonnegative")
    output.mkdir(parents=True, exist_ok=True)
    destination = output / f"{name}_{version}_{arch}.deb"
    if destination.exists():
        raise RuntimeError(f"refusing to replace an existing archive: {destination}")
    build_directory = ROOT / ".build"
    build_directory.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="deb-", dir=build_directory) as temporary:
        working = Path(temporary)
        stage = working / "root"
        control_directory = stage / "DEBIAN"
        control_directory.mkdir(parents=True)
        copy(binary, stage / "usr/bin/cybar", 0o755)
        # Keep the same documentation paths as Cargo's declared assets for both runtimes.
        documentation = stage / "usr/share/doc" / manifest["name"]
        for filename in ["LICENSE", "README.md", "BUILD.md"]:
            copy(ROOT / filename, documentation / filename)
        copy_tree(ROOT / "docs", documentation / "docs")
        copy_tree(ROOT / "assets", documentation / "assets")
        for source in sorted((ROOT / "examples").glob("*.lua")):
            copy(source, documentation / "examples" / source.name)
        copy(ROOT / "examples/config.lua", documentation / "config.lua")
        man = stage / "usr/share/man/man1/cybar.1.gz"
        man.parent.mkdir(parents=True)
        man.write_bytes(gzip.compress((ROOT / "docs/cybar.1").read_bytes(), compresslevel=9, mtime=0))
        completion_paths = {
            "bash": "usr/share/bash-completion/completions/cybar",
            "zsh": "usr/share/zsh/vendor-completions/_cybar",
            "fish": "usr/share/fish/vendor_completions.d/cybar.fish",
        }
        for shell, filename in completion_paths.items():
            target = stage / filename
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(run(str(binary), "completions", shell), encoding="utf-8")
            target.chmod(0o644)
        installed = sorted(path for path in stage.rglob("*") if path.is_file() and control_directory not in path.parents)
        size = math.ceil(sum(path.stat().st_size for path in installed) / 1024)
        fields = {"Package": name, "Version": version, "Architecture": arch,
                  "Maintainer": maintainer, "Section": section, "Priority": "optional",
                  "Installed-Size": str(size), "Depends": depends}
        for key in ["provides", "conflicts", "replaces"]:
            if value := metadata.get(key):
                fields[key.title()] = one_line(value, key)
        runtime_label = "LuaJIT" if runtime == "luajit" else "Lua 5.4"
        control = "\n".join(f"{key}: {value}" for key, value in fields.items())
        control += (f"\nDescription: {description}\n"
                    " An X11 dock with Lua configuration, shaped text, custom modules,\n"
                    " EWMH workspaces, multi-monitor support, IPC, and an XEMBED tray.\n"
                    f" This package includes the vendored {runtime_label} runtime.\n")
        (control_directory / "control").write_text(control, encoding="utf-8")
        checksums = []
        for path in installed:
            with path.open("rb") as stream:
                digest = hashlib.file_digest(stream, "md5").hexdigest()
            checksums.append(f"{digest}  {path.relative_to(stage).as_posix()}\n")
        (control_directory / "md5sums").write_text("".join(checksums), encoding="ascii")
        for path in sorted(stage.rglob("*"), reverse=True):
            path.chmod(0o755 if path.is_dir() or path == stage / "usr/bin/cybar" else 0o644)
            os.utime(path, (epoch, epoch))
        stage.chmod(0o755)
        os.utime(stage, (epoch, epoch))
        environment = dict(os.environ, SOURCE_DATE_EPOCH=str(epoch))
        archive = working / destination.name
        run("dpkg-deb", "--root-owner-group", "-Zxz", "-z9", "--build", str(stage), str(archive), env=environment)
        for field in ["Package", "Version", "Architecture", "Depends"]:
            actual = run("dpkg-deb", "--field", str(archive), field).strip()
            if actual != fields[field]:
                raise RuntimeError(f"archive verification failed for {field}: {actual!r}")
        contents = run("dpkg-deb", "--contents", str(archive))
        for filename in ["./usr/bin/cybar", "./usr/share/man/man1/cybar.1.gz", "./usr/share/doc/cybar/LICENSE"]:
            if filename not in contents:
                raise RuntimeError(f"archive is missing {filename}")
        if any(line.split()[1] != "root/root" for line in contents.splitlines() if line.strip()):
            raise RuntimeError("archive ownership verification failed")
        review = working / "review"
        run("dpkg-deb", "--extract", str(archive), str(review))
        if sha256(review / "usr/bin/cybar") != sha256(binary):
            raise RuntimeError("extracted binary does not match the requested input")
        if gzip.decompress((review / "usr/share/man/man1/cybar.1.gz").read_bytes()) != (ROOT / "docs/cybar.1").read_bytes():
            raise RuntimeError("compressed manual failed verification")
        # Never replace user files. Temporary stages and extraction checks remain in .build.
        with destination.open("xb") as target, archive.open("rb") as source:
            shutil.copyfileobj(source, target)
    print(f"Created {destination}")
    print(f"  runtime: {runtime_label}; architecture: {arch}; dependencies: {depends}")
    print(f"  SHA256: {sha256(destination)}")
    return destination


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", required=True, choices=["luajit", "lua54"])
    parser.add_argument("--binary", type=Path, required=True, help="native Linux binary built with the matching Cargo runtime feature")
    parser.add_argument("--out", type=Path, default=Path("dist"), help="output directory (default: dist)")
    args = parser.parse_args()
    try:
        binary = args.binary.expanduser().resolve(strict=True)
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise RuntimeError(f"binary is not an executable regular file: {binary}")
        package(args.runtime, binary, args.out.expanduser().resolve())
    except (OSError, RuntimeError, tomllib.TOMLDecodeError) as error:
        print(f"package-deb: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
