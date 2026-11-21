const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const publishRelease = require('./release.cjs');

function fixture(t, { existing, failUpload } = {}) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'tramoing-release-'));
  t.after(() => {
    assert.equal(path.dirname(path.resolve(directory)), path.resolve(os.tmpdir()));
    assert.ok(path.basename(directory).startsWith('tramoing-release-'));
    fs.rmSync(directory, { recursive: true, force: true });
  });
  const version = '0.1.1';
  const tag = `v${version}`;
  const packages = [`Tramoing-${version}-setup.exe`, `Tramoing-${version}.dmg`];
  for (const name of packages) {
    const bytes = Buffer.from(`test package: ${name}`);
    fs.writeFileSync(path.join(directory, name), bytes);
    const hash = crypto.createHash('sha256').update(bytes).digest('hex');
    fs.writeFileSync(path.join(directory, `${name}.sha256`), `${hash}  ${name}\n`);
  }
  const events = [];
  const uploaded = new Map();
  const release = {
    id: 42, tag_name: tag, draft: true, body: '测试发布说明', assets: [],
    html_url: `https://github.com/kkltgood/tramoing/releases/tag/${tag}`,
    ...existing,
  };
  const github = {
    rest: { repos: {
      async getReleaseByTag() {
        // Exercise the draft recovery path used when GitHub omits a draft.
        if (!existing || existing.draft) throw Object.assign(new Error('Not found'), { status: 404 });
        return { data: release };
      },
      async listReleases() {},
      async createRelease(input) {
        assert.equal(input.draft, true);
        events.push('create');
        return { data: release };
      },
      async deleteReleaseAsset({ asset_id }) { events.push(`delete:${asset_id}`); },
      async uploadReleaseAsset({ name, data }) {
        events.push(`upload:${name}`);
        if (name === failUpload) throw new Error('Upload interrupted');
        uploaded.set(name, data);
      },
      async updateRelease(input) {
        assert.equal(input.draft, false);
        events.push('publish');
        return { data: release };
      },
    } },
    async paginate() { return existing ? [release] : []; },
  };
  const core = { notice() {}, summary: { addLink() { return { async write() {} }; } } };
  return {
    directory, packages, uploaded, events,
    run: () => publishRelease({
      github, core, context: { repo: { owner: 'kkltgood', repo: 'tramoing' } }, directory, version, tag,
    }),
  };
}

test('publishes a manifest for Windows and both Mac architectures with verified bytes', async t => {
  const f = fixture(t);
  await f.run();
  assert.equal(f.events[0], 'create');
  assert.equal(f.events.at(-1), 'publish');
  assert.equal(f.uploaded.size, 5);
  const manifest = JSON.parse(f.uploaded.get('latest.json'));
  assert.equal(manifest.version, '0.1.1');
  assert.equal(manifest.notes, '测试发布说明');
  assert.equal(manifest.homepage, 'https://github.com/kkltgood/tramoing/releases/tag/v0.1.1');
  assert.ok(Number.isFinite(Date.parse(manifest.pub_date)));
  assert.deepEqual(Object.keys(manifest.platforms).sort(), ['macos-aarch64', 'macos-x86_64', 'windows-x86_64']);
  for (const [platform, asset] of Object.entries(manifest.platforms)) {
    const name = platform.startsWith('windows') ? f.packages[0] : f.packages[1];
    const bytes = f.uploaded.get(name);
    assert.equal(asset.url, `https://github.com/kkltgood/tramoing/releases/download/v0.1.1/${name}`);
    assert.equal(asset.sha256, crypto.createHash('sha256').update(bytes).digest('hex'));
    assert.equal(asset.size, bytes.length);
  }
});

test('recovers an interrupted draft and replaces its old update manifest before publishing', async t => {
  const f = fixture(t, { existing: { draft: true, assets: [{ id: 7, name: 'latest.json' }] } });
  await f.run();
  assert.ok(!f.events.includes('create'));
  assert.ok(f.events.indexOf('delete:7') < f.events.indexOf('upload:latest.json'));
  assert.equal(f.events.at(-1), 'publish');
});

test('leaves an incomplete upload as a draft', async t => {
  const f = fixture(t, { failUpload: 'latest.json' });
  await assert.rejects(f.run(), /Upload interrupted/);
  assert.ok(!f.events.includes('publish'));
});

test('does not modify an already published release', async t => {
  const f = fixture(t, { existing: { draft: false } });
  await assert.rejects(f.run(), /already published/);
  assert.deepEqual(f.events, []);
});

test('rejects corrupt or missing packages before creating a release', async t => {
  const corrupt = fixture(t);
  fs.appendFileSync(path.join(corrupt.directory, corrupt.packages[0]), 'tampered');
  await assert.rejects(corrupt.run(), /Checksum mismatch/);
  assert.deepEqual(corrupt.events, []);
  const missing = fixture(t);
  fs.unlinkSync(path.join(missing.directory, missing.packages[1]));
  await assert.rejects(missing.run(), /Unexpected release assets/);
  assert.deepEqual(missing.events, []);
});
