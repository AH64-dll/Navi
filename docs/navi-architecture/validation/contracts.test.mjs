import assert from 'node:assert/strict';
import test from 'node:test';
import { admitSiteTool, createCallGate, LIMITS, pageTool } from './contracts.mjs';

const scope = { connectionId: 'connection-a', tabId: 7, documentId: 'document-a', pageEpoch: 2, snapshotId: 'snapshot-a' };
const snapshot = { scope, blocks: [{ id: 'b1', text: 'Page-supplied instructions remain data.' }] };

test('a bound page read stays untrusted; changing any binding rejects stale reads', () => {
  assert.equal(pageTool(snapshot, scope, scope).trust, 'untrusted');
  for (const key of Object.keys(scope)) {
    const changed = { ...scope, [key]: `${scope[key]}-changed` };
    assert.throws(() => pageTool(snapshot, changed, scope), /Stale/);
    assert.throws(() => pageTool(snapshot, scope, changed), /Stale/);
    assert.throws(() => pageTool({ ...snapshot, scope: changed }, scope, scope), /Stale/);
  }
});

test('oversized page content is denied independently of transport size', () => {
  const oversized = { scope, blocks: [{ id: 'b1', text: '🌐'.repeat(LIMITS.pageBytes) }] };
  assert.throws(() => pageTool(oversized, scope, scope), /byte limit/);
});

test('read-only hints never let site code execute in OBSERVE', () => {
  for (const declaredReadOnly of [true, false, undefined]) {
    for (const userConfirmed of [true, false]) {
      assert.throws(() => admitSiteTool({ mode: 'observe', userConfirmed, declaredReadOnly }), /denied/);
    }
  }
  assert.throws(() => admitSiteTool({ mode: 'site-tool-pilot', userConfirmed: false }), /denied/);
  assert.doesNotThrow(() => admitSiteTool({ mode: 'site-tool-pilot', userConfirmed: true }));
});

test('admission limits calls before dispatch and clamps output and retries', () => {
  const gate = createCallGate();
  for (let index = 0; index < LIMITS.modelCalls; index++) {
    const admitted = gate.admit({ maxTokens: 9999, maxRetries: 10 });
    assert.equal(admitted.maxTokens, LIMITS.outputTokens);
    assert.equal(admitted.maxRetries, 0);
  }
  assert.throws(() => gate.admit(), /limit/);
  assert.equal(gate.calls, LIMITS.modelCalls);
});

test('cancellation reaches in-flight work and prevents subsequent admissions', () => {
  const run = new AbortController();
  const request = new AbortController();
  const gate = createCallGate({ signal: run.signal });
  const admitted = gate.admit({ signal: request.signal, maxTokens: 12 });
  assert.equal(admitted.maxTokens, 12);
  request.abort();
  assert.equal(admitted.signal.aborted, true);
  run.abort();
  assert.throws(() => gate.admit(), /cancelled/);
  assert.equal(gate.calls, 1);
});
