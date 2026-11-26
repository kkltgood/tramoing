# Optional dependency downloads

This directory tracks `manifest.json` and documentation only. Installer binaries
are excluded from Git and from the base Tramoing installer. Local copies under
this directory may be retained as ignored cache files; the default download cache
is `target/dependency-cache/`.

The client first reuses compatible installed dependencies, then checks its verified
download cache, then tries the pinned official URL, and finally the versioned
GitHub Release attachment listed in `manifest.json`.

- .NET Runtime 10.0.15, Windows x64: original Microsoft installer. Its SHA-512 was
  checked against Microsoft's `10.0/releases.json`; the manifest also pins size
  and SHA-256. Runtime source and notices: https://github.com/dotnet/runtime.
- PawnIO 2.2.0: original namazso.eu installer. Source and license:
  https://github.com/namazso/PawnIO; official redistributable:
  https://github.com/namazso/PawnIO.Setup/releases/tag/2.2.0.

## Validate or download locally

Run these commands from the repository root:

```sh
# Validate metadata without downloading or requiring installer files.
python3 scripts/verify-static-dependencies.py

# Download only pinned official files and verify size, SHA-256 and available SHA-512.
python3 scripts/verify-static-dependencies.py --download --dependency all

# Recheck files already in the default cache, without downloading.
python3 scripts/verify-static-dependencies.py --verify-cache --dependency all

# Verify original copies retained locally from the earlier layout.
python3 scripts/verify-static-dependencies.py --verify-cache --cache-dir static/dependencies
```

`--dependency` accepts `all`, `dotnet`, or `pawnio`; `--cache-dir` selects a different
cache root while preserving the manifest's relative paths. Hash verification does
not install or execute these files. Windows validation additionally checks the
Microsoft/namazso.eu Authenticode publisher signature.

## Publish a dependency backup

Push the workflow and manifest to the `main` branch of `kkltgood/tramoing`
(the local remote is named `opensource`). In GitHub, open **Actions → Publish
dependency backups → Run workflow** and select `all`, `dotnet`, or `pawnio`.

The Windows job downloads the exact official version, checks its size, hashes and
publisher signature, then uploads the installer, checksum and metadata to a draft
release before publishing it. Dependency releases use tags such as
`deps-dotnet-10.0.15` and `deps-pawnio-2.2.0`; they are explicitly not marked Latest,
so the app's normal update feed continues to refer to a software release.

The workflow runs only when requested. Ordinary code pushes and software releases
do not publish another dependency copy. Repeating it verifies and reuses an
identical published release; different bytes under an already published version
are rejected. Failed draft uploads may be retried. Retain old releases referenced
by supported app versions.

The fallback URLs become usable only after the corresponding dependency releases
are published. Committing a manifest or keeping local installers does not publish
those files. GitHub backups and PawnIO's official release share GitHub hosting, so
this fallback does not address a complete GitHub outage or network block.

To upgrade a dependency, review an exact official version, update its metadata and
fallback URL in the manifest, validate it, then publish the new dependency release.
The application rechecks hashes and publisher signatures before installation, after
copying downloads into a protected staging directory. Never bypass these checks.
