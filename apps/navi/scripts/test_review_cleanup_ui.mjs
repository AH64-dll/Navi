/**
 * Unit tests for station.js private review cleanup validator and DOM controls.
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
const STATION_PATH = fs.existsSync(path.resolve(__dirname, '../ui/station.js'))
  ? path.resolve(__dirname, '../ui/station.js')
  : path.resolve(__dirname, '../apps/navi/ui/station.js');
const stationSource = fs.readFileSync(STATION_PATH, 'utf8');

const START_MARKER = '/* review-inventory:start */';
const END_MARKER = '/* review-inventory:end */';
const startIdx = stationSource.indexOf(START_MARKER);
const endIdx = stationSource.indexOf(END_MARKER);
assert.ok(startIdx !== -1 && endIdx > startIdx, 'review-inventory block markers missing in station.js');
const blockSource = stationSource.slice(startIdx, endIdx + END_MARKER.length);

const FIXED_REASONS = [
  'not_found',
  'digest_mismatch',
  'invalid_entry_type',
  'invalid_permissions',
  'incomplete_or_extra_files',
  'verification_failed',
  'directory_changed',
  'canceled',
  'timed_out',
  'invalid_request',
  'invalid_storage',
];

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
      this.value = '';
      this.onclick = null;
      this.oninput = null;
      this.onchange = null;
      this._textContent = '';
    }
    get firstChild() {
      return this.children[0] || null;
    }
    get textContent() {
      return this._textContent + this.children.map((c) => c.textContent).join('');
    }
    set textContent(v) {
      this.children = [];
      this._textContent = String(v);
    }
    appendChild(child) {
      this.children.push(child);
      return child;
    }
    removeChild(child) {
      const idx = this.children.indexOf(child);
      if (idx !== -1) this.children.splice(idx, 1);
      return child;
    }
    setAttribute(name, value) {
      this.attributes[name] = String(value);
    }
    getAttribute(name) {
      return Object.prototype.hasOwnProperty.call(this.attributes, name)
        ? this.attributes[name]
        : null;
    }
  }
  return {
    createElement(tag) {
      return new FakeElement(tag);
    },
  };
}

function findByTag(root, tag) {
  const out = [];
  const want = tag.toUpperCase();
  const walk = (el) => {
    if (!el) return;
    if (el.tagName === want) out.push(el);
    for (const c of el.children || []) walk(c);
  };
  walk(root);
  return out;
}

function createHarness({ projectId = 'proj-1', rpcHandler } = {}) {
  let currentProject = projectId;
  const calls = [];
  const document = makeFakeDocument();
  const context = vm.createContext({
    document,
    TextEncoder,
    activeProjectId: () => currentProject,
    rpc: async (op, params) => {
      calls.push({ op, params: JSON.parse(JSON.stringify(params)) });
      if (!rpcHandler) throw new Error('unexpected rpc');
      return rpcHandler(op, params);
    },
  });
  vm.runInContext(
    `${blockSource}\n;globalThis.__exports={validateReviewInventoryResult,validateReviewCleanupResult,renderReviewInventory,reviewInventoryControls,REVIEW_CLEANUP_REASONS,REVIEW_CLEANUP_REASON_TEXT};`,
    context,
    { filename: 'station.js' },
  );
  return {
    api: context.__exports,
    calls,
    setProject(id) {
      currentProject = id;
    },
  };
}

function makeValidItem(ch, origPath = 'src/app.py') {
  const review_id = `review-${ch.repeat(32)}`;
  const proposal_digest = ch.repeat(64);
  return {
    review_id,
    valid: true,
    status: 'valid',
    applied: false,
    requires_fresh_validation: true,
    original_path: origPath,
    proposal_digest,
    before_sha256: '1'.repeat(64),
    after_sha256: '2'.repeat(64),
    before_bytes: 10,
    after_bytes: 12,
    files: ['before.txt', 'proposed-after.txt', 'review.json'],
  };
}

function makeInvalidItem(ch, reason = 'incomplete_or_extra_files') {
  return {
    review_id: `review-${ch.repeat(32)}`,
    valid: false,
    status: 'invalid',
    reason,
    applied: false,
  };
}

function makeInventory(items, storagePresent = true) {
  const valid_count = items.filter((i) => i.valid).length;
  const invalid_count = items.length - valid_count;
  return {
    schema: 'navi-review-inventory-v1',
    applied: false,
    source_contents_exposed: false,
    storage_present: storagePresent,
    inspected_count: items.length,
    valid_count,
    invalid_count,
    max_inspected_children: 64,
    max_valid_records: 32,
    items,
    notice: 'Metadata inventory of private inert review workspaces only.',
  };
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

async function runTests() {
  // 1. Static checks: budget present, textContent only, no localStorage, no delete-all.
  assert.match(
    stationSource,
    /\bpi_review_cleanup\s*:\s*15000\b/,
    'budgets map must include pi_review_cleanup:15000',
  );
  for (const forbidden of [
    'innerHTML',
    'outerHTML',
    'insertAdjacentHTML',
    'localStorage',
    'sessionStorage',
    'delete all',
    'Delete all',
    'proposal-reviews',
  ]) {
    assert.ok(!blockSource.includes(forbidden), `forbidden token in review block: ${forbidden}`);
  }

  // 2. Validator strictness
  {
    const { api } = createHarness();
    const rid = `review-${'a'.repeat(32)}`;
    const otherRid = `review-${'b'.repeat(32)}`;

    const okDel = api.validateReviewCleanupResult(
      { schema: 'navi-review-cleanup-v1', deleted: true, review_id: rid },
      rid,
    );
    deepEq(okDel, { schema: 'navi-review-cleanup-v1', deleted: true, review_id: rid });

    for (const reason of FIXED_REASONS) {
      const okRefuse = api.validateReviewCleanupResult(
        { schema: 'navi-review-cleanup-v1', deleted: false, review_id: rid, reason },
        rid,
      );
      deepEq(okRefuse, { schema: 'navi-review-cleanup-v1', deleted: false, review_id: rid, reason });
    }

    const badReplies = [
      null,
      [],
      'nope',
      { schema: 'wrong', deleted: true, review_id: rid },
      { schema: 'navi-review-cleanup-v1', deleted: true, review_id: otherRid },
      { schema: 'navi-review-cleanup-v1', deleted: false, review_id: null, reason: 'invalid_request' },
      { schema: 'navi-review-cleanup-v1', deleted: true, review_id: rid, reason: 'not_found' },
      { schema: 'navi-review-cleanup-v1', deleted: false, review_id: rid },
      { schema: 'navi-review-cleanup-v1', deleted: false, review_id: rid, reason: 'freeform' },
      { schema: 'navi-review-cleanup-v1', deleted: false, review_id: rid, reason: 123 },
      { schema: 'navi-review-cleanup-v1', deleted: 'true', review_id: rid },
      { schema: 'navi-review-cleanup-v1', deleted: true, review_id: rid, storage_path: '/tmp' },
      { schema: 'navi-review-cleanup-v1', deleted: false, review_id: rid, reason: 'not_found', extra: 1 },
    ];
    for (const bad of badReplies) {
      assert.throws(() => api.validateReviewCleanupResult(bad, rid), `expected reject: ${JSON.stringify(bad)}`);
    }
    assert.throws(
      () => api.validateReviewCleanupResult({ schema: 'navi-review-cleanup-v1', deleted: true, review_id: '../x' }, '../x'),
      'expected invalid expectedReviewId rejection',
    );
  }

  // 3. Valid rows get delete controls; invalid rows do NOT; strict typed phrase gating.
  {
    const itemA = makeValidItem('a');
    const itemBad = makeInvalidItem('b');
    const { api, calls } = createHarness({
      rpcHandler: (op) => {
        if (op === 'pi_review_inventory') return makeInventory([itemA, itemBad]);
        throw new Error(`unexpected ${op}`);
      },
    });
    const [browse, status, list] = api.reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();

    assert.equal(list.children.length, 2);
    const validLi = list.children[0];
    const invalidLi = list.children[1];

    // Valid row has one text input and one disabled Delete button.
    const validInputs = findByTag(validLi, 'INPUT').filter((e) => e.className === 'review-cleanup-confirm');
    const validBtns = findByTag(validLi, 'BUTTON').filter((e) => e.className === 'review-cleanup-delete');
    assert.equal(validInputs.length, 1);
    assert.equal(validBtns.length, 1);
    const [input] = validInputs;
    const [delBtn] = validBtns;
    assert.equal(input.value, '');
    assert.equal(delBtn.disabled, true);
    assert.equal(delBtn.textContent, 'Delete this private review copy');

    // Invalid row has NO inputs and NO buttons.
    assert.equal(findByTag(invalidLi, 'INPUT').length, 0);
    assert.equal(findByTag(invalidLi, 'BUTTON').length, 0);

    // Wrong confirmation strings keep Delete disabled.
    const exactPhrase = `delete private review ${itemA.review_id}`;
    for (const wrong of [
      '',
      'yes',
      'delete',
      'delete private review',
      `delete private review ${itemBad.review_id}`,
      `Delete private review ${itemA.review_id}`,
      ` ${exactPhrase}`,
      `${exactPhrase} `,
    ]) {
      input.value = wrong;
      input.oninput();
      assert.equal(delBtn.disabled, true, `must stay disabled for ${JSON.stringify(wrong)}`);
    }

    // Even forcing delBtn.disabled=false with wrong phrase must NOT call rpc and clears input.
    input.value = 'wrong';
    delBtn.disabled = false;
    await delBtn.onclick();
    assert.equal(calls.length, 1); // only the initial browse call
    assert.equal(input.value, '');
    assert.equal(delBtn.disabled, true);

    // Exact phrase enables button; changing it disables again.
    input.value = exactPhrase;
    input.oninput();
    assert.equal(delBtn.disabled, false);
    input.value = `${exactPhrase}!`;
    input.oninput();
    assert.equal(delBtn.disabled, true);
    assert.ok(!status.textContent.includes('Deleted'));
  }

  // 4. Valid delete uses validated row values (never DOM text), clears input, shows exact success text, auto-re-runs inventory.
  {
    const itemA = makeValidItem('a', 'src/first.py');
    const itemB = makeValidItem('b', 'src/second.py');
    let invCallCount = 0;
    const { api, calls } = createHarness({
      rpcHandler: (op) => {
        if (op === 'pi_review_inventory') {
          invCallCount += 1;
          return invCallCount === 1 ? makeInventory([itemA, itemB]) : makeInventory([itemB]);
        }
        if (op === 'pi_review_cleanup') {
          return { schema: 'navi-review-cleanup-v1', deleted: true, review_id: itemA.review_id };
        }
        throw new Error(`unexpected ${op}`);
      },
    });

    const [browse, status, list] = api.reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();

    const liA = list.children[0];
    const [inputA] = findByTag(liA, 'INPUT');
    const [delBtnA] = findByTag(liA, 'BUTTON');

    // Tamper with DOM summary text to verify review_id and proposal_digest never come from DOM.
    liA._textContent = `review-${'f'.repeat(32)} | valid | hacked.py | digest ${'f'.repeat(12)}`;

    const phrase = `delete private review ${itemA.review_id}`;
    inputA.value = phrase;
    inputA.oninput();
    assert.equal(delBtnA.disabled, false);

    await delBtnA.onclick();

    assert.equal(calls.length, 3);
    assert.equal(calls[0].op, 'pi_review_inventory');
    assert.equal(calls[1].op, 'pi_review_cleanup');
    deepEq(calls[1].params, {
      project_id: 'proj-1',
      review_id: itemA.review_id,
      proposal_digest: itemA.proposal_digest,
      confirm: phrase,
    });
    assert.deepEqual(Object.keys(calls[1].params).sort(), [
      'confirm',
      'project_id',
      'proposal_digest',
      'review_id',
    ]);
    assert.equal(calls[2].op, 'pi_review_inventory');
    deepEq(calls[2].params, { project_id: 'proj-1' });

    assert.equal(inputA.value, '');
    assert.ok(
      status.textContent.startsWith('Deleted this private review copy only. No project file changed.'),
      status.textContent,
    );
    assert.equal(list.children.length, 1);
    assert.ok(list.children[0].textContent.includes(itemB.review_id));
    assert.equal(browse.disabled, false);
    assert.equal(findByTag(list.children[0], 'BUTTON')[0].disabled, true);
  }

  // 5. One delete in flight at a time disables ALL delete buttons and Browse.
  {
    const itemA = makeValidItem('a');
    const itemB = makeValidItem('b');
    const cleanupGate = deferred();
    let invCount = 0;
    const { api, calls } = createHarness({
      rpcHandler: (op) => {
        if (op === 'pi_review_inventory') {
          invCount += 1;
          return invCount === 1 ? makeInventory([itemA, itemB]) : makeInventory([itemB]);
        }
        if (op === 'pi_review_cleanup') return cleanupGate.promise;
        throw new Error(`unexpected ${op}`);
      },
    });

    const [browse, _status, list] = api.reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();

    const [inputA] = findByTag(list.children[0], 'INPUT');
    const [delBtnA] = findByTag(list.children[0], 'BUTTON');
    const [inputB] = findByTag(list.children[1], 'INPUT');
    const [delBtnB] = findByTag(list.children[1], 'BUTTON');

    inputA.value = `delete private review ${itemA.review_id}`;
    inputA.oninput();
    inputB.value = `delete private review ${itemB.review_id}`;
    inputB.oninput();
    assert.equal(delBtnA.disabled, false);
    assert.equal(delBtnB.disabled, false);

    const pending = delBtnA.onclick();
    assert.equal(browse.disabled, true);
    assert.equal(delBtnA.disabled, true);
    assert.equal(delBtnB.disabled, true);

    // Typing in row B or clicking Browse/row B while in flight must stay disabled and not invoke rpc.
    inputB.oninput();
    assert.equal(delBtnB.disabled, true);
    await delBtnB.onclick();
    await browse.onclick();
    assert.equal(calls.filter((c) => c.op === 'pi_review_cleanup').length, 1);
    assert.equal(calls.filter((c) => c.op === 'pi_review_inventory').length, 1);

    cleanupGate.resolve({ schema: 'navi-review-cleanup-v1', deleted: true, review_id: itemA.review_id });
    await pending;
    assert.equal(browse.disabled, false);
  }

  // 6. deleted:false shows a fixed human sentence for every reason, clears input, does not re-run inventory.
  for (const reason of FIXED_REASONS) {
    const itemA = makeValidItem('c');
    const { api, calls } = createHarness({
      rpcHandler: (op) => {
        if (op === 'pi_review_inventory') return makeInventory([itemA]);
        if (op === 'pi_review_cleanup') {
          return { schema: 'navi-review-cleanup-v1', deleted: false, review_id: itemA.review_id, reason };
        }
        throw new Error(`unexpected ${op}`);
      },
    });
    const [browse, status, list] = api.reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();
    const [input] = findByTag(list.children[0], 'INPUT');
    const [delBtn] = findByTag(list.children[0], 'BUTTON');
    input.value = `delete private review ${itemA.review_id}`;
    input.oninput();
    await delBtn.onclick();

    assert.equal(calls.length, 2, `should not auto-refresh on deleted:false (${reason})`);
    assert.equal(input.value, '');
    assert.equal(delBtn.disabled, true);
    assert.equal(browse.disabled, false);
    assert.ok(status.textContent.length > 10, `missing message for ${reason}`);
    assert.ok(!status.textContent.includes('Deleted this private review copy only'), reason);
    assert.ok(!status.textContent.includes('/'), `must not show paths (${reason})`);
    if (reason === 'canceled' || reason === 'timed_out' || reason === 'directory_changed') {
      assert.ok(status.textContent.includes('Outcome unknown. Browse again before retrying.'), reason);
      assert.equal(list.children.length, 0);
    }
  }

  // 7. Error / timeout / disconnect / invalid reply -> exact unknown-outcome message, never echo paths.
  for (const mode of ['timeout', 'disconnect', 'invalid_reply']) {
    const itemA = makeValidItem('d');
    const { api, calls } = createHarness({
      rpcHandler: (op) => {
        if (op === 'pi_review_inventory') return makeInventory([itemA]);
        if (op === 'pi_review_cleanup') {
          if (mode === 'timeout') throw new Error('Timed out at /home/owner/.navi/proposal-reviews');
          if (mode === 'disconnect') throw new Error('Socket disconnected');
          return {
            schema: 'navi-review-cleanup-v1',
            deleted: true,
            review_id: itemA.review_id,
            storage_path: '/home/owner/.navi/proposal-reviews',
          };
        }
        throw new Error(`unexpected ${op}`);
      },
    });
    const [browse, status, list] = api.reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();
    const [input] = findByTag(list.children[0], 'INPUT');
    const [delBtn] = findByTag(list.children[0], 'BUTTON');
    input.value = `delete private review ${itemA.review_id}`;
    input.oninput();
    await delBtn.onclick();

    assert.equal(status.textContent, 'Outcome unknown. Browse again before retrying.');
    assert.ok(!status.textContent.includes('/home/owner'));
    assert.equal(input.value, '');
    assert.equal(list.children.length, 0);
    assert.equal(browse.disabled, false);
    assert.equal(calls.filter((c) => c.op === 'pi_review_inventory').length, 1);
  }

  // 8. Project switch before click and mid-flight -> exact unknown-outcome message, never claim success.
  {
    const itemA = makeValidItem('e');
    const { api, calls, setProject } = createHarness({
      projectId: 'proj-1',
      rpcHandler: (op) => {
        if (op === 'pi_review_inventory') return makeInventory([itemA]);
        throw new Error(`unexpected ${op}`);
      },
    });
    const [browse, status, list] = api.reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();
    const [input] = findByTag(list.children[0], 'INPUT');
    const [delBtn] = findByTag(list.children[0], 'BUTTON');
    input.value = `delete private review ${itemA.review_id}`;
    input.oninput();

    setProject('proj-2');
    await delBtn.onclick();
    assert.equal(calls.filter((c) => c.op === 'pi_review_cleanup').length, 0);
    assert.equal(status.textContent, 'Outcome unknown. Browse again before retrying.');
    assert.equal(input.value, '');
    assert.equal(list.children.length, 0);
  }
  {
    const itemA = makeValidItem('f');
    const gate = deferred();
    const { api, calls, setProject } = createHarness({
      projectId: 'proj-1',
      rpcHandler: (op) => {
        if (op === 'pi_review_inventory') return makeInventory([itemA]);
        if (op === 'pi_review_cleanup') return gate.promise;
        throw new Error(`unexpected ${op}`);
      },
    });
    const [browse, status, list] = api.reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();
    const [input] = findByTag(list.children[0], 'INPUT');
    const [delBtn] = findByTag(list.children[0], 'BUTTON');
    input.value = `delete private review ${itemA.review_id}`;
    input.oninput();

    const pending = delBtn.onclick();
    setProject('proj-2');
    gate.resolve({ schema: 'navi-review-cleanup-v1', deleted: true, review_id: itemA.review_id });
    await pending;

    assert.equal(status.textContent, 'Outcome unknown. Browse again before retrying.');
    assert.equal(input.value, '');
    assert.equal(list.children.length, 0);
    assert.equal(browse.disabled, false);
    assert.equal(calls.filter((c) => c.op === 'pi_review_inventory').length, 1);
  }

  // 9. Auto-refresh failure after validated delete preserves success message and clears stale list.
  {
    const itemA = makeValidItem('1');
    let invCalls = 0;
    const { api } = createHarness({
      rpcHandler: (op) => {
        if (op === 'pi_review_inventory') {
          invCalls += 1;
          if (invCalls === 1) return makeInventory([itemA]);
          throw new Error('inventory temporarily unavailable');
        }
        if (op === 'pi_review_cleanup') {
          return { schema: 'navi-review-cleanup-v1', deleted: true, review_id: itemA.review_id };
        }
        throw new Error(`unexpected ${op}`);
      },
    });
    const [browse, status, list] = api.reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();
    const [input] = findByTag(list.children[0], 'INPUT');
    const [delBtn] = findByTag(list.children[0], 'BUTTON');
    input.value = `delete private review ${itemA.review_id}`;
    input.oninput();
    await delBtn.onclick();

    assert.ok(
      status.textContent.startsWith('Deleted this private review copy only. No project file changed.'),
      status.textContent,
    );
    assert.equal(list.children.length, 0);
    assert.equal(browse.disabled, false);
  }

  console.log('test_review_cleanup_ui.mjs: all checks passed');
}

runTests().catch((err) => {
  console.error(err);
  process.exit(1);
});