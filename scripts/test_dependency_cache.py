"""Offline behavioral tests for acquiring pinned official dependency backups."""
import copy
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("dependency_cache", Path(__file__).with_name("verify-static-dependencies.py"))
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)


class Response(io.BytesIO):
    def geturl(self):
        return "https://official.example/installer.exe"


class Opener:
    def __init__(self, data):
        self.data = data
        self.calls = 0

    def open(self, request, timeout):
        self.calls += 1
        assert request.full_url == "https://official.example/installer.exe"
        return Response(self.data)


class DependencyCacheTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.data = b"verified installer fixture"
        self.item = {
            "id": "dotnet", "version": "10.0.15",
            "relative_path": "dotnet/10.0.15/windows-x64/runtime.exe",
            "url": "https://official.example/installer.exe", "size": len(self.data),
            "sha256": hashlib.sha256(self.data).hexdigest(),
            "official_sha512": hashlib.sha512(self.data).hexdigest(),
        }

    def test_manifest_validation_requires_no_installer_files(self):
        manifest_path = self.root / "manifest.json"
        manifest_path.write_bytes(cache.MANIFEST.read_bytes())
        manifest = cache.load_manifest(manifest_path)
        self.assertEqual([x["id"] for x in cache.select_dependencies(manifest, "pawnio")], ["pawnio"])
        self.assertEqual(list(self.root.iterdir()), [manifest_path])

    def test_manifest_rejects_unpinned_sources_paths_and_hashes(self):
        original = json.loads(cache.MANIFEST.read_text())
        for key, value in [("url", "https://example.com/latest.exe"),
                           ("relative_path", "../installer.exe"),
                           ("sha256", "0" * 63),
                           ("fallbacks", ["https://github.com/kkltgood/tramoing/releases/latest/download/runtime.exe"]),
                           ("publisher", "Another publisher")]:
            with self.subTest(key=key):
                manifest = copy.deepcopy(original)
                manifest["dependencies"][0][key] = value
                path = self.root / "bad.json"
                path.write_text(json.dumps(manifest))
                with self.assertRaises(ValueError): cache.load_manifest(path)

    def test_download_verified_then_reused_without_network(self):
        opener = Opener(self.data)
        path = cache.acquire(self.item, self.root, opener)
        self.assertEqual(path.read_bytes(), self.data)
        self.assertEqual(cache.acquire(self.item, self.root, opener), path)
        self.assertEqual(opener.calls, 1)
        self.assertEqual(list(path.parent.glob("*.part")), [])

    def test_corrupt_cache_is_repaired_from_official_source(self):
        path = cache.cache_path(self.root, self.item)
        path.parent.mkdir(parents=True)
        path.write_bytes(b"old damaged file")
        cache.acquire(self.item, self.root, Opener(self.data))
        cache.verify_file(path, self.item)

    def test_bad_download_is_not_committed_and_existing_file_is_preserved(self):
        path = cache.cache_path(self.root, self.item)
        path.parent.mkdir(parents=True)
        path.write_bytes(b"old damaged file")
        for data in [self.data[:-1], b"x" * len(self.data), self.data + b"extra"]:
            with self.subTest(data=data):
                with self.assertRaises(ValueError): cache.acquire(self.item, self.root, Opener(data))
                self.assertEqual(path.read_bytes(), b"old damaged file")
                self.assertEqual(list(path.parent.glob("*.part")), [])

    def test_official_sha512_must_also_match(self):
        self.item["official_sha512"] = "0" * 128
        with self.assertRaisesRegex(ValueError, "SHA-512"):
            cache.acquire(self.item, self.root, Opener(self.data))
        self.assertFalse(cache.cache_path(self.root, self.item).exists())

    def test_http_redirect_is_rejected(self):
        with self.assertRaises(ValueError):
            cache.HttpsOnlyRedirect().redirect_request(None, None, 302, "", {}, "http://example.com/file.exe")

    def test_symlink_cache_cannot_escape_selected_directory(self):
        outside = self.root / "outside"
        outside.mkdir()
        directory = self.root / "cache"
        directory.mkdir()
        try:
            (directory / "dotnet").symlink_to(outside, target_is_directory=True)
        except OSError as error:
            self.skipTest(f"Host does not permit symlinks: {error}")
        with self.assertRaises(ValueError): cache.cache_path(directory, self.item)


if __name__ == "__main__":
    unittest.main()
