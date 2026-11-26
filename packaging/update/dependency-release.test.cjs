const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { readDependencies, publishDependencies } = require('./dependency-release.cjs');
const hash = (bytes, algorithm = 'sha256') => crypto.createHash(algorithm).update(bytes).digest('hex');

function fixture(t, { dependency = 'all', published = false, draft = false } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'tramoing-dependency-release-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const repo = { owner: 'kkltgood', repo: 'tramoing' };
  const dependencies = [
    { id: 'dotnet', name: '.NET Runtime', version: '10.0.15', publisher: 'Microsoft Corporation' },
    { id: 'pawnio', name: 'PawnIO', version: '2.2.0', publisher: 'namazso.eu' },
  ].map(item => {
    const name = item.id === 'dotnet' ? `dotnet-runtime-${item.version}-win-x64.exe` : 'PawnIO_setup.exe';
    const relative_path = `${item.id}/${item.version}/windows-x64/${name}`;
    const bytes = Buffer.from(`original signed fixture for ${item.id}`);
    const filename = path.join(root, relative_path);
    fs.mkdirSync(path.dirname(filename), { recursive: true });
    fs.writeFileSync(filename, bytes);
    return {
      ...item, architecture: 'x64', relative_path, size: bytes.length,
      sha256: hash(bytes), ...(item.id === 'dotnet' ? { official_sha512: hash(bytes, 'sha512') } : {}),
      url: item.id === 'dotnet'
        ? `https://builds.dotnet.microsoft.com/dotnet/Runtime/${item.version}/${name}`
        : `https://github.com/namazso/PawnIO.Setup/releases/download/${item.version}/${name}`,
      fallbacks: [`https://github.com/kkltgood/tramoing/releases/download/deps-${item.id}-${item.version}/${name}`],
    };
  });
  const manifestPath = path.join(root, 'manifest.json');
  const save = () => fs.writeFileSync(manifestPath, JSON.stringify({ schema: 1, dependencies }));
  save();
  const items = readDependencies({ manifestPath, cacheRoot: root, dependency, repo });
  const releases = new Map();
  const contents = new Map();
  const events = [];
  let sequence = 10;
  let failUpload = null;
  let corruptUpload = null;
  function asset(name, bytes) {
    const value = { id: sequence++, name, size: bytes.length };
    contents.set(value.id, Buffer.from(bytes));
    return value;
  }
  if (published || draft) {
    for (const item of items) {
      releases.set(item.tag, { id: sequence++, tag_name: item.tag, draft,
        assets: [...item.files].map(([name, bytes]) => asset(name, bytes)),
      });
    }
  }
  const github = {
    rest: { repos: {
      async getReleaseByTag({ tag }) {
        const release = releases.get(tag);
        if (!release || release.draft) throw Object.assign(new Error('Not found'), { status: 404 });
        return { data: release };
      },
      async listReleases() {},
      async createRelease(input) {
        assert.equal(input.draft, true);
        assert.equal(input.make_latest, 'false');
        assert.equal(input.generate_release_notes, false);
        assert.equal(input.target_commitish, 'verified-commit');
        assert.match(input.tag_name, /^deps-(dotnet|pawnio)-/);
        events.push(['create', input.tag_name]);
        const release = { id: sequence++, tag_name: input.tag_name, draft: true, assets: [] };
        releases.set(release.tag_name, release);
        return { data: release };
      },
      async getReleaseAsset({ asset_id }) { return { data: contents.get(asset_id) }; },
      async deleteReleaseAsset({ asset_id }) {
        events.push(['delete', asset_id]);
        for (const release of releases.values()) {
          assert.equal(release.draft, true, 'must never delete published assets');
          release.assets = release.assets.filter(asset => asset.id !== asset_id);
        }
        contents.delete(asset_id);
      },
      async uploadReleaseAsset({ release_id, name, data }) {
        events.push(['upload', name]);
        if (name === failUpload) throw new Error('Interrupted upload');
        const release = [...releases.values()].find(item => item.id === release_id);
        assert.equal(release.draft, true);
        const added = asset(name, name === corruptUpload ? Buffer.from('unexpected remote data') : data);
        release.assets.push(added);
        return { data: added };
      },
      async updateRelease(input) {
        assert.equal(input.draft, false);
        assert.equal(input.make_latest, 'false');
        events.push(['publish', input.release_id]);
        const release = [...releases.values()].find(item => item.id === input.release_id);
        release.draft = false;
        return { data: { ...release, html_url: `https://github.com/kkltgood/tramoing/releases/tag/${release.tag_name}` } };
      },
    } },
    async paginate() { return [...releases.values()]; },
  };
  return {
    root, repo, dependencies, manifestPath, save, items, events, releases, contents,
    setFailUpload: name => { failUpload = name; },
    setCorruptUpload: name => { corruptUpload = name; },
    read: () => readDependencies({ manifestPath, cacheRoot: root, dependency, repo }),
    run: () => publishDependencies({ github, context: { repo, sha: 'verified-commit' }, core: { notice() {} }, manifestPath, cacheRoot: root, dependency }),
  };
}

test('publishes independent dependency releases without replacing the latest app release', async t => {
  const f = fixture(t);
  await f.run();
  assert.deepEqual([...f.releases.keys()], ['deps-dotnet-10.0.15', 'deps-pawnio-2.2.0']);
  assert.equal(f.events.filter(([event]) => event === 'upload').length, 6);
  assert.equal(f.events.filter(([event]) => event === 'publish').length, 2);
  for (const release of f.releases.values()) {
    assert.equal(release.draft, false);
    assert.equal(release.assets.length, 3);
  }
});

test('selecting one dependency leaves the other release untouched', async t => {
  const f = fixture(t, { dependency: 'pawnio' });
  await f.run();
  assert.deepEqual([...f.releases.keys()], ['deps-pawnio-2.2.0']);
});

test('published assets are read and verified but never mutated', async t => {
  const f = fixture(t, { published: true });
  await f.run();
  assert.deepEqual(f.events, []);
  const release = f.releases.values().next().value;
  const binary = release.assets[0];
  const changed = Buffer.from(f.contents.get(binary.id));
  changed[0] ^= 1;
  f.contents.set(binary.id, changed);
  await assert.rejects(f.run(), /Published dependency asset differs/);
  assert.deepEqual(f.events, []);
});

test('a published release missing an asset is rejected without repair or overwrite', async t => {
  const f = fixture(t, { published: true });
  f.releases.values().next().value.assets.pop();
  await assert.rejects(f.run(), /missing or duplicates/);
  assert.deepEqual(f.events, []);
});

test('an interrupted draft is resumed and matching uploaded assets are reused', async t => {
  const f = fixture(t, { dependency: 'dotnet' });
  f.setFailUpload('dependency-manifest.json');
  await assert.rejects(f.run(), /Interrupted upload/);
  assert.equal(f.releases.values().next().value.draft, true);
  assert.equal(f.events.filter(([event]) => event === 'publish').length, 0);
  f.events.length = 0;
  f.setFailUpload(null);
  await f.run();
  assert.deepEqual(f.events.map(([event]) => event), ['upload', 'publish']);
});

test('corrupted draft assets may be replaced but are read back before publication', async t => {
  const f = fixture(t, { dependency: 'pawnio', draft: true });
  const release = f.releases.values().next().value;
  f.contents.set(release.assets[0].id, Buffer.from('bad partial upload'));
  await f.run();
  assert.deepEqual(f.events.map(([event]) => event), ['delete', 'upload', 'publish']);
  const g = fixture(t, { dependency: 'dotnet' });
  g.setCorruptUpload('dependency-manifest.json');
  await assert.rejects(g.run(), /Published dependency asset differs/);
  assert.equal(g.releases.values().next().value.draft, true);
});

test('all selected local files are verified before any remote mutation', async t => {
  const f = fixture(t);
  const dependency = f.dependencies[1];
  fs.appendFileSync(path.join(f.root, dependency.relative_path), 'corrupt');
  await assert.rejects(f.run(), /checksum or size mismatch/);
  assert.deepEqual(f.events, []);
});

test('rejects incorrect SHA-512, nonofficial sources and unversioned fallbacks', t => {
  for (const change of [
    item => { item.official_sha512 = 'a'.repeat(128); },
    item => { item.url = 'https://example.com/runtime.exe'; },
    item => { item.fallbacks = ['https://github.com/kkltgood/tramoing/releases/latest/download/runtime.exe']; },
    item => { item.relative_path = '../outside.exe'; },
    item => { item.publisher = 'Unrelated publisher'; },
  ]) {
    const f = fixture(t);
    change(f.dependencies[0]);
    f.save();
    assert.throws(f.read, /Invalid dependency metadata|mismatch/);
  }
});

test('rejects unknown selection and publishing from another repository', t => {
  const f = fixture(t);
  assert.throws(() => readDependencies({ manifestPath: f.manifestPath, cacheRoot: f.root, dependency: 'unexpected', repo: f.repo }), /Unknown dependency/);
  assert.throws(() => readDependencies({ manifestPath: f.manifestPath, cacheRoot: f.root, repo: { owner: 'fork', repo: 'tramoing' } }), /only be published/);
});
