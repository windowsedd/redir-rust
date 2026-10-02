#!/usr/bin/env python3
"""Prepare a self-contained Chocolatey package from a stable Windows release ZIP."""
import argparse
import hashlib
from pathlib import Path
import re
import shutil
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("archive", type=Path)
    parser.add_argument("--output", type=Path, default=Path("dist/chocolatey"))
    args = parser.parse_args()
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", args.tag):
        parser.error("Chocolatey publishing requires a stable vMAJOR.MINOR.PATCH tag")
    name = f"redir-rust-{args.tag}-x86_64-pc-windows-msvc"
    if args.archive.name != f"{name}.zip":
        parser.error(f"expected {name}.zip")
    with zipfile.ZipFile(args.archive) as archive:
        try:
            executable = archive.read(f"{name}/redir-rust.exe")
            license_text = archive.read(f"{name}/LICENSE")
        except KeyError as error:
            parser.error(f"release archive is missing {error}")
    if not executable.startswith(b"MZ"):
        parser.error("release executable is not a Windows PE binary")
    version = args.tag[1:]
    template = Path(__file__).resolve().parents[1] / "packaging/chocolatey"
    output = args.output / version
    tools = output / "tools"
    tools.mkdir(parents=True, exist_ok=True)
    nuspec = (template / "redir-rust.nuspec").read_text(encoding="utf-8")
    (output / "redir-rust.nuspec").write_text(nuspec.replace("@VERSION@", version).replace("@TAG@", args.tag), encoding="utf-8")
    shutil.copyfile(template / "tools/chocolateyInstall.ps1", tools / "chocolateyInstall.ps1")
    (tools / "redir-rust.exe").write_bytes(executable)
    (tools / "LICENSE.txt").write_bytes(license_text)
    url = f"https://github.com/windowsedd/redir-rust/releases/download/{args.tag}/{name}.zip"
    verification = (
        "The maintainer packages the official MIT-licensed release binary.\n"
        f"Download: {url}\n"
        f"ZIP SHA256: {hashlib.sha256(args.archive.read_bytes()).hexdigest()}\n"
        f"Extract: {name}/redir-rust.exe\n"
        f"Executable SHA256: {hashlib.sha256(executable).hexdigest()}\n"
        "To verify, download the ZIP, compare its SHA256, extract the executable,\n"
        "and compare its SHA256 with the embedded tools/redir-rust.exe.\n"
    )
    (tools / "VERIFICATION.txt").write_text(verification, encoding="utf-8")
    print(output)


if __name__ == "__main__":
    main()
