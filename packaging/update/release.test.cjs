const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const publishRelease = require('./release.cjs');

function fixture(t, { existing, failUpload, notes } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'tramoing-release-'));
  t.after(() => {
    assert.equal(path.dirname(path.resolve(root)), path.resolve(os.tmpdir()));
    assert.ok(path.basename(root).startsWith('tramoing-release-'));
    fs.rmSync(root, { recursive: true, force: true });
  });
  const directory = path.join(root, 'release-files');
  fs.mkdirSync(directory);
  const version = '0.1.1';
  const tag = `v${version}`;
  const notesFile = path.join(root, 'notes.md');
  if (notes !== undefined) fs.writeFileSync(notesFile, notes);
  const packages = [
    `Tramoing-${version}-windows-x64-setup.exe`,
    `Tramoing-${version}-windows-arm64-setup.exe`,
    `Tramoing-${version}-macos-arm64.dmg`,
    `Tramoing-${version}-macos-x64.dmg`,
  ];
  for (const name of packages) {
    const bytes = Buffer.from(`test package: ${name}`);
    fs.writeFileSync(path.join(directory, name), bytes);
    const hash = crypto.createHash('sha256').update(bytes).digest('hex');
    fs.writeFileSync(path.join(directory, `${name}.sha256`), `${hash}  ${name}\n`);
  }
  const events = [];
  const created = [];
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
        created.push(input);
        release.body = input.generate_release_notes ? release.body : input.body;
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
    directory, packages, uploaded, events, created,
    run: () => publishRelease({
      github, core, context: { repo: { owner: 'kkltgood', repo: 'tramoing' } }, directory, version, tag, notesFile,
    }),
  };
}

test('publishes four distinct architecture packages with verified bytes and compatible platform keys', async t => {
  const f = fixture(t);
  await f.run();
  assert.equal(f.events[0], 'create');
  assert.equal(f.events.at(-1), 'publish');
  assert.equal(f.uploaded.size, 9);
  const manifest = JSON.parse(f.uploaded.get('latest.json'));
  assert.equal(manifest.version, '0.1.1');
  assert.equal(manifest.notes, '测试发布说明');
  assert.equal(manifest.homepage, 'https://github.com/kkltgood/tramoing/releases/tag/v0.1.1');
  assert.ok(Number.isFinite(Date.parse(manifest.pub_date)));
  const expected = {
    'windows-x86_64': 'Tramoing-0.1.1-windows-x64-setup.exe',
    'windows-aarch64': 'Tramoing-0.1.1-windows-arm64-setup.exe',
    'macos-aarch64': 'Tramoing-0.1.1-macos-arm64.dmg',
    'macos-x86_64': 'Tramoing-0.1.1-macos-x64.dmg',
  };
  assert.deepEqual(Object.keys(manifest.platforms).sort(), Object.keys(expected).sort());
  assert.equal(new Set(Object.values(manifest.platforms).map(asset => asset.url)).size, 4);
  for (const [platform, asset] of Object.entries(manifest.platforms)) {
    const name = expected[platform];
    const bytes = f.uploaded.get(name);
    assert.equal(asset.url, `https://github.com/kkltgood/tramoing/releases/download/v0.1.1/${name}`);
    assert.equal(asset.sha256, crypto.createHash('sha256').update(bytes).digest('hex'));
    assert.equal(asset.size, bytes.length);
  }
});

test('uses the version notes in both the release body and update manifest', async t => {
  const notes = '本次更新修复 macOS 高能耗，并增加应用置顶。\n\n- 重启后保留置顶顺序。';
  const f = fixture(t, { notes: `\n${notes}\n` });
  await f.run();
  assert.equal(f.created[0].generate_release_notes, false);
  assert.equal(f.created[0].body, notes);
  const manifest = JSON.parse(f.uploaded.get('latest.json'));
  assert.equal(manifest.notes, notes);
  assert.equal(f.events.at(-1), 'publish');
});

test('missing or empty version notes retain generated release notes', async t => {
  for (const notes of [undefined, ' \n\t']) {
    const f = fixture(t, { notes });
    await f.run();
    assert.equal(f.created[0].generate_release_notes, true);
    assert.match(f.created[0].body, /安装包/);
    for (const name of f.packages) assert.ok(f.created[0].body.includes(name));
    assert.equal(JSON.parse(f.uploaded.get('latest.json')).notes, '测试发布说明');
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
  for (let index = 0; index < 4; index++) {
    const corrupt = fixture(t);
    fs.appendFileSync(path.join(corrupt.directory, corrupt.packages[index]), 'tampered');
    await assert.rejects(corrupt.run(), /Checksum mismatch/);
    assert.deepEqual(corrupt.events, []);
    for (const suffix of ['', '.sha256']) {
      const missing = fixture(t);
      fs.unlinkSync(path.join(missing.directory, `${missing.packages[index]}${suffix}`));
      await assert.rejects(missing.run(), /Unexpected release assets/);
      assert.deepEqual(missing.events, []);
    }
  }
});

test('rejects stale packages with generic filenames instead of publishing ambiguous downloads', async t => {
  for (const name of ['Tramoing-0.1.1.dmg', 'Tramoing-0.1.1-setup.exe']) {
    const f = fixture(t);
    fs.writeFileSync(path.join(f.directory, name), 'old package');
    await assert.rejects(f.run(), /Unexpected release assets/);
    assert.deepEqual(f.events, []);
  }
});
