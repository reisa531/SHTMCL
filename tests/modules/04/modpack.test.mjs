// Cross-checks the committed `.mrpack` fixtures against the rules the Rust reader
// enforces. Runs with the built-in test runner:
//   node --test tests/modules/04/modpack.test.mjs
//
// This file deliberately re-implements the *rules* (not the Rust code) so a
// fixture that silently drifts away from the documented format fails here first.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, existsSync } from 'node:fs';
import { inflateRawSync } from 'node:zlib';
import { join } from 'node:path';

import { FIXTURE_DIR, checkFixtures, sampleManifest } from './make-fixtures.mjs';

const INDEX_NAME = 'modrinth.index.json';
const OVERRIDE_PREFIXES = [
  ['overrides/', 'shared'],
  ['client-overrides/', 'client'],
  ['server-overrides/', 'server'],
];

function fixture(name) {
  const path = join(FIXTURE_DIR, name);
  assert.ok(existsSync(path), `fixture ${name} is missing; run make-fixtures.mjs`);
  return readFileSync(path);
}

/** Minimal central-directory reader, mirrors the budgets in paths.rs. */
function readEntryNames(bytes) {
  let eocd = -1;
  for (let offset = bytes.length - 22; offset >= 0; offset -= 1) {
    if (bytes.readUInt32LE(offset) === 0x06054b50) {
      eocd = offset;
      break;
    }
  }
  assert.notEqual(eocd, -1, 'no end-of-central-directory record');
  const count = bytes.readUInt16LE(eocd + 10);
  let offset = bytes.readUInt32LE(eocd + 16);
  const names = [];
  for (let index = 0; index < count; index += 1) {
    assert.equal(bytes.readUInt32LE(offset), 0x02014b50, 'bad central directory signature');
    const nameLength = bytes.readUInt16LE(offset + 28);
    const extraLength = bytes.readUInt16LE(offset + 30);
    const commentLength = bytes.readUInt16LE(offset + 32);
    names.push(bytes.subarray(offset + 46, offset + 46 + nameLength).toString('utf8'));
    offset += 46 + nameLength + extraLength + commentLength;
  }
  return names;
}

function readStoredEntry(bytes, wanted, { allowDeflate = false } = {}) {
  const names = readEntryNames(bytes);
  assert.ok(names.includes(wanted), `${wanted} is not in the archive`);
  // The fixture writer emits entries in order, so the payload can be located by
  // scanning local headers.
  let offset = 0;
  while (offset + 30 <= bytes.length && bytes.readUInt32LE(offset) === 0x04034b50) {
    const method = bytes.readUInt16LE(offset + 8);
    const compressed = bytes.readUInt32LE(offset + 18);
    const uncompressed = bytes.readUInt32LE(offset + 22);
    const nameLength = bytes.readUInt16LE(offset + 26);
    const extraLength = bytes.readUInt16LE(offset + 28);
    const name = bytes.subarray(offset + 30, offset + 30 + nameLength).toString('utf8');
    const dataStart = offset + 30 + nameLength + extraLength;
    const payload = bytes.subarray(dataStart, dataStart + compressed);
    if (!allowDeflate) {
      assert.equal(method, 0, 'fixtures default to stored archives');
    }
    if (name === wanted) {
      if (method === 0) {
        assert.equal(payload.length, uncompressed);
        return payload;
      }
      assert.equal(method, 8, `unexpected compression method ${method}`);
      const inflated = inflateRawSync(payload);
      assert.equal(inflated.length, uncompressed);
      return inflated;
    }
    offset = dataStart + compressed;
  }
  throw new Error(`${wanted} payload not found`);
}

function entryMethods(bytes) {
  const methods = [];
  let offset = 0;
  while (offset + 30 <= bytes.length && bytes.readUInt32LE(offset) === 0x04034b50) {
    methods.push(bytes.readUInt16LE(offset + 8));
    const compressed = bytes.readUInt32LE(offset + 18);
    const nameLength = bytes.readUInt16LE(offset + 26);
    const extraLength = bytes.readUInt16LE(offset + 28);
    offset += 30 + nameLength + extraLength + compressed;
  }
  return methods;
}

function normalise(raw) {
  if (!raw || raw.includes('\\') || raw.startsWith('/') || /^[A-Za-z]:/.test(raw)) {
    return { ok: false, reason: 'absolute_or_escaped' };
  }
  const parts = [];
  for (const part of raw.split('/')) {
    if (part === '' || part === '.') continue;
    if (part === '..') return { ok: false, reason: 'parent_dir' };
    if (part.endsWith(' ') || part.endsWith('.')) return { ok: false, reason: 'trailing' };
    parts.push(part);
  }
  if (parts.length === 0) return { ok: false, reason: 'empty' };
  return { ok: true, path: parts.join('/') };
}

function classifyOverride(name) {
  for (const [prefix, scope] of OVERRIDE_PREFIXES) {
    if (name.startsWith(prefix)) {
      const rest = name.slice(prefix.length);
      if (!rest || rest.endsWith('/')) return null;
      const normalised = normalise(rest);
      return normalised.ok ? { scope, target: normalised.path } : { scope, error: normalised.reason };
    }
  }
  return null;
}

const EXPORT_EXCLUDED_PREFIXES = ['logs/', 'crash-reports/', '.microsoft/', 'launcher_accounts/', 'screenshots/', 'debug/'];
const EXPORT_EXCLUDED_FILES = [
  'usercache.json',
  'launcher_profiles.json',
  'launcher_accounts.json',
  'realms_persistence.json',
  'launcher_accounts_microsoft_store.json',
];

function excludedFromExport(path, includeSaves) {
  const lower = path.toLowerCase();
  if (EXPORT_EXCLUDED_FILES.includes(lower)) return true;
  if (EXPORT_EXCLUDED_PREFIXES.some((prefix) => lower.startsWith(prefix))) return true;
  if (lower.endsWith('.log') || lower.endsWith('.log.gz')) return true;
  if (!includeSaves && lower.startsWith('saves/')) return true;
  return false;
}

test('committed fixtures match the generated bytes', () => {
  assert.deepEqual(checkFixtures(), []);
});

test('sample archive holds a valid modrinth index and the documented overrides', () => {
  const bytes = fixture('sample.mrpack');
  const names = readEntryNames(bytes);
  assert.ok(names.includes(INDEX_NAME));

  const manifest = JSON.parse(readStoredEntry(bytes, INDEX_NAME).toString('utf8'));
  assert.equal(manifest.formatVersion, 1);
  assert.equal(manifest.game, 'minecraft');
  assert.equal(typeof manifest.name, 'string');
  assert.equal(typeof manifest.versionId, 'string');
  assert.equal(manifest.dependencies.minecraft, '1.20.1');
  assert.ok(manifest.files.length >= 2);

  for (const file of manifest.files) {
    const normalised = normalise(file.path);
    assert.ok(normalised.ok, `file path must stay inside the root: ${file.path}`);
    assert.ok(file.hashes.sha1 || file.hashes.sha512, 'every file needs a hash');
    assert.ok(Array.isArray(file.downloads));
  }

  const overrides = names
    .map(classifyOverride)
    .filter(Boolean)
    .map((entry) => `${entry.scope}:${entry.target}`)
    .sort();
  assert.deepEqual(overrides, [
    'client:config/client-only.json',
    'server:config/server-only.json',
    'shared:config/sodium-options.json',
  ]);
});

test('traversal fixture contains an entry the reader must refuse', () => {
  const names = readEntryNames(fixture('traversal.mrpack'));
  const escaping = names.filter((name) => name.includes('..'));
  assert.equal(escaping.length, 1, 'fixture must carry exactly one escaping entry');
  const classified = classifyOverride(escaping[0]);
  assert.ok(classified && classified.error === 'parent_dir');
});

test('incomplete fixture is missing the required fields', () => {
  const manifest = JSON.parse(
    readStoredEntry(fixture('incomplete.mrpack'), INDEX_NAME).toString('utf8'),
  );
  assert.equal(manifest.name, undefined);
  assert.equal(manifest.versionId, undefined);
  assert.equal(manifest.formatVersion, 1);
});

test('deflated fixture uses method 8 and inflates to the same manifest', () => {
  const deflated = fixture('sample-deflate.mrpack');
  assert.deepEqual(
    [...new Set(entryMethods(deflated))],
    [8],
    'every entry must be raw-deflated',
  );
  const fromDeflate = readStoredEntry(deflated, INDEX_NAME, { allowDeflate: true }).toString(
    'utf8',
  );
  const fromStored = readStoredEntry(fixture('sample.mrpack'), INDEX_NAME).toString('utf8');
  assert.deepEqual(JSON.parse(fromDeflate), JSON.parse(fromStored));
});

test('sample manifest keeps a stable, deduplicated file set', () => {
  const first = sampleManifest();
  const second = sampleManifest();
  assert.deepEqual(first, second);
  const paths = first.files.map((file) => file.path.toLowerCase());
  assert.equal(new Set(paths).size, paths.length, 'paths must be unique case-insensitively');
});

test('export excludes credentials, logs and unselected saves', () => {
  assert.ok(excludedFromExport('logs/latest.log', true));
  assert.ok(excludedFromExport('usercache.json', true));
  assert.ok(excludedFromExport('.microsoft/account.json', true));
  assert.ok(excludedFromExport('saves/world/level.dat', false));
  assert.ok(!excludedFromExport('saves/world/level.dat', true));
  assert.ok(!excludedFromExport('mods/sodium-0.5.3.jar', false));
});
