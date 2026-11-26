"""Validate pinned dependency metadata; optionally acquire or verify a local cache.

Installers are downloaded from their pinned official URLs and never tracked by
Git. Release publishing separately verifies Authenticode signatures on Windows.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import tempfile
import urllib.request
from urllib.parse import urlparse

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "static" / "dependencies" / "manifest.json"
CACHE = ROOT / "target" / "dependency-cache"
REPOSITORY = "https://github.com/kkltgood/tramoing"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def https_url(value):
    parsed = urlparse(value)
    return parsed.scheme == "https" and parsed.hostname and not parsed.username and not parsed.password and not parsed.fragment


def load_manifest(path=MANIFEST):
    manifest = json.loads(Path(path).read_text(encoding="utf-8"))
    require(manifest.get("schema") == 1, "Unsupported dependency manifest schema")
    items = manifest.get("dependencies", [])
    require(len(items) == 2 and {item.get("id") for item in items} == {"dotnet", "pawnio"}, "Expected exactly dotnet and pawnio dependencies")
    for item in items:
        identity = item["id"]
        version = item.get("version", "")
        require(re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version), f"Invalid {identity} version")
        require(re.fullmatch(r"\d+\.\d+\.\d+", item.get("minimum_version", "")), "Invalid minimum version")
        require(item.get("architecture") == "x64", "Only x64 dependency packages are supported")
        relative = item.get("relative_path", "")
        require(isinstance(relative, str) and "\\" not in relative and ":" not in relative, "Invalid dependency cache path")
        parts = relative.split("/")
        require(len(parts) == 4 and parts[:3] == [identity, version, "windows-x64"] and all(part and part not in (".", "..") for part in parts), "Dependency cache path must match its version and architecture")
        filename = parts[-1]
        expected_name = f"dotnet-runtime-{version}-win-x64.exe" if identity == "dotnet" else "PawnIO_setup.exe"
        require(filename == expected_name, "Unexpected dependency filename")
        official = (
            f"https://builds.dotnet.microsoft.com/dotnet/Runtime/{version}/{filename}"
            if identity == "dotnet" else
            f"https://github.com/namazso/PawnIO.Setup/releases/download/{version}/{filename}"
        )
        require(item.get("url") == official, "Dependency URL must pin its official release")
        expected_publisher = "Microsoft Corporation" if identity == "dotnet" else "namazso.eu"
        require(item.get("publisher") == expected_publisher, "Unexpected publisher")
        require(type(item.get("size")) is int and 0 < item["size"] <= 200_000_000, "Invalid dependency size")
        require(re.fullmatch(r"[0-9a-f]{64}", item.get("sha256", "")), "Invalid SHA-256")
        if identity == "dotnet":
            require(re.fullmatch(r"[0-9a-f]{128}", item.get("official_sha512", "")), "Missing official Microsoft SHA-512")
        fallback = f"{REPOSITORY}/releases/download/deps-{identity}-{version}/{filename}"
        require(item.get("fallbacks") == [fallback] and https_url(fallback), "Fallback must identify the immutable dependency release")
    return manifest


def select_dependencies(manifest, dependency):
    require(dependency in ("all", "dotnet", "pawnio"), "Unknown dependency selection")
    return [item for item in manifest["dependencies"] if dependency == "all" or item["id"] == dependency]


def cache_path(root, item):
    root = Path(root).resolve()
    path = root.joinpath(*PurePosixPath(item["relative_path"]).parts)
    require(path.resolve().is_relative_to(root), "Dependency path escapes cache directory")
    return path


def verify_file(path, item):
    path = Path(path)
    require(path.is_file() and not path.is_symlink(), f"Dependency cache missing or invalid: {path}")
    require(path.stat().st_size == item["size"], f"Size mismatch: {path}")
    sha256, sha512 = hashlib.sha256(), hashlib.sha512()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            sha256.update(chunk)
            sha512.update(chunk)
    require(sha256.hexdigest() == item["sha256"], f"SHA-256 mismatch: {path}")
    if item.get("official_sha512"):
        require(sha512.hexdigest() == item["official_sha512"], f"Official SHA-512 mismatch: {path}")


class HttpsOnlyRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        require(https_url(newurl), "Dependency download redirected outside HTTPS")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def acquire(item, root, opener=None):
    """Reuse verified bytes or atomically replace them with a verified official download."""
    destination = cache_path(root, item)
    if destination.exists():
        try:
            verify_file(destination, item)
            return destination
        except ValueError:
            pass
    destination.parent.mkdir(parents=True, exist_ok=True)
    require(https_url(item["url"]), "Dependency downloads require HTTPS")
    client = opener or urllib.request.build_opener(HttpsOnlyRedirect())
    request = urllib.request.Request(item["url"], headers={"User-Agent": "Tramoing-dependency-publisher/1"})
    temporary = None
    try:
        with client.open(request, timeout=60) as response:
            require(https_url(response.geturl()), "Dependency response must use HTTPS")
            with tempfile.NamedTemporaryFile(dir=destination.parent, prefix=".download-", suffix=".part", delete=False) as output:
                temporary = Path(output.name)
                received = 0
                while chunk := response.read(1024 * 1024):
                    received += len(chunk)
                    require(received <= item["size"], "Dependency download exceeds pinned size")
                    output.write(chunk)
                output.flush()
                os.fsync(output.fileno())
        verify_file(temporary, item)
        os.replace(temporary, destination)
        return destination
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--dependency", choices=("all", "dotnet", "pawnio"), default="all")
    parser.add_argument("--cache-dir", type=Path, default=CACHE)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--download", action="store_true", help="Fetch missing/corrupt files from pinned official URLs")
    mode.add_argument("--verify-cache", action="store_true", help="Verify existing files without network access")
    arguments = parser.parse_args()
    manifest = load_manifest(arguments.manifest)
    for item in select_dependencies(manifest, arguments.dependency):
        if arguments.download:
            path = acquire(item, arguments.cache_dir)
        elif arguments.verify_cache:
            path = cache_path(arguments.cache_dir, item)
            verify_file(path, item)
        else:
            print(f"Validated metadata: {item['id']} {item['version']}")
            continue
        print(f"Verified {item['id']} {item['version']} ({item['size']:,} bytes): {path}")


if __name__ == "__main__":
    main()
