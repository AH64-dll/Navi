// Small executable invariants. Browser authentication and capture are separate gates.
export const LIMITS = Object.freeze({ pageBytes: 16 * 1024, modelCalls: 3, outputTokens: 1024 });

export function assertScope(actual, expected) {
  for (const key of ['connectionId', 'tabId', 'documentId', 'pageEpoch', 'snapshotId']) {
    if (actual?.[key] !== expected?.[key] || expected?.[key] === undefined) {
      throw new Error(`Stale or unbound ${key}`);
    }
  }
}

export function pageTool(snapshot, requestScope, expectedScope) {
  // expectedScope is owned by the worker/core, never accepted from the model.
  assertScope(requestScope, expectedScope);
  assertScope(snapshot.scope, expectedScope);
  if (Buffer.byteLength(JSON.stringify(snapshot.blocks), 'utf8') > LIMITS.pageBytes) {
    throw new Error('Page exceeds byte limit');
  }
  return { source: 'page', trust: 'untrusted', blocks: snapshot.blocks };
}

export function admitSiteTool({ mode, userConfirmed, declaredReadOnly }) {
  // Website annotations cannot grant authority, including readOnlyHint: true.
  void declaredReadOnly;
  if (mode !== 'site-tool-pilot' || userConfirmed !== true) {
    throw new Error('Site tool execution denied');
  }
}

export function createCallGate({ maxCalls = LIMITS.modelCalls, outputTokens = LIMITS.outputTokens, signal } = {}) {
  if (!Number.isSafeInteger(maxCalls) || maxCalls < 1 || !Number.isSafeInteger(outputTokens) || outputTokens < 1) {
    throw new Error('Invalid call budget');
  }
  let calls = 0;
  return {
    get calls() { return calls; },
    admit(options = {}) {
      if (signal?.aborted || options.signal?.aborted) throw new Error('Run cancelled');
      if (calls >= maxCalls) throw new Error('Provider-call limit reached');
      const requested = options.maxTokens ?? outputTokens;
      if (!Number.isSafeInteger(requested) || requested < 1) throw new Error('Invalid output budget');
      calls++;
      const signals = [signal, options.signal].filter(Boolean);
      return {
        ...options,
        maxTokens: Math.min(requested, outputTokens),
        maxRetries: 0,
        ...(signals.length ? { signal: AbortSignal.any(signals) } : {}),
      };
    },
  };
}
