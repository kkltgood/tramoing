const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { readComponent, publishComponent } = require('./component-release.cjs');

function fixture(t, { published = false, draft = false, failUpload } = {}) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'tramoing-component-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const repo = { owner: 'kkltgood', repo: 'tramoing' };
  const name = 'Tramoing-Sensors-1.0.0-windows-x64-setup.exe';
  const bytes = Buffer.from('independent optional component');
  const sha256 = crypto.createHash('sha256').update(bytes).digest('hex');
  const manifest = {
    component_version: '1.0.0', protocol: 1, architecture: 'x64',
    url: `https://github.com/kkltgood/tramoing/releases/download/sensors-v1.0.0/${name}`,
    sha256, size: bytes.length, source_sha256: 'a'.repeat(64),
  };
  fs.writeFileSync(path.join(directory, name), bytes);
  fs.writeFileSync(path.join(directory, `${name}.sha256`), `${sha256}  ${name}\n`);
  fs.writeFileSync(path.join(directory, 'sensors-component.json'), JSON.stringify(manifest));
  const component = readComponent(directory, repo);
  const remote = new Map(component.files);
  const events = [];
  const release = {
    id: 51, tag_name: component.tag, draft,
    assets: [...component.files.keys()].map((name, id) => ({ id, name })),
  };
  const github = {
    rest: { repos: {
      async getReleaseByTag() {
        if (published) return { data: release };
        throw Object.assign(new Error('Not found'), { status: 404 });
      },
      async listReleases() {},
      async createRelease(input) {
        assert.equal(input.tag_name, 'sensors-v1.0.0');
        assert.equal(input.draft, true);
        assert.equal(input.make_latest, 'false');
        events.push('create');
        return { data: { ...release, draft: true, assets: [] } };
      },
      async getReleaseAsset({ asset_id }) {
        return { data: remote.get(release.assets.find(asset => asset.id === asset_id).name) };
      },
      async uploadReleaseAsset({ name, data }) {
        events.push(`upload:${name}`);
        if (failUpload === name) throw new Error('Interrupted');
        remote.set(name, data);
      },
      async deleteReleaseAsset({ asset_id }) { events.push(`delete:${asset_id}`); },
      async updateRelease(input) {
        assert.equal(input.draft, false);
        assert.equal(input.make_latest, 'false');
        events.push('publish');
      },
    } },
    async paginate() { return draft ? [release] : []; },
  };
  return {
    directory, component, name, manifest, remote, events,
    run: () => publishComponent({ github, context: { repo, sha: 'test-commit' }, core: { notice() {} }, component }),
  };
}

test('publishes an independently versioned component without changing the latest main release', async t => {
  const f = fixture(t);
  await f.run();
  assert.equal(f.events[0], 'create');
  assert.equal(f.events.at(-1), 'publish');
  assert.equal(f.events.filter(item => item.startsWith('upload:')).length, 3);
});

test('reuses identical published component bytes without any mutation', async t => {
  const f = fixture(t, { published: true });
  await f.run();
  assert.deepEqual(f.events, []);
});

test('refuses to replace published installer or source metadata under an existing version', async t => {
  const installer = fixture(t, { published: true });
  installer.remote.set(installer.name, Buffer.from('different bytes'));
  await assert.rejects(installer.run(), /Published sensor component differs/);
  assert.deepEqual(installer.events, []);
  const manifest = fixture(t, { published: true });
  manifest.remote.set('sensors-component.json', Buffer.from(JSON.stringify({ ...manifest.manifest, source_sha256: 'b'.repeat(64) })));
  await assert.rejects(manifest.run(), /source_sha256/);
  assert.deepEqual(manifest.events, []);
});

test('an interrupted component upload stays draft and can be resumed', async t => {
  const f = fixture(t, { draft: true, failUpload: 'sensors-component.json' });
  await assert.rejects(f.run(), /Interrupted/);
  assert.ok(!f.events.includes('create'));
  assert.ok(!f.events.includes('publish'));
  assert.ok(f.events.some(event => event.startsWith('delete:')));
});

test('rejects corrupted local component data and unversioned download URLs before publishing', t => {
  const f = fixture(t);
  fs.appendFileSync(path.join(f.directory, f.name), 'corrupt');
  assert.throws(() => readComponent(f.directory, { owner: 'kkltgood', repo: 'tramoing' }), /checksum or size/);
  const badUrl = fixture(t);
  fs.writeFileSync(path.join(badUrl.directory, 'sensors-component.json'), JSON.stringify({ ...badUrl.manifest, url: 'https://example.com/latest.exe' }));
  assert.throws(() => readComponent(badUrl.directory, { owner: 'kkltgood', repo: 'tramoing' }), /immutable version/);
});
