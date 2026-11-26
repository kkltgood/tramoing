// Optional components have their own immutable release. A main-app update must
// never change bytes at a component URL already embedded in older executables.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const digest = bytes => crypto.createHash('sha256').update(bytes).digest('hex');

function readComponent(directory, { owner, repo }) {
  const manifestName = 'sensors-component.json';
  const manifestBytes = fs.readFileSync(path.join(directory, manifestName));
  const manifest = JSON.parse(manifestBytes);
  const version = manifest.component_version;
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version) ||
      manifest.protocol !== 1 || manifest.architecture !== 'x64' ||
      !/^[a-f0-9]{64}$/.test(manifest.sha256) || !/^[a-f0-9]{64}$/.test(manifest.source_sha256) ||
      !Number.isSafeInteger(manifest.size) || manifest.size <= 0) {
    throw new Error('Invalid sensor component manifest');
  }
  const tag = `sensors-v${version}`;
  const name = `Tramoing-Sensors-${version}-windows-x64-setup.exe`;
  if (manifest.url !== `https://github.com/${owner}/${repo}/releases/download/${tag}/${name}`) {
    throw new Error('Sensor component URL must identify its immutable version release');
  }
  const binary = fs.readFileSync(path.join(directory, name));
  const checksum = fs.readFileSync(path.join(directory, `${name}.sha256`));
  if (binary.length !== manifest.size || digest(binary) !== manifest.sha256 ||
      checksum.toString('utf8').trim() !== `${manifest.sha256}  ${name}`) {
    throw new Error('Sensor component checksum or size mismatch');
  }
  return { version, tag, manifest, files: new Map([
    [name, binary], [`${name}.sha256`, checksum], [manifestName, manifestBytes],
  ]) };
}

async function findRelease(github, repo, tag) {
  try {
    return (await github.rest.repos.getReleaseByTag({ ...repo, tag })).data;
  } catch (error) {
    if (error.status !== 404) throw error;
    const releases = await github.paginate(github.rest.repos.listReleases, { ...repo, per_page: 100 });
    return releases.find(release => release.tag_name === tag);
  }
}

async function publishComponent({ github, context, core, component }) {
  const repo = context.repo;
  let release = await findRelease(github, repo, component.tag);
  if (release && !release.draft) {
    // Verify existing bytes instead of silently accepting or replacing a release.
    for (const [name, expected] of component.files) {
      const asset = release.assets.find(item => item.name === name);
      if (!asset) throw new Error(`Published sensor component is incomplete: ${name}`);
      const response = await github.rest.repos.getReleaseAsset({
        ...repo, asset_id: asset.id, headers: { accept: 'application/octet-stream' },
      });
      const actual = Buffer.from(response.data);
      if (name === 'sensors-component.json') {
        const existing = JSON.parse(actual.toString('utf8'));
        for (const key of Object.keys(component.manifest)) {
          if (existing[key] !== component.manifest[key]) {
            throw new Error(`Published sensor component differs: ${key}; increment its version`);
          }
        }
      } else if (digest(actual) !== digest(expected)) {
        throw new Error(`Published sensor component differs: ${name}; increment its version`);
      }
    }
    core.notice(`Reused sensor component ${component.version}`);
    return;
  }
  if (!release) {
    release = (await github.rest.repos.createRelease({
      ...repo, tag_name: component.tag, target_commitish: process.env.RELEASE_COMMIT || context.sha,
      name: `Tramoing temperature component ${component.version}`, draft: true,
      make_latest: 'false', generate_release_notes: false,
      body: 'Optional Windows x64 temperature component. Install from Tramoing settings. .NET Runtime and PawnIO are detected and downloaded separately from their official sources when required.',
    })).data;
  }
  for (const [name, data] of component.files) {
    const previous = release.assets.find(asset => asset.name === name);
    if (previous) await github.rest.repos.deleteReleaseAsset({ ...repo, asset_id: previous.id });
    await github.rest.repos.uploadReleaseAsset({ ...repo, release_id: release.id, name, data });
  }
  await github.rest.repos.updateRelease({ ...repo, release_id: release.id, draft: false, make_latest: 'false' });
  core.notice(`Published sensor component ${component.version}`);
}

module.exports = { readComponent, publishComponent };
