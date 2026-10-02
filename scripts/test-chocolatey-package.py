#!/usr/bin/env python3
"""Verify embedded content, metadata, hashes, and rejection of invalid releases."""
import hashlib
from pathlib import Path
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
import zipfile

script = Path(__file__).with_name("chocolatey-package.py")
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    archive = root / "redir-rust-v0.2.0-x86_64-pc-windows-msvc.zip"
    name = archive.stem
    executable = b"MZtest executable"

    def bundle(binary=executable, license=True):
        with zipfile.ZipFile(archive, "w") as output:
            output.writestr(f"{name}/redir-rust.exe", binary)
            if license:
                output.writestr(f"{name}/LICENSE", b"MIT license fixture")

    def generate(tag="v0.2.0", path=archive):
        return subprocess.run(
            [sys.executable, str(script), tag, str(path), "--output", str(root / "out")],
            capture_output=True, text=True,
        )

    bundle()
    result = generate()
    assert result.returncode == 0, result.stderr
    package = root / "out/0.2.0"
    assert (package / "tools/redir-rust.exe").read_bytes() == executable
    assert (package / "tools/LICENSE.txt").read_bytes() == b"MIT license fixture"
    verification = (package / "tools/VERIFICATION.txt").read_text()
    assert hashlib.sha256(archive.read_bytes()).hexdigest() in verification
    assert hashlib.sha256(executable).hexdigest() in verification
    assert f"/v0.2.0/{archive.name}" in verification
    nuspec = (package / "redir-rust.nuspec").read_text()
    assert "@VERSION@" not in nuspec and "@TAG@" not in nuspec
    namespace = {"n": "http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd"}
    metadata = ET.fromstring(nuspec).find("n:metadata", namespace)
    assert metadata.find("n:id", namespace).text == "redir-rust"
    assert metadata.find("n:version", namespace).text == "0.2.0"
    assert metadata.find("n:dependencies/n:dependency", namespace).get("id") == "vcredist140"
    assert generate("v0.2.0-beta").returncode != 0
    assert generate(path=root / "wrong.zip").returncode != 0
    bundle(license=False)
    assert generate().returncode != 0
    bundle(binary=b"not a PE executable")
    assert generate().returncode != 0
print("Chocolatey package generation checks passed")
