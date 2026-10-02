#!/usr/bin/env python3
"""Generate WinGet manifests from the actual release ZIP (no dependencies)."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import zipfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="Stable release tag, e.g. v0.2.0")
    parser.add_argument("archive", type=Path)
    parser.add_argument("--output", type=Path, default=Path("dist/winget"))
    args = parser.parse_args()
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", args.tag):
        parser.error("WinGet publishing requires a stable vMAJOR.MINOR.PATCH tag")
    name = f"redir-rust-{args.tag}-x86_64-pc-windows-msvc"
    if args.archive.name != f"{name}.zip":
        parser.error(f"expected {name}.zip")
    nested = f"{name}/redir-rust.exe"
    with zipfile.ZipFile(args.archive) as archive:
        if nested not in archive.namelist():
            parser.error(f"archive is missing {nested}")
    digest = hashlib.sha256(args.archive.read_bytes()).hexdigest().upper()
    version = args.tag[1:]
    package = "windowsedd.redir-rust"
    base = "https://github.com/windowsedd/redir-rust"
    common = {"PackageIdentifier": package, "PackageVersion": version}
    manifests = {
        "": {"DefaultLocale": "en-US", "ManifestType": "version"},
        ".installer": {
            "InstallerType": "zip",
            "Dependencies": {"PackageDependencies": [{"PackageIdentifier": "Microsoft.VCRedist.2015+.x64"}]},
            "NestedInstallerType": "portable",
            "NestedInstallerFiles": [{"RelativeFilePath": nested, "PortableCommandAlias": "redir-rust"}],
            "UpgradeBehavior": "uninstallPrevious",
            "Installers": [{"Architecture": "x64", "InstallerUrl": f"{base}/releases/download/{args.tag}/{name}.zip", "InstallerSha256": digest}],
            "ManifestType": "installer",
        },
        ".locale.en-US": {
            "PackageLocale": "en-US", "Publisher": "windowsedd",
            "PublisherUrl": "https://github.com/windowsedd",
            "PackageName": "redir-rust", "PackageUrl": base,
            "License": "MIT", "LicenseUrl": f"{base}/blob/{args.tag}/LICENSE",
            "ShortDescription": "TCP/UDP port redirector with failover and Minecraft offline responses",
            "Moniker": "redir-rust", "Tags": ["tcp", "udp", "proxy", "minecraft"],
            "ReleaseNotesUrl": f"{base}/releases/tag/{args.tag}",
            "ManifestType": "defaultLocale",
        },
    }
    output = args.output / version
    output.mkdir(parents=True, exist_ok=True)
    for suffix, manifest in manifests.items():
        document = {**common, **manifest, "ManifestVersion": "1.10.0"}
        # JSON is a YAML subset, accepted by WinGet's YAML parser.
        (output / f"{package}{suffix}.yaml").write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    print(output)


if __name__ == "__main__":
    main()
