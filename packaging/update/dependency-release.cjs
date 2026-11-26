// Dependency installers are release assets, never Git blobs or main-app assets.
// Only a manually dispatched workflow calls this after Authenticode verification.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const digest = (bytes, algorithm = 'sha256') => crypto.createHash(algorithm).update(bytes).digest('hex');
const known = {
  dotnet: { publisher: 'Microsoft Corporation', host: 'builds.dotnet.microsoft.com' },
  pawnio: { publisher: 'namazso.eu', host: 'github.com' },
};

function officialUrl(value, id, version, name) {
  const url = new URL(value);
  if (url.protocol !== 'https:' || url.hostname !== known[id].host ||
      url.username || url.password || url.search || url.hash) return false;
  if (id === 'dotnet') return url.pathname === `/dotnet/Runtime/${version}/${name}`;
  return url.pathname === `/namazso/PawnIO.Setup/releases/download/${version}/${name}`;
}

function readDependencies({ manifestPath, cacheRoot, dependency = 'all', repo }) {
  if (repo.owner !== 'kkltgood' || repo.repo !== 'tramoing') {
    throw new Error('Dependency backups may only be published to kkltgood/tramoing');
  }
  if (!['all', ...Object.keys(known)].includes(dependency)) throw new Error('Unknown dependency selection');
  const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  if (manifest.schema !== 1 || !Array.isArray(manifest.dependencies) ||
      manifest.dependencies.length !== 2 ||
      new Set(manifest.dependencies.map(item => item.id)).size !== 2 ||
      manifest.dependencies.some(item => !known[item.id])) {
    throw new Error('Invalid dependency manifest');
  }
  const root = fs.realpathSync(cacheRoot);
  return manifest.dependencies.filter(item => dependency === 'all' || item.id === dependency).map(item => {
    if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(item.version) ||
        item.architecture !== 'x64' || item.publisher !== known[item.id].publisher ||
        !Number.isSafeInteger(item.size) || item.size < 1 || item.size > 200_000_000 ||
        !/^[a-f0-9]{64}$/.test(item.sha256) ||
        (item.official_sha512 !== undefined && !/^[a-f0-9]{128}$/.test(item.official_sha512))) {
      throw new Error(`Invalid dependency metadata: ${item.id}`);
    }
    const name = item.id === 'dotnet' ? `dotnet-runtime-${item.version}-win-x64.exe` : 'PawnIO_setup.exe';
    const tag = `deps-${item.id}-${item.version}`;
    const relativePath = `${item.id}/${item.version}/windows-x64/${name}`;
    const fallback = `https://github.com/${repo.owner}/${repo.repo}/releases/download/${tag}/${name}`;
    if (item.relative_path !== relativePath || !officialUrl(item.url, item.id, item.version, name) ||
        !Array.isArray(item.fallbacks) || !item.fallbacks.includes(fallback)) {
      throw new Error(`Dependency source, local path or fixed release URL mismatch: ${item.id}`);
    }
    const filename = fs.realpathSync(path.join(root, ...relativePath.split('/')));
    const relative = path.relative(root, filename);
    if (relative.startsWith(`..${path.sep}`) || relative === '..' || path.isAbsolute(relative)) {
      throw new Error('Dependency cache path escapes its root');
    }
    if (!fs.statSync(filename).isFile()) throw new Error('Dependency cache entry is not a file');
    const bytes = fs.readFileSync(filename);
    if (bytes.length !== item.size || digest(bytes) !== item.sha256 ||
        (item.official_sha512 && digest(bytes, 'sha512') !== item.official_sha512)) {
      throw new Error(`Dependency checksum or size mismatch: ${item.id}`);
    }
    const record = {
      schema: 1, id: item.id, name: item.name, version: item.version,
      architecture: item.architecture, official_url: item.url,
      publisher: item.publisher, size: item.size, sha256: item.sha256,
      ...(item.official_sha512 ? { official_sha512: item.official_sha512 } : {}),
    };
    return {
      id: item.id, name: item.name, version: item.version, tag,
      files: new Map([
        [name, bytes],
        [`${name}.sha256`, Buffer.from(`${item.sha256}  ${name}\n`)],
        ['dependency-manifest.json', Buffer.from(`${JSON.stringify(record, null, 2)}\n`)],
      ]),
    };
  });
}

async function findRelease(github, repo, tag) {
  try {
    return (await github.rest.repos.getReleaseByTag({ ...repo, tag })).data;
  } catch (error) {
    if (error.status !== 404) throw error;
    // Some endpoints hide drafts from getReleaseByTag; list with the write token.
    const releases = await github.paginate(github.rest.repos.listReleases, { ...repo, per_page: 100 });
    return releases.find(release => release.tag_name === tag);
  }
}

async function readAsset(github, repo, asset) {
  const response = await github.rest.repos.getReleaseAsset({
    ...repo, asset_id: asset.id, headers: { accept: 'application/octet-stream' },
  });
  if (!(response.data instanceof ArrayBuffer) && !ArrayBuffer.isView(response.data)) {
    throw new Error(`Release asset did not return binary data: ${asset.name}`);
  }
  return Buffer.from(response.data);
}

async function verifyRelease(github, repo, release, item) {
  for (const [name, expected] of item.files) {
    const assets = release.assets.filter(asset => asset.name === name);
    if (assets.length !== 1) throw new Error(`Dependency release is missing or duplicates an asset: ${name}`);
    const actual = await readAsset(github, repo, assets[0]);
    if (actual.length !== expected.length || digest(actual) !== digest(expected)) {
      throw new Error(`Published dependency asset differs: ${name}; never replace a published version`);
    }
  }
}

async function publishDependencies({
  github, context, core,
  manifestPath = 'static/dependencies/manifest.json',
  cacheRoot,
  dependency = 'all',
}) {
  // Validate every selected file before the first remote mutation.
  const items = readDependencies({ manifestPath, cacheRoot, dependency, repo: context.repo });
  const repo = context.repo;
  for (const item of items) {
    let release = await findRelease(github, repo, item.tag);
    if (release && !release.draft) {
      await verifyRelease(github, repo, release, item);
      core.notice(`Verified existing dependency backup ${item.tag}`);
      continue;
    }
    if (!release) {
      release = (await github.rest.repos.createRelease({
        ...repo, tag_name: item.tag, target_commitish: context.sha,
        name: `${item.name} ${item.version} — dependency backup`,
        draft: true, make_latest: 'false', generate_release_notes: false,
        body: [
          `Backup of the original ${item.name} ${item.version} Windows x64 installer.`,
          'Tramoing tries the pinned official source first and uses this release only as a fallback.',
          'The original publisher signature is preserved. See dependency-manifest.json and the SHA-256 file for the pinned identity.',
          'This is not a Tramoing application update. Published assets are never replaced.',
        ].join('\n\n'),
      })).data;
    }
    const assets = [...release.assets];
    for (const [name, bytes] of item.files) {
      const previous = assets.find(asset => asset.name === name);
      if (previous) {
        const actual = await readAsset(github, repo, previous);
        if (actual.length === bytes.length && digest(actual) === digest(bytes)) continue;
        await github.rest.repos.deleteReleaseAsset({ ...repo, asset_id: previous.id });
        assets.splice(assets.indexOf(previous), 1);
      }
      const uploaded = await github.rest.repos.uploadReleaseAsset({
        ...repo, release_id: release.id, name, data: bytes,
      });
      assets.push(uploaded.data);
    }
    // Read back uploaded bytes before making this backup publicly installable.
    await verifyRelease(github, repo, { ...release, assets }, item);
    const result = await github.rest.repos.updateRelease({
      ...repo, release_id: release.id, draft: false, make_latest: 'false',
    });
    core.notice(`Published dependency backup ${item.tag}`);
    if (core.summary && result.data?.html_url) {
      await core.summary.addLink(item.tag, result.data.html_url).write();
    }
  }
}

module.exports = { readDependencies, publishDependencies };
