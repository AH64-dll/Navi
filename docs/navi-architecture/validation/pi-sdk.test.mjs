import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, readdir, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { basename, dirname, join } from 'node:path';
import { tmpdir } from 'node:os';

process.env.PI_OFFLINE = '1';
const { fauxProvider, fauxAssistantMessage, fauxToolCall } = await import('@earendil-works/pi-ai');
const { createMemoryRuntime, createObserveSession } = await import('./pi-session.mjs');

async function fixture(t, { responses, maxCalls, signal, readPage = async () => ({ blockId: 'b1', text: 'Fixture evidence' }) }) {
  const cwd = await mkdtemp(join(tmpdir(), 'navi-sdk-check-'));
  // A real neighboring context file must not enter the prompt or get rewritten.
  const sentinel = 'NAVI_TEST_UNTRUSTED_PROJECT_INSTRUCTIONS';
  await writeFile(join(cwd, 'AGENTS.md'), sentinel);
  let session;
  t.after(async () => {
    if (session) { await session.abort(); session.dispose(); }
    const resolved = await realpath(cwd);
    assert.equal(dirname(resolved), await realpath(tmpdir()));
    assert.match(basename(resolved), /^navi-sdk-check-/);
    await rm(resolved, { recursive: true, force: true });
  });
  const modelRuntime = await createMemoryRuntime();
  const faux = fauxProvider({ tokensPerSecond: 1_000_000 });
  modelRuntime.registerNativeProvider(faux.provider);
  faux.setResponses(responses);
  const harness = await createObserveSession({ cwd, model: faux.getModel(), modelRuntime, readPage, maxCalls, signal });
  session = harness.session;
  return { ...harness, faux, cwd, sentinel };
}

test('published SDK runs a page-tool round trip with no built-ins, discovery or session files', { timeout: 15000 }, async t => {
  let reads = 0;
  const harness = await fixture(t, {
    responses: [
      (context, options) => {
        assert.deepEqual(context.tools.map(tool => tool.name), ['navi_page']);
        assert.equal(JSON.stringify(context).includes('NAVI_TEST_UNTRUSTED_PROJECT_INSTRUCTIONS'), false);
        assert.equal(options.maxTokens, 1024);
        assert.equal(options.maxRetries, 0);
        return fauxAssistantMessage(fauxToolCall('navi_page', { blockId: 'b1' }));
      },
      context => {
        assert.match(JSON.stringify(context.messages), /Fixture evidence/);
        return fauxAssistantMessage('Answer grounded in block b1.');
      },
    ],
    readPage: async blockId => { reads++; assert.equal(blockId, 'b1'); return { text: 'Fixture evidence' }; },
  });
  await harness.session.prompt('Explain the captured page.', { expandPromptTemplates: false });
  assert.equal(harness.faux.state.callCount, 2);
  assert.equal(harness.gate.calls, 2);
  assert.equal(reads, 1);
  assert.deepEqual(await readdir(harness.cwd), ['AGENTS.md']);
  assert.equal(await readFile(join(harness.cwd, 'AGENTS.md'), 'utf8'), harness.sentinel);
  assert.equal(harness.session.messages.at(-1).stopReason, 'stop');
});

test('SDK admission rejects a second provider call before the provider is invoked', { timeout: 15000 }, async t => {
  const harness = await fixture(t, {
    maxCalls: 1,
    responses: [fauxAssistantMessage(fauxToolCall('navi_page', { blockId: 'b1' })), fauxAssistantMessage('Must not run')],
  });
  await harness.session.prompt('Read and answer.', { expandPromptTemplates: false });
  assert.equal(harness.faux.state.callCount, 1);
  assert.equal(harness.gate.calls, 1);
  assert.equal(harness.session.messages.at(-1).stopReason, 'error');
  assert.match(harness.session.messages.at(-1).errorMessage, /Provider-call limit/);
});

test('SDK refuses an invented shell tool and can report the failure', { timeout: 15000 }, async t => {
  const harness = await fixture(t, {
    responses: [
      fauxAssistantMessage(fauxToolCall('bash', { command: 'NAVI_FORBIDDEN_COMMAND' })),
      context => {
        const result = context.messages.find(message => message.role === 'toolResult');
        assert.equal(result.isError, true);
        return fauxAssistantMessage('That tool is unavailable.');
      },
    ],
  });
  await harness.session.prompt('Adversarial tool request fixture.', { expandPromptTemplates: false });
  assert.equal(harness.faux.state.callCount, 2);
  assert.deepEqual(await readdir(harness.cwd), ['AGENTS.md']);
});

test('a cancelled run starts no provider request', { timeout: 15000 }, async t => {
  const controller = new AbortController();
  controller.abort();
  const harness = await fixture(t, { signal: controller.signal, responses: [fauxAssistantMessage('Must not run')] });
  await harness.session.prompt('Cancelled request.', { expandPromptTemplates: false });
  assert.equal(harness.faux.state.callCount, 0);
  assert.equal(harness.session.messages.at(-1).stopReason, 'error');
});

test('one model response cannot exceed the page-read allowance', { timeout: 15000 }, async t => {
  let reads = 0;
  const harness = await fixture(t, {
    responses: [
      fauxAssistantMessage(Array.from({ length: 5 }, (_, index) => fauxToolCall('navi_page', { blockId: `b${index}` }))),
      context => {
        const results = context.messages.filter(message => message.role === 'toolResult');
        assert.equal(results.length, 5);
        assert.equal(results.filter(result => result.isError).length, 1);
        return fauxAssistantMessage('Read budget reached.');
      },
    ],
    readPage: async () => { reads++; return { text: 'Small block' }; },
  });
  await harness.session.prompt('Read multiple blocks.', { expandPromptTemplates: false });
  assert.equal(reads, 4);
  assert.equal(harness.faux.state.callCount, 2);
});

test('oversized tool output is rejected before it enters provider context', { timeout: 15000 }, async t => {
  const harness = await fixture(t, {
    responses: [
      fauxAssistantMessage(fauxToolCall('navi_page', { blockId: 'b1' })),
      context => {
        const result = context.messages.find(message => message.role === 'toolResult');
        assert.equal(result.isError, true);
        assert.match(JSON.stringify(result), /byte limit/);
        assert.equal(JSON.stringify(context).includes('OVERSIZED_PAGE_MARKER'), false);
        return fauxAssistantMessage('Page block exceeds limit.');
      },
    ],
    readPage: async () => ({ text: 'OVERSIZED_PAGE_MARKER'.repeat(1000) }),
  });
  await harness.session.prompt('Read a large block.', { expandPromptTemplates: false });
  assert.equal(harness.faux.state.callCount, 2);
});

test('host cancellation aborts a pending page read and stops the tool loop', { timeout: 15000 }, async t => {
  const controller = new AbortController();
  let markStarted;
  const started = new Promise(resolve => { markStarted = resolve; });
  let readAborted = false;
  const harness = await fixture(t, {
    signal: controller.signal,
    responses: [fauxAssistantMessage(fauxToolCall('navi_page', { blockId: 'b1' })), fauxAssistantMessage('Must not run')],
    readPage: async (_blockId, signal) => new Promise((_resolve, reject) => {
      signal.throwIfAborted();
      signal.addEventListener('abort', () => { readAborted = true; reject(signal.reason); }, { once: true });
      markStarted();
    }),
  });
  const prompt = harness.session.prompt('Read then cancel.', { expandPromptTemplates: false });
  await started;
  controller.abort();
  await harness.session.abort();
  await prompt;
  assert.equal(readAborted, true);
  assert.equal(harness.faux.state.callCount, 1);
  assert.equal(harness.session.isStreaming, false);
});
