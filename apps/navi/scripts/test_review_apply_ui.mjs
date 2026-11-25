import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, '..');
const stationPath = path.join(repoRoot, 'ui', 'station.js');
const stationSource = fs.readFileSync(stationPath, 'utf8');

const START_MARKER = '/* review-inventory:start */';
const END_MARKER = '/* review-inventory:end */';

function extractReviewBlock(source) {
  const start = source.indexOf(START_MARKER);
  const end = source.indexOf(END_MARKER);
  assert.notEqual(start, -1, 'missing review-inventory start marker');
  assert.notEqual(end, -1, 'missing review-inventory end marker');
  assert.ok(end > start, 'review-inventory end marker must follow start marker');
  return source.slice(start, end + END_MARKER.length);
}

const reviewBlock = extractReviewBlock(stationSource);

const VALID_ID_1 = 'review-0123456789abcdef0123456789abcdef';
const VALID_ID_2 = 'review-fedcba9876543210fedcba9876543210';
const INVALID_ROW_ID = 'review-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa';
const DIGEST_1 = 'a'.repeat(64);
const DIGEST_2 = 'b'.repeat(64);

const EXPECTED_SUCCESS_TEXT =
  'Applied this review to the project file. The private review copy was kept.';
const EXPECTED_UNKNOWN_TEXT =
  'Outcome unknown. The project file may or may not have changed. Browse again and check the file before retrying.';

const REFUSAL_REASONS = [
  'invalid_request',
  'not_found',
  'digest_mismatch',
  'verification_failed',
  'before_mismatch',
  'invalid_entry_type',
  'invalid_permissions',
  'too_large',
  'invalid_storage',
];

const UNKNOWN_REASONS = ['directory_changed', 'canceled', 'timed_out'];
const ALL_REASONS = [...REFUSAL_REASONS, ...UNKNOWN_REASONS];

function createFakeElement(tagName) {
  return {
    tagName: tagName.toUpperCase(),
    className: '',
    textContent: '',
    type: '',
    value: '',
    title: '',
    disabled: false,
    oninput: null,
    onchange: null,
    onclick: null,
    parentNode: null,
    children: [],
    attributes: new Map(),
    get firstChild() {
      return this.children.length ? this.children[0] : null;
    },
    setAttribute(name, value) {
      this.attributes.set(String(name), String(value));
    },
    getAttribute(name) {
      return this.attributes.has(String(name)) ? this.attributes.get(String(name)) : null;
    },
    appendChild(child) {
      child.parentNode = this;
      this.children.push(child);
      return child;
    },
    removeChild(child) {
      const idx = this.children.indexOf(child);
      if (idx === -1) throw new Error('Child not found');
      this.children.splice(idx, 1);
      child.parentNode = null;
      return child;
    },
  };
}

function createFakeDocument() {
  return {
    createElement(tagName) {
      return createFakeElement(tagName);
    },
  };
}

function findAllByClass(root, className) {
  const out = [];
  const visit = (node) => {
    if (!node) return;
    if (node.className === className) out.push(node);
    for (const child of node.children || []) visit(child);
  };
  visit(root);
  return out;
}

function findOneByClass(root, className) {
  const matches = findAllByClass(root, className);
  assert.equal(matches.length, 1, `expected 1 element with class ${className}, found ${matches.length}`);
  return matches[0];
}

function setInputValue(input, value) {
  input.value = value;
  if (typeof input.oninput === 'function') input.oninput();
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

function makeValidItem(reviewId = VALID_ID_1, digest = DIGEST_1, originalPath = 'src/app.js') {
  return {
    review_id: reviewId,
    valid: true,
    status: 'valid',
    applied: false,
    requires_fresh_validation: true,
    original_path: originalPath,
    proposal_digest: digest,
    before_sha256: '1'.repeat(64),
    after_sha256: '2'.repeat(64),
    before_bytes: 100,
    after_bytes: 120,
    files: ['before.txt', 'proposed-after.txt', 'review.json'],
  };
}

function makeInvalidItem(reviewId = INVALID_ROW_ID, reason = 'verification_failed') {
  return {
    review_id: reviewId,
    valid: false,
    status: 'invalid',
    reason,
    applied: false,
  };
}

function makeInventory(items) {
  const validCount = items.filter((item) => item.valid).length;
  const invalidCount = items.length - validCount;
  return {
    schema: 'navi-review-inventory-v1',
    applied: false,
    source_contents_exposed: false,
    storage_present: true,
    inspected_count: items.length,
    valid_count: validCount,
    invalid_count: invalidCount,
    max_inspected_children: 64,
    max_valid_records: 32,
    items,
    notice: 'Metadata only.',
  };
}

async function previewAll(root) {
  for (const b of findAllByClass(root, 'review-preview-button')) await b.onclick();
}

function loadHarness({ rpc, activeProjectId }) {
  const document = createFakeDocument();
  const context = vm.createContext({
    document,
    TextEncoder,
    rpc: async (method, params) => {
      if (method === 'pi_review_preview') {
        return { schema: 'navi-review-preview-v1', review_id: params.review_id, ok: true, stale: false, truncated: false, diff_lines: 2, before_bytes: 3, after_bytes: 3, diff: '-a\n+b\n' };
      }
      return rpc(method, params);
    },
    activeProjectId,
  });
  vm.runInContext(reviewBlock, context, { filename: 'ui/station.js#review-inventory' });
  const constants = vm.runInContext(
    '({ REVIEW_APPLY_SCHEMA, REVIEW_APPLY_REASONS, REVIEW_APPLY_REASON_TEXT, REVIEW_APPLY_SUCCESS_TEXT, REVIEW_APPLY_UNKNOWN_TEXT, REVIEW_CLEANUP_SUCCESS_TEXT })',
    context
  );
  return {
    context,
    document,
    ...constants,
    reviewInventoryControls: context.reviewInventoryControls,
    validateReviewApplyResult: context.validateReviewApplyResult,
    buildReviewApplyRow: context.buildReviewApplyRow,
    renderReviewInventory: context.renderReviewInventory,
  };
}

async function testBudgetAndStaticGuards() {
  assert.match(stationSource, /\bpi_review_apply\s*:\s*20000\b/, 'budget table must include pi_review_apply:20000');
  assert.doesNotMatch(reviewBlock, /\binnerHTML\b/, 'review block must not use innerHTML');
  assert.doesNotMatch(reviewBlock, /\blocalStorage\b/, 'review block must not use localStorage');
}

async function testValidatorAcceptAndReject() {
  const { validateReviewApplyResult, REVIEW_APPLY_REASONS } = loadHarness({
    rpc: async () => {
      throw new Error('unexpected rpc');
    },
    activeProjectId: () => 'proj-1',
  });

  assert.equal(REVIEW_APPLY_REASONS.size, 12);
  for (const reason of ALL_REASONS) {
    assert.ok(REVIEW_APPLY_REASONS.has(reason), `missing allowed reason: ${reason}`);
  }

  const okTrue = validateReviewApplyResult(
    { schema: 'navi-review-apply-v1', applied: true, review_id: VALID_ID_1 },
    VALID_ID_1
  );
  assert.deepStrictEqual(JSON.parse(JSON.stringify(okTrue)), {
    schema: 'navi-review-apply-v1',
    applied: true,
    review_id: VALID_ID_1,
  });

  for (const reason of ALL_REASONS) {
    const okFalse = validateReviewApplyResult(
      { schema: 'navi-review-apply-v1', applied: false, review_id: VALID_ID_1, reason },
      VALID_ID_1
    );
    assert.deepStrictEqual(JSON.parse(JSON.stringify(okFalse)), {
      schema: 'navi-review-apply-v1',
      applied: false,
      review_id: VALID_ID_1,
      reason,
    });
  }

  const inherited = Object.create({
    schema: 'navi-review-apply-v1',
    applied: true,
    review_id: VALID_ID_1,
  });

  const nonEnumReason = {
    schema: 'navi-review-apply-v1',
    applied: true,
    review_id: VALID_ID_1,
  };
  Object.defineProperty(nonEnumReason, 'reason', { value: 'not_found', enumerable: false });

  const invalidCases = [
    [null, VALID_ID_1],
    [undefined, VALID_ID_1],
    ['string', VALID_ID_1],
    [42, VALID_ID_1],
    [[], VALID_ID_1],
    [new Date(), VALID_ID_1],
    [inherited, VALID_ID_1],
    [nonEnumReason, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: true, review_id: VALID_ID_1 }, 'bad-id'],
    [{ schema: 'navi-review-apply-v1', applied: true, review_id: 'review-0123456789ABCDEF0123456789ABCDEF' }, 'review-0123456789ABCDEF0123456789ABCDEF'],
    [{ schema: 'navi-review-cleanup-v1', applied: true, review_id: VALID_ID_1 }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v2', applied: true, review_id: VALID_ID_1 }, VALID_ID_1],
    [{ applied: true, review_id: VALID_ID_1 }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', review_id: VALID_ID_1 }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: true }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: 'true', review_id: VALID_ID_1 }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: 1, review_id: VALID_ID_1 }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: true, review_id: VALID_ID_2 }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: true, review_id: VALID_ID_1, reason: 'not_found' }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: true, review_id: VALID_ID_1, reason: undefined }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: true, review_id: VALID_ID_1, extra: 1 }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: false, review_id: VALID_ID_1 }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: false, review_id: VALID_ID_1, other: 'not_found' }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: false, review_id: VALID_ID_1, reason: 'not_found', extra: 1 }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: false, review_id: VALID_ID_1, reason: 'unknown_reason' }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: false, review_id: VALID_ID_1, reason: 'incomplete_or_extra_files' }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: false, review_id: VALID_ID_1, reason: '' }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: false, review_id: VALID_ID_1, reason: null }, VALID_ID_1],
    [{ schema: 'navi-review-apply-v1', applied: false, review_id: VALID_ID_1, reason: 42 }, VALID_ID_1],
  ];

  for (const [raw, expectedId] of invalidCases) {
    assert.throws(
      () => validateReviewApplyResult(raw, expectedId),
      /Invalid review apply/,
      `expected rejection for ${JSON.stringify(raw)} with expectedId=${expectedId}`
    );
  }
}

async function testApplyRowOnlyOnValidRowsAndConfirmGating() {
  const calls = [];
  const { reviewInventoryControls } = loadHarness({
    rpc: async (method, params) => {
      calls.push({ method, params });
      if (method === 'pi_review_inventory') {
        return makeInventory([makeValidItem(VALID_ID_1, DIGEST_1), makeInvalidItem(INVALID_ROW_ID)]);
      }
      throw new Error(`unexpected rpc ${method}`);
    },
    activeProjectId: () => 'proj-1',
  });

  const [browse, , list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
  await browse.onclick();

  assert.equal(list.children.length, 2);
  const validLi = list.children[0];
  const invalidLi = list.children[1];

  assert.equal(findAllByClass(validLi, 'review-cleanup-controls').length, 1);
  assert.equal(findAllByClass(validLi, 'review-apply-controls').length, 1);
  assert.equal(findAllByClass(invalidLi, 'review-cleanup-controls').length, 0);
  assert.equal(findAllByClass(invalidLi, 'review-apply-controls').length, 0);
  assert.equal(invalidLi.children.length, 0);

  const applyInput = findOneByClass(validLi, 'review-apply-confirm');
  const applyBtn = findOneByClass(validLi, 'review-apply-button');

  assert.equal(applyInput.type, 'text');
  assert.equal(applyInput.getAttribute('autocomplete'), 'off');
  assert.equal(applyInput.value, '');
  assert.equal(applyBtn.type, 'button');
  assert.equal(applyBtn.textContent, 'Apply this review to the project file');
  assert.match(applyBtn.title, /project file/i);
  assert.match(applyBtn.title, /private review copy/i);
  assert.match(applyBtn.title, /kept/i);
  assert.equal(applyBtn.disabled, true);

  const exactConfirm = `apply private review ${VALID_ID_1}`;
  for (const wrong of [
    '',
    `delete private review ${VALID_ID_1}`,
    `apply private review ${VALID_ID_2}`,
    ` ${exactConfirm}`,
    `${exactConfirm} `,
    exactConfirm.slice(0, -1),
    exactConfirm.toUpperCase(),
  ]) {
    setInputValue(applyInput, wrong);
    assert.equal(applyBtn.disabled, true, `apply button must remain disabled for "${wrong}"`);
    await applyBtn.onclick();
  }
  assert.equal(calls.length, 1, 'disabled clicks must not invoke rpc');

  await previewAll(list);
  setInputValue(applyInput, exactConfirm);
  assert.equal(applyBtn.disabled, false, 'apply button must enable on exact confirmation phrase');

  setInputValue(applyInput, `${exactConfirm}!`);
  assert.equal(applyBtn.disabled, true, 'apply button must disable again when confirmation no longer matches');
}

async function testSuccessPathStrictKeysAndReInventory() {
  const calls = [];
  let inventoryCalls = 0;
  const { reviewInventoryControls } = loadHarness({
    rpc: async (method, params) => {
      calls.push({ method, params });
      if (method === 'pi_review_inventory') {
        inventoryCalls += 1;
        return makeInventory([makeValidItem(VALID_ID_1, DIGEST_1)]);
      }
      if (method === 'pi_review_apply') {
        return {
          schema: 'navi-review-apply-v1',
          applied: true,
          review_id: VALID_ID_1,
        };
      }
      throw new Error(`unexpected rpc ${method}`);
    },
    activeProjectId: () => 'proj-1',
  });

  const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
  await browse.onclick();

  const applyInput = findOneByClass(list, 'review-apply-confirm');
  const applyBtn = findOneByClass(list, 'review-apply-button');
  const exactConfirm = `apply private review ${VALID_ID_1}`;
  await previewAll(list);
  setInputValue(applyInput, exactConfirm);
  assert.equal(applyBtn.disabled, false);

  await applyBtn.onclick();

  assert.equal(calls.length, 3);
  assert.equal(calls[0].method, 'pi_review_inventory');
  assert.equal(calls[1].method, 'pi_review_apply');
  assert.equal(calls[2].method, 'pi_review_inventory');
  assert.equal(inventoryCalls, 2);

  const applyParams = calls[1].params;
  assert.deepEqual(Object.keys(applyParams).sort(), [
    'confirm',
    'project_id',
    'proposal_digest',
    'review_id',
  ]);
  assert.deepStrictEqual(JSON.parse(JSON.stringify(applyParams)), {
    project_id: 'proj-1',
    review_id: VALID_ID_1,
    proposal_digest: DIGEST_1,
    confirm: exactConfirm,
  });

  assert.equal(applyInput.value, '', 'old apply input must be cleared');
  assert.ok(
    status.textContent.startsWith(EXPECTED_SUCCESS_TEXT),
    `expected status to keep success text, got: ${status.textContent}`
  );
  assert.match(status.textContent, /Private review metadata: 1 inspected/);
  assert.equal(browse.disabled, false);
  assert.equal(list.children.length, 1);
  assert.equal(findOneByClass(list, 'review-apply-confirm').value, '');
  assert.equal(findOneByClass(list, 'review-apply-button').disabled, true);
}

async function testRefusalAndUnknownReasons() {
  for (const reason of REFUSAL_REASONS) {
    const calls = [];
    const { reviewInventoryControls, REVIEW_APPLY_REASON_TEXT } = loadHarness({
      rpc: async (method, params) => {
        calls.push({ method, params });
        if (method === 'pi_review_inventory') {
          return makeInventory([makeValidItem(VALID_ID_1, DIGEST_1)]);
        }
        if (method === 'pi_review_apply') {
          return {
            schema: 'navi-review-apply-v1',
            applied: false,
            review_id: VALID_ID_1,
            reason,
          };
        }
        throw new Error(`unexpected rpc ${method}`);
      },
      activeProjectId: () => 'proj-1',
    });

    const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();

    const applyInput = findOneByClass(list, 'review-apply-confirm');
    const applyBtn = findOneByClass(list, 'review-apply-button');
    await previewAll(list);
    setInputValue(applyInput, `apply private review ${VALID_ID_1}`);
    await applyBtn.onclick();

    assert.equal(calls.length, 2, `refusal ${reason} must not re-run inventory`);
    assert.equal(status.textContent, REVIEW_APPLY_REASON_TEXT[reason]);
    assert.match(status.textContent, /not changed/i, `refusal ${reason} must state file was not changed`);
    if (reason === 'before_mismatch') {
      assert.match(status.textContent, /changed since the review/i);
    }
    assert.equal(list.children.length, 1, `refusal ${reason} should keep rendered row`);
    assert.equal(applyInput.value, '', `input must clear after refusal ${reason}`);
    assert.equal(applyBtn.disabled, true);
    assert.equal(browse.disabled, false);
  }

  for (const reason of UNKNOWN_REASONS) {
    const calls = [];
    const { reviewInventoryControls } = loadHarness({
      rpc: async (method, params) => {
        calls.push({ method, params });
        if (method === 'pi_review_inventory') {
          return makeInventory([makeValidItem(VALID_ID_1, DIGEST_1)]);
        }
        if (method === 'pi_review_apply') {
          return {
            schema: 'navi-review-apply-v1',
            applied: false,
            review_id: VALID_ID_1,
            reason,
          };
        }
        throw new Error(`unexpected rpc ${method}`);
      },
      activeProjectId: () => 'proj-1',
    });

    const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();

    const applyInput = findOneByClass(list, 'review-apply-confirm');
    const applyBtn = findOneByClass(list, 'review-apply-button');
    await previewAll(list);
    setInputValue(applyInput, `apply private review ${VALID_ID_1}`);
    await applyBtn.onclick();

    assert.equal(calls.length, 2, `unknown reason ${reason} must not re-run inventory`);
    assert.equal(status.textContent, EXPECTED_UNKNOWN_TEXT);
    assert.equal(list.children.length, 0, `unknown reason ${reason} must clear panel`);
    assert.equal(applyInput.value, '', `input must clear after unknown reason ${reason}`);
    assert.equal(browse.disabled, false);
  }
}

async function testMutualExclusionAcrossApplyDeleteBrowse() {
  const calls = [];
  let applyGate = null;
  let deleteGate = null;
  let browseGate = null;

  const { reviewInventoryControls } = loadHarness({
    rpc: async (method, params) => {
      calls.push({ method, params });
      if (method === 'pi_review_inventory') {
        if (browseGate) return browseGate.promise;
        return makeInventory([
          makeValidItem(VALID_ID_1, DIGEST_1, 'src/a.js'),
          makeValidItem(VALID_ID_2, DIGEST_2, 'src/b.js'),
        ]);
      }
      if (method === 'pi_review_apply') {
        return applyGate.promise;
      }
      if (method === 'pi_review_cleanup') {
        return deleteGate.promise;
      }
      throw new Error(`unexpected rpc ${method}`);
    },
    activeProjectId: () => 'proj-1',
  });

  const [browse, , list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
  await browse.onclick();

  const row1 = list.children[0];
  const row2 = list.children[1];
  const delInput1 = findOneByClass(row1, 'review-cleanup-confirm');
  const delBtn1 = findOneByClass(row1, 'review-cleanup-delete');
  const applyInput1 = findOneByClass(row1, 'review-apply-confirm');
  const applyBtn1 = findOneByClass(row1, 'review-apply-button');
  const delInput2 = findOneByClass(row2, 'review-cleanup-confirm');
  const delBtn2 = findOneByClass(row2, 'review-cleanup-delete');
  const applyInput2 = findOneByClass(row2, 'review-apply-confirm');
  const applyBtn2 = findOneByClass(row2, 'review-apply-button');

  setInputValue(delInput1, `delete private review ${VALID_ID_1}`);
  await previewAll(list);
  setInputValue(applyInput1, `apply private review ${VALID_ID_1}`);
  setInputValue(delInput2, `delete private review ${VALID_ID_2}`);
  await previewAll(list);
  setInputValue(applyInput2, `apply private review ${VALID_ID_2}`);

  assert.equal(browse.disabled, false);
  assert.equal(delBtn1.disabled, false);
  assert.equal(applyBtn1.disabled, false);
  assert.equal(delBtn2.disabled, false);
  assert.equal(applyBtn2.disabled, false);

  // 1) Apply in flight blocks browse, delete, and another apply
  applyGate = deferred();
  const applyPromise = applyBtn1.onclick();
  assert.equal(browse.disabled, true);
  assert.equal(applyBtn1.disabled, true);
  assert.equal(delBtn1.disabled, true);
  assert.equal(applyBtn2.disabled, true);
  assert.equal(delBtn2.disabled, true);

  await browse.onclick();
  await delBtn1.onclick();
  await applyBtn2.onclick();
  await delBtn2.onclick();
  assert.equal(calls.length, 2, 'no concurrent rpc allowed while apply is in flight');

  applyGate.resolve({
    schema: 'navi-review-apply-v1',
    applied: false,
    review_id: VALID_ID_1,
    reason: 'digest_mismatch',
  });
  await applyPromise;

  assert.equal(applyInput1.value, '');
  assert.equal(applyBtn1.disabled, true);
  assert.equal(browse.disabled, false);
  assert.equal(delBtn1.disabled, false);
  assert.equal(applyBtn2.disabled, false);
  assert.equal(delBtn2.disabled, false);

  // 2) Delete in flight blocks apply and browse
  deleteGate = deferred();
  const deletePromise = delBtn1.onclick();
  assert.equal(browse.disabled, true);
  assert.equal(delBtn1.disabled, true);
  assert.equal(applyBtn2.disabled, true);
  assert.equal(delBtn2.disabled, true);

  await browse.onclick();
  await applyBtn2.onclick();
  assert.equal(calls.length, 3, 'no concurrent rpc allowed while delete is in flight');

  deleteGate.resolve({
    schema: 'navi-review-cleanup-v1',
    deleted: false,
    review_id: VALID_ID_1,
    reason: 'not_found',
  });
  await deletePromise;

  assert.equal(browse.disabled, false);
  assert.equal(applyBtn2.disabled, false);
  assert.equal(delBtn2.disabled, false);

  // 3) Browse in flight blocks apply and delete
  browseGate = deferred();
  const browsePromise = browse.onclick();
  assert.equal(browse.disabled, true);
  assert.equal(applyBtn2.disabled, true);
  assert.equal(delBtn2.disabled, true);

  await applyBtn2.onclick();
  await delBtn2.onclick();
  assert.equal(calls.length, 4, 'no concurrent rpc allowed while browse is in flight');

  browseGate.resolve(makeInventory([makeValidItem(VALID_ID_1, DIGEST_1)]));
  await browsePromise;
  assert.equal(browse.disabled, false);
  assert.equal(list.children.length, 1);
}

async function testUnknownOutcomeOnThrownErrorAndInvalidReply() {
  for (const mode of ['throw', 'invalid-reply']) {
    const calls = [];
    const { reviewInventoryControls } = loadHarness({
      rpc: async (method, params) => {
        calls.push({ method, params });
        if (method === 'pi_review_inventory') {
          return makeInventory([makeValidItem(VALID_ID_1, DIGEST_1)]);
        }
        if (method === 'pi_review_apply') {
          if (mode === 'throw') throw new Error('network failed');
          return { schema: 'navi-review-apply-v1', applied: true, review_id: VALID_ID_1, extra: 'bad' };
        }
        throw new Error(`unexpected rpc ${method}`);
      },
      activeProjectId: () => 'proj-1',
    });

    const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();

    const applyInput = findOneByClass(list, 'review-apply-confirm');
    const applyBtn = findOneByClass(list, 'review-apply-button');
    await previewAll(list);
    setInputValue(applyInput, `apply private review ${VALID_ID_1}`);

    await applyBtn.onclick();

    assert.equal(calls.length, 2, `${mode} must not trigger re-inventory`);
    assert.equal(list.children.length, 0, `${mode} must clear the panel`);
    assert.equal(status.textContent, EXPECTED_UNKNOWN_TEXT);
    assert.equal(applyInput.value, '', `${mode} must clear the typed input`);
    assert.equal(browse.disabled, false, `${mode} must release the in-flight lock`);
  }
}

async function testUnknownOutcomeOnProjectSwitchBeforeAndMidCall() {
  // Before call
  {
    let currentProject = 'proj-1';
    const calls = [];
    const { reviewInventoryControls } = loadHarness({
      rpc: async (method, params) => {
        calls.push({ method, params });
        if (method === 'pi_review_inventory') {
          return makeInventory([makeValidItem(VALID_ID_1, DIGEST_1)]);
        }
        throw new Error(`unexpected rpc ${method}`);
      },
      activeProjectId: () => currentProject,
    });

    const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();

    const applyInput = findOneByClass(list, 'review-apply-confirm');
    const applyBtn = findOneByClass(list, 'review-apply-button');
    await previewAll(list);
    setInputValue(applyInput, `apply private review ${VALID_ID_1}`);

    currentProject = 'proj-2';
    await applyBtn.onclick();

    assert.equal(calls.length, 1, 'project switch before call must not invoke pi_review_apply');
    assert.equal(list.children.length, 0);
    assert.equal(applyInput.value, '');
    assert.equal(status.textContent, EXPECTED_UNKNOWN_TEXT);
    assert.equal(browse.disabled, false);
  }

  // Mid-call
  {
    let currentProject = 'proj-1';
    const calls = [];
    const applyGate = deferred();
    const { reviewInventoryControls } = loadHarness({
      rpc: async (method, params) => {
        calls.push({ method, params });
        if (method === 'pi_review_inventory') {
          return makeInventory([makeValidItem(VALID_ID_1, DIGEST_1)]);
        }
        if (method === 'pi_review_apply') {
          return applyGate.promise;
        }
        throw new Error(`unexpected rpc ${method}`);
      },
      activeProjectId: () => currentProject,
    });

    const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
    await browse.onclick();

    const applyInput = findOneByClass(list, 'review-apply-confirm');
    const applyBtn = findOneByClass(list, 'review-apply-button');
    await previewAll(list);
    setInputValue(applyInput, `apply private review ${VALID_ID_1}`);

    const applyPromise = applyBtn.onclick();
    assert.equal(browse.disabled, true);

    currentProject = 'proj-2';
    applyGate.resolve({
      schema: 'navi-review-apply-v1',
      applied: true,
      review_id: VALID_ID_1,
    });
    await applyPromise;

    assert.equal(calls.length, 2, 'project switch mid-call must discard result and skip re-inventory');
    assert.equal(list.children.length, 0);
    assert.equal(applyInput.value, '');
    assert.equal(status.textContent, EXPECTED_UNKNOWN_TEXT);
    assert.equal(browse.disabled, false);
  }
}

async function testExistingDeleteFlowUnbroken() {
  const calls = [];
  let invCount = 0;
  const { reviewInventoryControls, REVIEW_CLEANUP_SUCCESS_TEXT } = loadHarness({
    rpc: async (method, params) => {
      calls.push({ method, params });
      if (method === 'pi_review_inventory') {
        invCount += 1;
        return makeInventory(invCount === 1 ? [makeValidItem(VALID_ID_1, DIGEST_1)] : []);
      }
      if (method === 'pi_review_cleanup') {
        return { schema: 'navi-review-cleanup-v1', deleted: true, review_id: VALID_ID_1 };
      }
      throw new Error(`unexpected rpc ${method}`);
    },
    activeProjectId: () => 'proj-1',
  });

  const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
  await browse.onclick();

  const delInput = findOneByClass(list, 'review-cleanup-confirm');
  const delBtn = findOneByClass(list, 'review-cleanup-delete');
  setInputValue(delInput, `delete private review ${VALID_ID_1}`);
  assert.equal(delBtn.disabled, false);

  await delBtn.onclick();

  assert.equal(calls.length, 3);
  assert.equal(calls[1].method, 'pi_review_cleanup');
  assert.equal(calls[2].method, 'pi_review_inventory');
  assert.ok(status.textContent.startsWith(REVIEW_CLEANUP_SUCCESS_TEXT));
  assert.equal(list.children.length, 0);
  assert.equal(browse.disabled, false);
}

function previewRecord(over = {}) {
  return { schema: 'navi-review-preview-v1', review_id: VALID_ID_1, ok: true, stale: false, truncated: false, diff_lines: 2, before_bytes: 3, after_bytes: 3, diff: '-<b>x</b>\n+y\n', ...over };
}

async function testPreviewGatesApplyAndShowsDiffAsText() {
  const calls = [];
  let reply = previewRecord();
  const { reviewInventoryControls, context } = loadHarness({
    rpc: async (method, params) => {
      calls.push({ method, params });
      if (method === 'pi_review_inventory') return makeInventory([makeValidItem(VALID_ID_1, DIGEST_1)]);
      throw new Error(`unexpected rpc ${method}`);
    },
    activeProjectId: () => 'proj-1',
  });
  // route preview through a controllable reply
  const orig = context.rpc;
  context.rpc = async (method, params) => (method === 'pi_review_preview' ? (calls.push({ method, params }), reply) : orig(method, params));
  const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
  await browse.onclick();
  const input = findOneByClass(list, 'review-apply-confirm');
  const applyBtn = findOneByClass(list, 'review-apply-button');
  const previewBtn = findOneByClass(list, 'review-preview-button');
  const pre = findOneByClass(list, 'review-preview-diff');
  const confirm = `apply private review ${VALID_ID_1}`;
  setInputValue(input, confirm);
  assert.equal(applyBtn.disabled, true, 'apply disabled before preview');
  await previewBtn.onclick();
  const pc = calls.find((c) => c.method === 'pi_review_preview');
  assert.deepEqual(Object.keys(pc.params).sort(), ['project_id', 'proposal_digest', 'review_id']);
  assert.equal(pre.hidden, false);
  assert.equal(pre.textContent, '-<b>x</b>\n+y\n');
  assert.equal(applyBtn.disabled, false, 'apply enabled after good preview and exact confirm');
  reply = previewRecord({ stale: true });
  await previewBtn.onclick();
  assert.equal(applyBtn.disabled, true, 'stale preview disables apply');
  assert.equal(pre.hidden, true);
  reply = previewRecord();
  await previewBtn.onclick();
  assert.equal(applyBtn.disabled, false);
  reply = { schema: 'navi-review-preview-v1', review_id: VALID_ID_1, ok: false, reason: 'digest_mismatch' };
  await previewBtn.onclick();
  assert.equal(applyBtn.disabled, true, 'failed preview disables apply');
  reply = previewRecord({ extra: 1 });
  await previewBtn.onclick();
  assert.equal(applyBtn.disabled, true, 'malformed preview disables apply');
  assert.equal(pre.textContent, '');
}

async function testPreviewValidatorRejects() {
  const { context } = loadHarness({ rpc: async () => { throw new Error('x'); }, activeProjectId: () => 'proj-1' });
  const v = (raw) => context.validateReviewPreviewResult(raw, VALID_ID_1);
  assert.equal(v(previewRecord()).ok, true);
  for (const bad of [null, [], previewRecord({ schema: 'x' }), previewRecord({ review_id: VALID_ID_2 }), previewRecord({ diff: 5 }), previewRecord({ diff_lines: -1 }), previewRecord({ stale: 'no' }), previewRecord({ diff: 'x'.repeat(40001) }),
    { schema: 'navi-review-preview-v1', review_id: VALID_ID_1, ok: false, reason: '/etc/passwd' }]) {
    assert.throws(() => v(bad));
  }
}

function trRecord(over = {}) {
  return { schema: 'navi-review-testrun-v1', review_id: VALID_ID_1, ok: true, passed: true, exit_code: 0, stdout_tail: '', stderr_tail: 'OK <b>x</b>', ...over };
}

async function testTestrunGatedByPreviewAndConfirmAndRendersText() {
  const calls = [];
  let reply = trRecord();
  const { reviewInventoryControls, context } = loadHarness({
    rpc: async (method, params) => {
      calls.push({ method, params });
      if (method === 'pi_review_inventory') return makeInventory([makeValidItem(VALID_ID_1, DIGEST_1)]);
      throw new Error(`unexpected rpc ${method}`);
    },
    activeProjectId: () => 'proj-1',
  });
  const orig = context.rpc;
  context.rpc = async (method, params) => (method === 'pi_review_testrun' ? (calls.push({ method, params }), reply) : orig(method, params));
  const [browse, status, list] = reviewInventoryControls({ meta: { project_id: 'proj-1' } });
  await browse.onclick();
  const input = findOneByClass(list, 'review-testrun-confirm');
  const btn = findOneByClass(list, 'review-testrun-button');
  const out = findOneByClass(list, 'review-testrun-output');
  const previewBtn = findOneByClass(list, 'review-preview-button');
  const confirm = `run tests on private copy ${VALID_ID_1}`;
  setInputValue(input, confirm);
  assert.equal(btn.disabled, true, 'disabled before preview');
  await previewBtn.onclick();
  assert.equal(btn.disabled, false);
  setInputValue(input, confirm + 'x');
  assert.equal(btn.disabled, true, 'wrong phrase disables');
  setInputValue(input, `apply private review ${VALID_ID_1}`);
  assert.equal(btn.disabled, true, 'apply phrase does not enable test run');
  setInputValue(input, confirm);
  await btn.onclick();
  const c = calls.find((x) => x.method === 'pi_review_testrun');
  assert.deepEqual(Object.keys(c.params).sort(), ['confirm', 'project_id', 'proposal_digest', 'review_id']);
  assert.equal(c.params.confirm, confirm);
  assert.equal(out.hidden, false);
  assert.equal(out.textContent, 'OK <b>x</b>');
  assert.match(status.textContent, /Tests passed/);
  assert.match(status.textContent, /not changed/);
  assert.equal(input.value, '');
  setInputValue(input, confirm);
  reply = trRecord({ passed: false, exit_code: 1 });
  await btn.onclick();
  assert.match(status.textContent, /Tests failed/);
  setInputValue(input, confirm);
  reply = { schema: 'navi-review-testrun-v1', review_id: VALID_ID_1, ok: false, reason: 'stale_target' };
  await btn.onclick();
  assert.match(status.textContent, /changed since/);
  assert.equal(out.hidden, true);
  setInputValue(input, confirm);
  reply = trRecord({ extra: 1 });
  await btn.onclick();
  assert.match(status.textContent, /did not complete/);
  assert.equal(out.textContent, '');
}

async function testTestrunValidatorRejects() {
  const { context } = loadHarness({ rpc: async () => { throw new Error('x'); }, activeProjectId: () => 'proj-1' });
  const v = (raw) => context.validateReviewTestrunResult(raw, VALID_ID_1);
  assert.equal(v(trRecord()).ok, true);
  assert.equal(v(trRecord({ exit_code: null })).ok, true);
  for (const bad of [null, [], trRecord({ schema: 'x' }), trRecord({ review_id: VALID_ID_2 }), trRecord({ passed: 'y' }), trRecord({ exit_code: '0' }),
    trRecord({ stderr_tail: 'x'.repeat(4001) }), trRecord({ stdout_tail: 5 }),
    { schema: 'navi-review-testrun-v1', review_id: VALID_ID_1, ok: false, reason: '/etc/passwd' }]) {
    assert.throws(() => v(bad));
  }
}

const tests = [
  ['budget and static guards', testBudgetAndStaticGuards],
  ['validator accept and reject cases', testValidatorAcceptAndReject],
  ['apply row only on valid rows and confirm gating', testApplyRowOnlyOnValidRowsAndConfirmGating],
  ['success path strict four keys and re-inventory', testSuccessPathStrictKeysAndReInventory],
  ['refusal and unknown outcome reasons', testRefusalAndUnknownReasons],
  ['mutual exclusion across apply, delete, and browse', testMutualExclusionAcrossApplyDeleteBrowse],
  ['unknown outcome on thrown error and invalid reply', testUnknownOutcomeOnThrownErrorAndInvalidReply],
  ['unknown outcome on project switch before and mid-call', testUnknownOutcomeOnProjectSwitchBeforeAndMidCall],
  ['existing delete flow unbroken', testExistingDeleteFlowUnbroken],
  ['preview gates apply and renders diff as text', testPreviewGatesApplyAndShowsDiffAsText],
  ['preview validator rejects malformed', testPreviewValidatorRejects],
  ['test run gated by preview and confirm, renders text', testTestrunGatedByPreviewAndConfirmAndRendersText],
  ['test run validator rejects malformed', testTestrunValidatorRejects],
];

for (const [name, fn] of tests) {
  await fn();
  console.log(`PASS: ${name}`);
}
console.log(`All ${tests.length} review apply UI tests passed.`);
