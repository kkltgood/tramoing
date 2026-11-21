// Publish only after every package and checksum is verified. Keep the release
// private as a draft until latest.json and all installers have been uploaded.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

module.exports = async function publishRelease({
  github, context, core,
  directory = 'release-files',
  version = process.env.RELEASE_VERSION,
  tag = process.env.RELEASE_TAG,
}) {
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version) || tag !== `v${version}`) {
    throw new Error('Release tag must match the stable package version.');
  }
  const packages = [`Tramoing-${version}-setup.exe`, `Tramoing-${version}.dmg`];
  const names = packages.flatMap(name => [name, `${name}.sha256`]).sort();
  const actual = fs.readdirSync(directory).sort();
  if (JSON.stringify(actual) !== JSON.stringify(names)) {
    throw new Error(`Unexpected release assets: ${actual.join(', ')}`);
  }

  const { owner, repo } = context.repo;
  const baseUrl = `https://github.com/${owner}/${repo}/releases/download/${tag}`;
  const assets = packages.map(name => {
    const file = fs.readFileSync(path.join(directory, name));
    if (!file.length) throw new Error(`Empty package: ${name}`);
    const checksum = fs.readFileSync(path.join(directory, `${name}.sha256`), 'utf8').trim();
    const digest = crypto.createHash('sha256').update(file).digest('hex');
    if (checksum !== `${digest}  ${name}`) throw new Error(`Checksum mismatch: ${name}`);
    return { url: `${baseUrl}/${name}`, sha256: digest, size: file.length };
  });

  let release;
  try {
    release = (await github.rest.repos.getReleaseByTag({ owner, repo, tag })).data;
  } catch (error) {
    if (error.status !== 404) throw error;
    // The tag endpoint can omit drafts left by an interrupted upload.
    const releases = await github.paginate(github.rest.repos.listReleases, {
      owner, repo, per_page: 100,
    });
    release = releases.find(item => item.tag_name === tag);
  }
  if (release && !release.draft) {
    throw new Error(`${tag} is already published. Use a new version instead of replacing its files.`);
  }
  if (!release) {
    release = (await github.rest.repos.createRelease({
      owner, repo, tag_name: tag, name: `Tramoing ${tag}`, draft: true,
      generate_release_notes: true,
      body: '安装包：Windows 10/11 x64 使用 `-setup.exe`；macOS 11+（Apple Silicon / Intel）使用通用 `.dmg`。\n\n每个安装包均附带 SHA-256 校验文件，`latest.json` 用于应用内更新。Windows 安装包尚未做代码签名；macOS 使用 ad-hoc 签名，尚未进行 Developer ID 签名及公证。',
    })).data;
  }

  const manifest = {
    version,
    notes: release.body || '',
    pub_date: new Date().toISOString(),
    homepage: `https://github.com/${owner}/${repo}/releases/tag/${tag}`,
    platforms: {
      'windows-x86_64': assets[0],
      'macos-aarch64': assets[1],
      'macos-x86_64': assets[1],
    },
  };
  fs.writeFileSync(path.join(directory, 'latest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  names.push('latest.json');

  for (const name of names) {
    const existing = release.assets.find(asset => asset.name === name);
    if (existing) await github.rest.repos.deleteReleaseAsset({ owner, repo, asset_id: existing.id });
    await github.rest.repos.uploadReleaseAsset({
      owner, repo, release_id: release.id, name,
      data: fs.readFileSync(path.join(directory, name)),
    });
  }
  const published = (await github.rest.repos.updateRelease({
    owner, repo, release_id: release.id, draft: false, make_latest: 'legacy',
  })).data;
  core.notice(`Published ${published.html_url}`);
  await core.summary.addLink(`Tramoing ${tag}`, published.html_url).write();
};
