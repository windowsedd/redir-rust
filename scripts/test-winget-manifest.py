#!/usr/bin/env python3
"""Check the generator against ZIP layout, hashing, and invalid inputs."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import zipfile

script = Path(__file__).with_name("winget-manifest.py")
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    archive = root / "redir-rust-v0.2.0-x86_64-pc-windows-msvc.zip"
    nested = "redir-rust-v0.2.0-x86_64-pc-windows-msvc/redir-rust.exe"
    with zipfile.ZipFile(archive, "w") as bundle:
        bundle.writestr(nested, b"test executable")

    def generate(tag="v0.2.0", path=archive):
        return subprocess.run(
            [sys.executable, str(script), tag, str(path), "--output", str(root / "out")],
            capture_output=True, text=True,
        )

    result = generate()
    assert result.returncode == 0, result.stderr
    manifests = list((root / "out/0.2.0").glob("*.yaml"))
    assert len(manifests) == 3
    installer = json.loads((root / "out/0.2.0/windowsedd.redir-rust.installer.yaml").read_text())
    assert installer["Installers"][0]["InstallerSha256"] == hashlib.sha256(archive.read_bytes()).hexdigest().upper()
    assert installer["Dependencies"]["PackageDependencies"][0]["PackageIdentifier"] == "Microsoft.VCRedist.2015+.x64"
    assert installer["NestedInstallerFiles"][0]["RelativeFilePath"] == nested
    assert installer["Installers"][0]["InstallerUrl"].endswith(f"/v0.2.0/{archive.name}")
    assert generate("v0.2.0-beta").returncode != 0
    assert generate(path=root / "wrong.zip").returncode != 0
    with zipfile.ZipFile(archive, "w") as bundle:
        bundle.writestr("unexpected.exe", b"wrong layout")
    assert generate().returncode != 0
print("WinGet generation checks passed")
