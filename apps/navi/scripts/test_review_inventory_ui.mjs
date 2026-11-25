/**
 * Unit tests for station.js private review inventory validator and DOM controls.
 * Loads the real review-inventory block from apps/navi/ui/station.js inside node:vm.
 */
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

// Objects from the vm realm have a different Object prototype; compare structurally.
function deepEq(actual, expected, message) {
  assert.equal(JSON.stringify(actual), JSON.stringify(expected), message);
}

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const STATION_PATH = path.resolve(__dirname, '../ui/station.js');
const stationSource = fs.readFileSync(STATION_PATH, 'utf8');

const START_MARKER = '/* review-inventory:start */';
const END_MARKER = '/* review-inventory:end */';
const startIdx = stationSource.indexOf(START_MARKER);
const endIdx = stationSource.indexOf(END_MARKER);
assert.ok(startIdx !== -1 && endIdx > startIdx, 'review-inventory block markers missing in station.js');
const blockSource = stationSource.slice(startIdx, endIdx + END_MARKER.length);

function makeFakeDocument() {
  class FakeElement {
    constructor(tagName) {
      this.tagName = tagName.toUpperCase();
      this.children = [];
      this.attributes = Object.create(null);
      this.disabled = false;
      this.type = '';
      this.title = '';
      this.className = '';
      this.onclick = null;
      this._textContent = '';
    }
    get firstChild() {
      return this.children.length ? this.children[0] : null;
    }
    get textContent() {
      return this._textContent;
    }
    set textContent(v) {
      this._textContent = String(v);
    }
    get innerHTML() {
      throw new Error('innerHTML read forbidden');
    }
    set innerHTML(_v) {
      throw new Error('innerHTML write forbidden');
    }
    setAttribute(name, value) {
      this.attributes[name] = String(value);
    }
    getAttribute(name) {
      return Object.prototype.hasOwnProperty.call(this.attributes, name)
        ? this.attributes[name]
        : null;
    }
    appendChild(child) {
      this.children.push(child);
      return child;
    }
    removeChild(child) {
      const idx = this.children.indexOf(child);
      if (idx === -1) throw new Error('Child not found');
      this.children.splice(idx, 1);
      return child;
    }
  }
  return {
    createElement(tag) {
      return new FakeElement(tag);
    },
  };
}

function loadSandbox({ rpc, activeProjectId } = {}) {
  const forbiddenStorage = new Proxy(
    {},
    {
      get() {
        throw new Error('localStorage must not be used by review inventory controls');
      },
    }
  );
  const sandbox = {
    TextEncoder,
    document: makeFakeDocument(),
    localStorage: forbiddenStorage,
    rpc: rpc || (async () => { throw new Error('unexpected rpc'); }),
    activeProjectId: activeProjectId || (() => 'proj-1'),
  };
  vm.runInNewContext(
    blockSource + '\nthis.__exports = { validateReviewInventoryResult, reviewInventoryControls };',
    sandbox,
    { filename: 'station.js#review-inventory' }
  );
  return sandbox.__exports;
}

function sampleValidItem(hexSuffix = '39b3cfb6b0d77be498d5d0785020f1c5', originalPath = 'src/s.py') {
  return {
    review_id: `review-${hexSuffix}`,
    valid: true,
    status: 'valid',
    applied: false,
    requires_fresh_validation: true,
    original_path: originalPath,
    proposal_digest: '25ada7bca03323eab54c9b31f86bdad0f180b84fba1ceb4af3ba1ada06fc1b8c',
    before_sha256: '9f56e761d79bfdb34304a012586cb04d16b435ef6130091a97702e559260a2f2',
    after_sha256: '98752ee28d5484bdc2814fb70adb6a0b2fb31f6a9b8ee7ae81fd2fc9cf300b3b',
    before_bytes: 5,
    after_bytes: 4,
    files: ['before.txt', 'proposed-after.txt', 'review.json'],
  };
}

function sampleInvalidItem(hexSuffix = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', reason = 'incomplete_or_extra_files') {
  return {
    review_id: `review-${hexSuffix}`,
    valid: false,
    status: 'invalid',
    reason,
    applied: false,
  };
}

function sampleEnvelope({ storagePresent = true, items = [], validCount = 0, invalidCount = 0 } = {}) {
  return {
    schema: 'navi-review-inventory-v1',
    applied: false,
    source_contents_exposed: false,
    storage_present: storagePresent,
    inspected_count: validCount + invalidCount,
    valid_count: validCount,
    invalid_count: invalidCount,
    max_inspected_children: 64,
    max_valid_records: 32,
    items,
    notice: 'Metadata inventory of private inert review workspaces only.',
  };
}

const tests = [];
function test(name, fn) {
  tests.push({ name, fn });
}

test('station.js registers budget, wires editPreviewPart, and avoids innerHTML/localStorage', () => {
  assert.match(stationSource, /\bpi_review_inventory\s*:\s*15000\b/);
  assert.match(
    stationSource,
    /box\.append\(workspace,workspaceState\);\s*\n\s*box\.append\(\.\.\.reviewInventoryControls\(message\)\);/
  );
  for (const forbidden of ['innerHTML', 'outerHTML', 'insertAdjacentHTML', 'localStorage', 'storage_path']) {
    assert.ok(!blockSource.includes(forbidden), `block must not contain ${forbidden}`);
  }
});

test('validator accepts empty, mixed, and max-bound (32 valid + 32 invalid) records', () => {
  const { validateReviewInventoryResult } = loadSandbox();

  const empty = sampleEnvelope({ storagePresent: false, items: [], validCount: 0, invalidCount: 0 });
  deepEq(validateReviewInventoryResult(empty), empty);

  const mixed = sampleEnvelope({
    storagePresent: true,
    items: [sampleValidItem(), sampleInvalidItem()],
    validCount: 1,
    invalidCount: 1,
  });
  deepEq(validateReviewInventoryResult(mixed), mixed);

  const maxItems = [];
  for (let i = 0; i < 32; i++) {
    maxItems.push(sampleValidItem(i.toString(16).padStart(32, '0'), 'mod.py'));
  }
  for (let i = 32; i < 64; i++) {
    maxItems.push(sampleInvalidItem(i.toString(16).padStart(32, '0'), 'verification_failed'));
  }
  const atCap = sampleEnvelope({ storagePresent: true, items: maxItems, validCount: 32, invalidCount: 32 });
  deepEq(validateReviewInventoryResult(atCap), atCap);
});

test('validator rejects bad top-level flags, schema, extra keys, and count/bound mismatches', () => {
  const { validateReviewInventoryResult } = loadSandbox();
  const base = sampleEnvelope({ storagePresent: false, items: [], validCount: 0, invalidCount: 0 });

  for (const bad of [
    null,
    undefined,
    [],
    'string',
    { ...base, schema: 'navi-review-inventory-v2' },
    { ...base, applied: true },
    { ...base, source_contents_exposed: true },
    { ...base, max_inspected_children: 65 },
    { ...base, max_valid_records: 33 },
    { ...base, storage_path: '/home/owner/.navi/proposal-reviews' },
    { ...base, extra: 1 },
    { ...base, notice: '' },
    { ...base, notice: 'bad\nnotice' },
    { ...base, notice: 'x'.repeat(513) },
    sampleEnvelope({ storagePresent: false, items: [sampleInvalidItem()], validCount: 0, invalidCount: 1 }),
  ]) {
    assert.throws(() => validateReviewInventoryResult(bad));
  }

  // Count mismatches
  const oneValid = sampleEnvelope({ storagePresent: true, items: [sampleValidItem()], validCount: 1, invalidCount: 0 });
  assert.throws(() => validateReviewInventoryResult({ ...oneValid, inspected_count: 2 }));
  assert.throws(() => validateReviewInventoryResult({ ...oneValid, valid_count: 0, invalid_count: 1 }));

  // Over caps: 33 valid or 65 inspected
  const valid33 = Array.from({ length: 33 }, (_, i) =>
    sampleValidItem(i.toString(16).padStart(32, '0'), 'a.py')
  );
  assert.throws(() =>
    validateReviewInventoryResult(
      sampleEnvelope({ storagePresent: true, items: valid33, validCount: 33, invalidCount: 0 })
    )
  );
  const invalid65 = Array.from({ length: 65 }, (_, i) =>
    sampleInvalidItem(i.toString(16).padStart(32, '0'), 'verification_failed')
  );
  assert.throws(() =>
    validateReviewInventoryResult(
      sampleEnvelope({ storagePresent: true, items: invalid65, validCount: 0, invalidCount: 65 })
    )
  );
});

test('validator rejects bad item keys, ids, hashes, paths, source fields, and >65536B payload', () => {
  const { validateReviewInventoryResult } = loadSandbox();
  const wrapValid = (item) =>
    sampleEnvelope({ storagePresent: true, items: [item], validCount: 1, invalidCount: 0 });
  const wrapInvalid = (item) =>
    sampleEnvelope({ storagePresent: true, items: [item], validCount: 0, invalidCount: 1 });

  // Bad review_id and duplicates
  for (const badId of ['review-short', 'review-39B3CFB6B0D77BE498D5D0785020F1C5', '../escape']) {
    assert.throws(() => validateReviewInventoryResult(wrapInvalid({ ...sampleInvalidItem(), review_id: badId })));
  }
  assert.throws(() =>
    validateReviewInventoryResult(
      sampleEnvelope({
        storagePresent: true,
        items: [sampleInvalidItem(), sampleInvalidItem()],
        validCount: 0,
        invalidCount: 2,
      })
    )
  );

  // Forbidden source/path/created keys on valid item
  for (const [k, v] of [
    ['before', 'secret'],
    ['after', 'secret'],
    ['proposal', {}],
    ['storage_path', '/home/owner/.navi/proposal-reviews/x'],
    ['path', 's.py'],
    ['created', '2026-11-24T00:00:00Z'],
    ['request_id', 'a'.repeat(32)],
  ]) {
    assert.throws(() => validateReviewInventoryResult(wrapValid({ ...sampleValidItem(), [k]: v })));
  }

  // Bad original_path (absolute, traversal, backslash, control/bidi chars)
  for (const badPath of ['', '/etc/passwd', '../a.py', 'a/../b.py', 'a/./b.py', 'a//b.py', 'a\\b.py', 'a\nb.py', 'a\u202eb.py']) {
    assert.throws(() => validateReviewInventoryResult(wrapValid(sampleValidItem('1'.repeat(32), badPath))));
  }

  // Bad hashes, bytes, files, flags
  assert.throws(() => validateReviewInventoryResult(wrapValid({ ...sampleValidItem(), proposal_digest: 'z'.repeat(64) })));
  assert.throws(() => validateReviewInventoryResult(wrapValid({ ...sampleValidItem(), before_sha256: { toString: () => 'a'.repeat(64) } })));
  assert.throws(() => validateReviewInventoryResult(wrapValid({ ...sampleValidItem(), before_bytes: 16385 })));
  assert.throws(() => validateReviewInventoryResult(wrapValid({ ...sampleValidItem(), after_bytes: -1 })));
  assert.throws(() => validateReviewInventoryResult(wrapValid({ ...sampleValidItem(), files: ['before.txt', 'proposed-after.txt'] })));
  assert.throws(() => validateReviewInventoryResult(wrapValid({ ...sampleValidItem(), requires_fresh_validation: false })));
  assert.throws(() => validateReviewInventoryResult(wrapValid({ ...sampleValidItem(), applied: true })));

  // Bad invalid item reason or extra field
  assert.throws(() => validateReviewInventoryResult(wrapInvalid(sampleInvalidItem('2'.repeat(32), 'other_reason'))));
  assert.throws(() => validateReviewInventoryResult(wrapInvalid({ ...sampleInvalidItem(), original_path: 's.py' })));

  // Oversized serialized payload (>65536 bytes) with otherwise schema-valid items
  const longRel = `${'a/'.repeat(1098)}f.py`;
  const bigItems = Array.from({ length: 32 }, (_, i) =>
    sampleValidItem(i.toString(16).padStart(32, '0'), longRel)
  );
  const oversized = sampleEnvelope({ storagePresent: true, items: bigItems, validCount: 32, invalidCount: 0 });
  assert.ok(new TextEncoder().encode(JSON.stringify(oversized)).length > 65536);
  assert.throws(() => validateReviewInventoryResult(oversized));
});

test('DOM controls render metadata-only text, disable while active, discard on switch, and handle errors', async () => {
  let active = 'proj-1';
  const calls = [];
  let resolvePending;
  const { reviewInventoryControls } = loadSandbox({
    activeProjectId: () => active,
    rpc: (op, params) => {
      calls.push({ op, params });
      return new Promise((resolve, reject) => {
        resolvePending = { resolve, reject };
      });
    },
  });

  const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
  assert.equal(browse.tagName, 'BUTTON');
  assert.equal(browse.textContent, 'Browse private review files');
  assert.equal(status.getAttribute('role'), 'status');
  assert.equal(list.tagName, 'UL');

  // 1) Active request disables button, ignores second click, sends exact {project_id}, renders valid + invalid
  const pending1 = browse.onclick();
  assert.equal(browse.disabled, true);
  await browse.onclick();
  assert.equal(calls.length, 1);
  deepEq(calls[0], { op: 'pi_review_inventory', params: { project_id: 'proj-1' } });

  resolvePending.resolve(
    sampleEnvelope({
      storagePresent: true,
      items: [sampleValidItem(), sampleInvalidItem()],
      validCount: 1,
      invalidCount: 1,
    })
  );
  await pending1;
  assert.equal(browse.disabled, false);
  assert.match(status.textContent, /2 inspected \(1 valid, 1 invalid\)/);
  assert.match(status.textContent, /Metadata inventory of private inert review workspaces only\./);
  assert.equal(list.children.length, 2);
  assert.equal(list.children[0].children.length, 4);
  assert.match(list.children[0].textContent, /review-39b3cfb6b0d77be498d5d0785020f1c5/);
  assert.match(list.children[0].textContent, /\| valid \| src\/s\.py \| bytes 5 -> 4/);
  assert.match(list.children[0].textContent, /digest 25ada7bca033/);
  assert.match(list.children[0].textContent, /sha256 9f56e761d79b -> 98752ee28d54/);
  assert.ok(!list.children[0].textContent.includes('25ada7bca03323eab54c9b31f86bdad0'));
  assert.match(list.children[1].textContent, /review-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \| invalid \(incomplete_or_extra_files\)/);

  // 2) Project switch while RPC is in flight discards result and clears list
  const pending2 = browse.onclick();
  assert.equal(browse.disabled, true);
  active = 'proj-2';
  resolvePending.resolve(
    sampleEnvelope({ storagePresent: true, items: [sampleValidItem()], validCount: 1, invalidCount: 0 })
  );
  await pending2;
  assert.equal(browse.disabled, false);
  assert.equal(list.children.length, 0);
  assert.match(status.textContent, /Project changed\. Result discarded\./);

  // 3) Clicking while wrong project is selected refuses without RPC
  const prevCalls = calls.length;
  await browse.onclick();
  assert.equal(calls.length, prevCalls);
  assert.match(status.textContent, /Select the original project/);

  // 4) RPC error reports disabled/unavailable/canceled and re-enables button
  active = 'proj-1';
  const pending3 = browse.onclick();
  resolvePending.reject(new Error('Optional private review inventory is disabled'));
  await pending3;
  assert.equal(browse.disabled, false);
  assert.equal(list.children.length, 0);
  assert.match(status.textContent, /disabled, unavailable, or canceled/i);

  // 5) Tampered payload from daemon (e.g. extra storage_path) is rejected and never rendered
  const pending4 = browse.onclick();
  resolvePending.resolve({
    ...sampleEnvelope({ storagePresent: true, items: [sampleValidItem()], validCount: 1, invalidCount: 0 }),
    storage_path: '/home/owner/.navi/proposal-reviews',
  });
  await pending4;
  assert.equal(browse.disabled, false);
  assert.equal(list.children.length, 0);
  assert.match(status.textContent, /disabled, unavailable, or canceled/i);
});

let failed = 0;
for (const { name, fn } of tests) {
  try {
    await fn();
    console.log(`ok - ${name}`);
  } catch (err) {
    failed += 1;
    console.error(`FAIL - ${name}`);
    console.error(err);
  }
}
if (failed) process.exit(1);
console.log(`Passed ${tests.length} UI review inventory tests.`);