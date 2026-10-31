/**
 * Worker authority and grant-routing tests against a fake platform.
 *
 * A mock can prove: who may talk to the worker, which browser-supplied identity binds a document, when a
 * grant ends, and which CLI requests are refused before any browser work. It cannot prove live Chrome
 * behaviour; that stays a manual checklist.
 *
 * The fake browser answers `hello` and runs the injected script (opening its port) inside `executeScript`,
 * which is the real ordering. Timers come from a fake clock that only advances when a test asks, so every
 * await here settles deterministically.
 */
import assert from "node:assert/strict";
import test from "node:test";
import type { Clock } from "../../src/core/ports.ts";
import { HANDSHAKE_TIMEOUT_MS, MAX_SCROLL_DELTA } from "../../src/protocol/limits.ts";
import type { Capability, Frame, Grant, RequestFrame, ResultFrame } from "../../src/protocol/types.ts";
import { parseFrame } from "../../src/protocol/validate.ts";
import type {
	InjectionResult,
	MessageSender,
	NativePortApi,
	Platform,
	RuntimePortApi,
	TabChangeInfo,
	TabInfo,
} from "../platform/types.ts";
import { CONTENT_BIND_TIMEOUT_MS, createWorker } from "../worker/worker.ts";

const EXTENSION_ID = "abcdefghijklmnopabcdefghijklmnop";
const POPUP_URL = `chrome-extension://${EXTENSION_ID}/popup.html`;
const CONNECTION_ID = "connection-1";
const DOCUMENT_ID = "doc-1";
const OTHER_DOCUMENT_ID = "doc-2";
const TAB_ID = 7;
const ALL_CAPABILITIES: Capability[] = ["observe", "focus", "scroll"];

type Listener<T> = (value: T) => void;

function sink<T>() {
	const listeners: Listener<T>[] = [];
	return {
		addListener(listener: Listener<T>) {
			listeners.push(listener);
		},
		emit(value: T) {
			for (const listener of [...listeners]) listener(value);
		},
	};
}

/** Yields to pending microtasks without a timer. */
function flush(): Promise<void> {
	return new Promise((resolve) => {
		setImmediate(resolve);
	});
}

class FakeNativePort implements NativePortApi {
	readonly posted: unknown[] = [];
	readonly onMessage = sink<unknown>();
	readonly onDisconnect = sink<void>();
	disconnected = false;
	private readonly confirms: Capability[] | null;
	private readonly answers: boolean;
	private ackSent = false;

	constructor(options: { answersHello?: boolean; ackCapabilities?: Capability[] | null }) {
		this.answers = options.answersHello ?? true;
		this.confirms = options.ackCapabilities ?? null;
	}

	postMessage(message: unknown): void {
		this.posted.push(message);
		const frame = message as { type?: string; capabilities?: Capability[] };
		if (frame.type !== "hello" || !this.answers) return;
		const confirmed = this.confirms ?? frame.capabilities ?? ["observe"];
		queueMicrotask(() => {
			if (this.ackSent || this.disconnected) return;
			this.ackSent = true;
			this.onMessage.emit({
				v: 1,
				seq: 1,
				type: "hello.ack",
				connectionId: CONNECTION_ID,
				peer: { kind: "cli", version: "0.0.1" },
				capabilities: confirmed,
			});
		});
	}

	disconnect(): void {
		this.disconnected = true;
	}

	/** Frames the worker sent, parsed with the shared validator. */
	frames(): Frame[] {
		const parsed: Frame[] = [];
		for (const value of this.posted) {
			if (typeof value !== "object" || value === null) continue;
			parsed.push(parseFrame(value));
		}
		return parsed;
	}

	lastFrame(): Frame | undefined {
		const frames = this.frames();
		return frames[frames.length - 1];
	}
}

class FakeContentPort implements RuntimePortApi {
	readonly name = "navi.content";
	readonly sender: MessageSender;
	readonly posted: unknown[] = [];
	readonly onMessage = sink<unknown>();
	readonly onDisconnect = sink<void>();
	disconnected = false;

	constructor(sender: MessageSender) {
		this.sender = sender;
	}

	postMessage(message: unknown): void {
		this.posted.push(message);
	}

	disconnect(): void {
		this.disconnected = true;
	}

	operations(): Array<Record<string, unknown>> {
		const found: Array<Record<string, unknown>> = [];
		for (const value of this.posted) {
			if (typeof value !== "object" || value === null) continue;
			const message = value as Record<string, unknown>;
			if (message.t === "content.observe" || message.t === "content.focus" || message.t === "content.scroll") {
				found.push(message);
			}
		}
		return found;
	}

	lastOperation(): Record<string, unknown> | undefined {
		return this.operations().at(-1);
	}

	countOf(t: string): number {
		return this.posted.filter((value) => (value as { t?: string }).t === t).length;
	}

	/** Answers like the injected aggregator would. */
	answerObservation(requestId: string, observationId: string, nodes: unknown[] = []): void {
		this.onMessage.emit({
			t: "content.observation",
			requestId,
			draft: {
				observationId,
				capturedAt: 1,
				url: "https://example.test/page",
				nodes,
				visited: 0,
				truncated: false,
				omitted: [],
			},
		});
	}

	answerAction(
		requestId: string,
		outcome: "applied" | "unknown",
		scroll?: { scrollX: number; scrollY: number },
	): void {
		const message: Record<string, unknown> = { t: "content.action", requestId, outcome };
		if (scroll) {
			message.scrollX = scroll.scrollX;
			message.scrollY = scroll.scrollY;
		}
		this.onMessage.emit(message);
	}

	fail(requestId: string, code: string): void {
		this.onMessage.emit({ t: "content.failure", requestId, code, message: "the page operation failed" });
	}
}

type Options = {
	tab?: TabInfo;
	focused?: boolean;
	documentId?: string;
	/** Document id the injected script reports on its port. Defaults to the injected document id. */
	portDocumentId?: string;
	injectError?: boolean;
	nativeError?: boolean;
	answersHello?: boolean;
	ackCapabilities?: Capability[] | null;
	/** Holds `tabs.get` open so a test can interleave work with a pending identity check. */
	gateTabLookup?: boolean;
};

type FakePlatform = Platform & {
	/** The most recent native connection. Throws when the worker never opened one. */
	native: FakeNativePort;
	/** Every native connection the worker opened, in order. */
	connections: FakeNativePort[];
	executeCalls: number;
	lastInjection: { target: { tabId: number; frameIds: number[] }; files: string[] };
	onInject?: (documentId: string) => void;
	/** Releases a `tabs.get` call held by `gateTabLookup`. */
	releaseTabLookup(): void;
	removed(tabId: number): void;
	replaced(removedTabId: number): void;
	updated(tabId: number, changeInfo: TabChangeInfo): void;
};

function fakePlatform(options: Options): FakePlatform {
	const tab: TabInfo = options.tab ?? {
		id: TAB_ID,
		windowId: 1,
		active: true,
		discarded: false,
		url: "https://example.test/page?secret=1#frag",
		title: "Example page",
	};
	const connections: FakeNativePort[] = [];
	const openNative = (): FakeNativePort => {
		const created = new FakeNativePort({
			answersHello: options.answersHello,
			ackCapabilities: options.ackCapabilities,
		});
		connections.push(created);
		return created;
	};
	const removed = sink<number>();
	const replaced = sink<{ added: number; removed: number }>();
	const updated = sink<{ tabId: number; changeInfo: TabChangeInfo }>();
	let releaseLookup: (() => void) | null = null;
	const platform: FakePlatform = {
		releaseTabLookup() {
			const release = releaseLookup;
			releaseLookup = null;
			release?.();
		},
		get native() {
			const last = connections[connections.length - 1];
			if (!last) throw new Error("connectNative has not been called");
			return last;
		},
		connections,
		executeCalls: 0,
		lastInjection: { target: { tabId: -1, frameIds: [] }, files: [] },
		removed: (tabId) => removed.emit(tabId),
		replaced: (removedTabId) => replaced.emit({ added: -1, removed: removedTabId }),
		updated: (tabId, changeInfo) => updated.emit({ tabId, changeInfo }),
		runtime: {
			id: EXTENSION_ID,
			getURL: (path) => `chrome-extension://${EXTENSION_ID}/${path}`,
			connectNative: () => {
				if (options.nativeError) throw new Error("Specified native messaging host not found");
				return openNative();
			},
			// The worker core never registers these; the service-worker entry does.
			onMessage: { addListener: () => undefined },
			onConnect: { addListener: () => undefined },
		},
		tabs: {
			get: async (tabId) => {
				if (options.gateTabLookup === true) {
					await new Promise<void>((resolve) => {
						releaseLookup = resolve;
					});
				}
				if (tab.id !== tabId) throw new Error("no such tab");
				return tab;
			},
			query: async () => (tab.active === true ? [tab] : []),
			onRemoved: { addListener: (listener) => removed.addListener(listener) },
			onReplaced: {
				addListener: (listener) => replaced.addListener((value) => listener(value.added, value.removed)),
			},
			onUpdated: {
				addListener: (listener) => updated.addListener((value) => listener(value.tabId, value.changeInfo, tab)),
			},
		},
		windows: {
			getLastFocused: async () => ({ id: 1, focused: options.focused ?? true }),
			onFocusChanged: { addListener: () => undefined },
		},
		scripting: {
			executeScript: async (injection): Promise<InjectionResult[]> => {
				platform.executeCalls++;
				platform.lastInjection = { target: injection.target, files: injection.files };
				if (options.injectError) throw new Error("cannot inject into this document");
				const documentId = options.documentId ?? DOCUMENT_ID;
				// The real browser runs the script before the injection promise resolves, so the port the
				// script opens already exists when the worker starts waiting for it.
				platform.onInject?.(documentId);
				return [{ documentId, frameId: 0 }];
			},
		},
	};
	return platform;
}

class FakeClock implements Clock {
	private time = 1_000;
	private timers: Array<{ at: number; callback: () => void }> = [];

	now(): number {
		return this.time;
	}

	schedule(callback: () => void, delayMs: number): () => void {
		const timer = { at: this.time + delayMs, callback };
		this.timers.push(timer);
		return () => {
			this.timers = this.timers.filter((entry) => entry !== timer);
		};
	}

	/** Fires every timer due within `delayMs`, in order. */
	advance(delayMs: number): void {
		this.time += delayMs;
		const due = this.timers.filter((timer) => timer.at <= this.time).sort((left, right) => left.at - right.at);
		this.timers = this.timers.filter((timer) => timer.at > this.time);
		for (const timer of due) timer.callback();
	}
}

type Cli = {
	request(payload: Record<string, unknown>, requestId: string): void;
	/** Sends a request that deliberately names another binding. */
	requestWithBinding(payload: Record<string, unknown>, requestId: string, binding: Grant["binding"]): void;
	/** Sends a grant-carrying event frame, which a CLI must never send. */
	eventWithGrant(grant: Grant): void;
	cancel(targetRequestId: string, requestId: string): void;
	result(requestId: string): ResultFrame | undefined;
	event(): { grant: Grant | null; reason: string } | undefined;
};

type Harness = {
	worker: ReturnType<typeof createWorker>;
	platform: FakePlatform;
	clock: FakeClock;
	ports: FakeContentPort[];
	readGrant(): Grant;
	attach(capabilities: Capability[]): Promise<unknown>;
	popup(message: unknown): Promise<unknown>;
	cli: Cli;
};

function harness(options: Options = {}): Harness {
	const platform = fakePlatform(options);
	const clock = new FakeClock();
	let counter = 0;
	const worker = createWorker({ platform, clock, newId: () => `id-${++counter}` });
	// Same wiring as browser/worker/main.ts, so lifecycle events travel the real path.
	platform.tabs.onRemoved.addListener((tabId) => worker.handleTabRemoved(tabId));
	platform.tabs.onReplaced.addListener((_added, removedTabId) => worker.handleTabReplaced(removedTabId));
	platform.tabs.onUpdated.addListener((tabId, changeInfo) => worker.handleTabUpdated(tabId, changeInfo));
	const ports: FakeContentPort[] = [];
	platform.onInject = (documentId) => {
		const sender: MessageSender = {
			id: EXTENSION_ID,
			url: "https://example.test/page",
			tab: { id: TAB_ID },
			frameId: 0,
			documentId: options.portDocumentId ?? documentId,
		};
		const port = new FakeContentPort(sender);
		ports.push(port);
		worker.handleContentPort(port);
	};
	const popupSender: MessageSender = { id: EXTENSION_ID, url: POPUP_URL };
	let currentGrant: Grant | null = null;
	let seq = 1;
	const emit = (requestId: string, payload: Record<string, unknown>): void => {
		seq++;
		assert.ok(currentGrant, "a grant is required before a CLI request");
		platform.native.onMessage.emit({
			v: 1,
			seq,
			type: "request",
			connectionId: CONNECTION_ID,
			requestId,
			payload: { binding: currentGrant.binding, ...payload },
		} as unknown as RequestFrame);
	};
	const cli: Cli = {
		request: (payload, requestId) => emit(requestId, payload),
		requestWithBinding: (payload, requestId, binding) => {
			seq++;
			platform.native.onMessage.emit({
				v: 1,
				seq,
				type: "request",
				connectionId: CONNECTION_ID,
				requestId,
				payload: { binding, ...payload },
			} as unknown as RequestFrame);
		},
		eventWithGrant: (grant) => {
			seq++;
			platform.native.onMessage.emit({
				v: 1,
				seq,
				type: "event",
				connectionId: CONNECTION_ID,
				kind: "binding.changed",
				payload: { grant, reason: "attached" },
			});
		},
		cancel: (targetRequestId, requestId) => {
			seq++;
			platform.native.onMessage.emit({
				v: 1,
				seq,
				type: "request",
				connectionId: CONNECTION_ID,
				requestId,
				payload: { op: "cancel", requestId: targetRequestId },
			} satisfies RequestFrame);
		},
		result: (requestId) => {
			for (const frame of platform.native.frames()) {
				if (frame.type === "result" && frame.requestId === requestId) return frame;
			}
			return undefined;
		},
		event: () => {
			const events = platform.native.frames().filter((frame) => frame.type === "event");
			const last = events[events.length - 1];
			return last && last.type === "event" ? last.payload : undefined;
		},
	};
	const popup = (message: unknown) => worker.handlePopupMessage(message, popupSender);
	const attach = async (capabilities: Capability[]): Promise<unknown> => {
		const response = await popup({ t: "popup.attach", capabilities });
		// A failed attach opens no native session, so nothing can be read from it.
		const event = platform.connections.length > 0 ? cli.event() : undefined;
		currentGrant = event?.grant ?? null;
		return response;
	};
	return {
		worker,
		platform,
		clock,
		ports,
		readGrant() {
			const event = cli.event();
			const value = event?.grant ?? null;
			assert.ok(value, "expected an attached grant");
			return value;
		},
		attach,
		popup,
		cli,
	};
}

function expectOk(created: Harness, requestId: string): Extract<ResultFrame, { ok: true }> {
	const result = created.cli.result(requestId);
	if (!result) throw new Error(`no result frame for ${requestId}`);
	if (!result.ok) throw new Error(`expected success for ${requestId}, got ${result.error.code}`);
	return result;
}

function expectError(created: Harness, requestId: string): Extract<ResultFrame, { ok: false }> {
	const result = created.cli.result(requestId);
	if (!result) throw new Error(`no result frame for ${requestId}`);
	if (result.ok) throw new Error(`expected an error for ${requestId}`);
	return result;
}

async function attached(options: Options = {}): Promise<Harness & { port: FakeContentPort; grant: Grant }> {
	const created = harness(options);
	await created.attach(ALL_CAPABILITIES);
	const port = created.ports[0];
	assert.ok(port, "the injected content script must open a port");
	return { ...created, port, grant: created.readGrant() };
}

/** Runs an observe and returns the observation id the worker accepted. */
async function observe(created: Harness & { port: FakeContentPort }, requestId: string): Promise<string> {
	created.cli.request({ op: "observe" }, requestId);
	const operation = created.port.lastOperation();
	assert.ok(operation && operation.t === "content.observe", "observe must reach the page channel");
	created.port.answerObservation(operation.requestId as string, operation.observationId as string);
	await flush();
	const payload = expectOk(created, requestId).payload;
	const observation = (payload as { observation: { observationId: string } }).observation;
	return observation.observationId;
}

test("attach pins the active tab, injects the top frame and announces one grant", async () => {
	const created = await attached();
	assert.equal(created.platform.lastInjection.target.tabId, TAB_ID);
	assert.deepEqual(created.platform.lastInjection.target.frameIds, [0]);
	assert.deepEqual(created.platform.lastInjection.files, ["content.js"]);
	assert.equal(created.grant.binding.tabId, TAB_ID);
	assert.equal(created.grant.binding.documentId, DOCUMENT_ID);
	assert.deepEqual(created.grant.capabilities, ALL_CAPABILITIES);
	assert.equal(created.cli.event()?.reason, "attached");
	assert.equal(created.port.disconnected, false);
	assert.equal(created.ports.length, 1);
});

test("the CLI confirmation narrows the grant and never widens it", async () => {
	const created = await attached({ ackCapabilities: ["observe"] });
	assert.deepEqual(created.grant.capabilities, ["observe"]);
});

test("a popup message from another sender is refused and learns no state", async () => {
	const created = harness();
	for (const sender of [
		{ id: "otherextension", url: POPUP_URL },
		{ id: EXTENSION_ID, url: `chrome-extension://${EXTENSION_ID}/other.html` },
		{ id: EXTENSION_ID, url: POPUP_URL, tab: { id: 3 } },
	] satisfies MessageSender[]) {
		const response = (await created.worker.handlePopupMessage(
			{ t: "popup.attach", capabilities: ["observe"] },
			sender,
		)) as {
			state: { phase: string; tab?: unknown; error?: { code: string } };
		};
		assert.equal(response.state.phase, "detached");
		assert.equal(response.state.tab, undefined);
		assert.equal(response.state.error?.code, "INTERNAL");
	}
	assert.equal(created.platform.executeCalls, 0);
	assert.equal(created.platform.connections.length, 0, "an unauthenticated sender must not open a native connection");
});

test("a content port without matching browser identity is never adopted", async () => {
	const created = await attached();
	const impostors: MessageSender[] = [
		{ id: "otherextension", tab: { id: TAB_ID }, frameId: 0, documentId: DOCUMENT_ID, url: "https://example.test/" },
		{ id: EXTENSION_ID, tab: { id: TAB_ID }, frameId: 1, documentId: DOCUMENT_ID, url: "https://example.test/" },
		{ id: EXTENSION_ID, tab: { id: 8 }, frameId: 0, documentId: DOCUMENT_ID, url: "https://example.test/" },
		{
			id: EXTENSION_ID,
			tab: { id: TAB_ID },
			frameId: 0,
			documentId: OTHER_DOCUMENT_ID,
			url: "https://example.test/",
		},
		{ id: EXTENSION_ID, tab: { id: TAB_ID }, frameId: 0, url: "https://example.test/" },
	];
	for (const sender of impostors) {
		const port = new FakeContentPort(sender);
		created.worker.handleContentPort(port);
		assert.equal(port.disconnected, true, `sender ${JSON.stringify(sender)} must be rejected`);
	}
	created.cli.request({ op: "observe" }, "req-bound");
	assert.equal(created.port.operations().length, 1, "only the bound port receives work");
	assert.equal(created.ports.length, 1);
});

test("an injected port whose document id differs fails the attach without a grant", async () => {
	const created = harness({ portDocumentId: OTHER_DOCUMENT_ID });
	const attaching = created.attach(ALL_CAPABILITIES);
	await flush();
	created.clock.advance(CONTENT_BIND_TIMEOUT_MS + 1);
	const response = (await attaching) as { state: { phase: string; error?: { code: string } } };
	assert.equal(response.state.phase, "detached");
	assert.equal(response.state.error?.code, "CAPTURE_FAILED");
	assert.equal(created.cli.event(), undefined);
	assert.equal(created.ports[0]?.disconnected, true);
});

test("a missing native connector fails the attach before any injection", async () => {
	const created = harness({ nativeError: true });
	const response = (await created.attach(ALL_CAPABILITIES)) as { state: { phase: string; error?: { code: string } } };
	assert.equal(response.state.phase, "detached");
	assert.equal(response.state.error?.code, "NOT_ATTACHED");
	assert.equal(created.platform.executeCalls, 0);
});

test("a CLI that never answers the handshake fails the attach before any injection", async () => {
	const created = harness({ answersHello: false });
	const attaching = created.attach(ALL_CAPABILITIES);
	await flush();
	created.clock.advance(HANDSHAKE_TIMEOUT_MS + 1);
	const response = (await attaching) as { state: { phase: string; error?: { code: string } } };
	assert.equal(response.state.phase, "detached");
	assert.notEqual(response.state.error, undefined);
	assert.equal(created.platform.executeCalls, 0);
});

test("observe carries the worker binding and returns the observation", async () => {
	const created = await attached();
	created.cli.request({ op: "observe" }, "req-1");
	const operation = created.port.lastOperation();
	assert.ok(operation && operation.t === "content.observe");
	const sentBinding = operation.binding as { documentId: string; grantId: string };
	assert.equal(sentBinding.documentId, DOCUMENT_ID);
	assert.equal(sentBinding.grantId, created.grant.binding.grantId);
	assert.equal(typeof operation.observationId, "string");
	created.port.answerObservation(operation.requestId as string, operation.observationId as string);
	await flush();
	const payload = expectOk(created, "req-1").payload as {
		op: string;
		observation: { binding: { documentId: string } };
	};
	assert.equal(payload.op, "observe");
	assert.equal(payload.observation.binding.documentId, DOCUMENT_ID);
});

test("an over-budget observation is refused without ending the connection", async () => {
	const created = await attached();
	created.cli.request({ op: "observe" }, "req-1");
	const operation = created.port.lastOperation();
	assert.ok(operation && operation.t === "content.observe");
	const node = {
		ref: "n1",
		depth: 0,
		tag: "P",
		text: "x".repeat(200),
		interactive: false,
		editable: false,
		offscreen: false,
	};
	created.port.onMessage.emit({
		t: "content.observation",
		requestId: operation.requestId,
		draft: {
			observationId: operation.observationId,
			capturedAt: 1,
			url: "https://example.test/page",
			nodes: Array.from({ length: 100 }, () => ({ ...node })),
			visited: 100,
			truncated: false,
			omitted: [],
		},
	});
	await flush();
	assert.equal(expectError(created, "req-1").error.code, "TOO_LARGE");
	assert.equal(created.platform.native.frames().filter((frame) => frame.type === "bye").length, 0);
});

test("a stale binding or an ungranted capability is refused before browser work", async () => {
	const created = await attached({ ackCapabilities: ["observe"] });
	created.cli.requestWithBinding({ op: "observe" }, "req-stale", {
		...created.grant.binding,
		pageEpoch: created.grant.binding.pageEpoch + 1,
	});
	await flush();
	assert.equal(expectError(created, "req-stale").error.code, "BINDING_STALE");
	created.cli.request({ op: "focus", observationId: "obs-1", ref: "n1" }, "req-cap");
	await flush();
	assert.equal(expectError(created, "req-cap").error.code, "UNSUPPORTED_OP");
	assert.equal(created.port.operations().length, 0);
});

test("focus against a tab that is not active and focused is refused, never activated", async () => {
	const created = await attached({ focused: false });
	const observationId = await observe(created, "req-obs");
	created.cli.request({ op: "focus", observationId, ref: "n1" }, "req-focus");
	await flush();
	assert.equal(expectError(created, "req-focus").error.code, "TAB_NOT_ACTIVE");
	assert.equal(created.port.countOf("content.focus"), 0);
	assert.equal(created.platform.executeCalls, 1, "the tab is inspected, never activated or re-injected");
});

test("a stale observation id is refused before dispatch", async () => {
	const created = await attached();
	created.cli.request({ op: "focus", observationId: "obs-old", ref: "n1" }, "req-old");
	await flush();
	assert.equal(expectError(created, "req-old").error.code, "STALE_OBSERVATION");
	created.cli.request({ op: "scroll", observationId: "obs-old", ref: "n1", align: "start" }, "req-old-scroll");
	await flush();
	assert.equal(expectError(created, "req-old-scroll").error.code, "STALE_OBSERVATION");
	assert.equal(created.port.countOf("content.scroll"), 0);
	assert.equal(created.port.countOf("content.focus"), 0);
});

test("a scroll shape the wire forbids never reaches the handler or the page", async () => {
	const created = await attached();
	// Either a ref or a delta: both together is a protocol violation, so the connection ends.
	created.cli.request({ op: "scroll", observationId: "obs-old", ref: "n1", deltaY: 10 }, "req-mixed");
	await flush();
	const frames = created.platform.native.frames();
	assert.equal(frames.filter((frame) => frame.type === "bye").length, 1);
	assert.equal(created.port.countOf("content.scroll"), 0);
	assert.equal(created.cli.result("req-mixed"), undefined);
	// The worker's own delta policy can only be reached with a wire-valid frame, so it stays defence in depth.
	assert.ok(MAX_SCROLL_DELTA > 0);
});

test("a second browser operation while one is in flight is refused", async () => {
	const created = await attached();
	created.cli.request({ op: "observe" }, "req-a");
	created.cli.request({ op: "observe" }, "req-b");
	await flush();
	assert.equal(expectError(created, "req-b").error.code, "TOO_MANY_INFLIGHT");
});

test("cancelling an in-flight observe reaches the page and settles one local outcome", async () => {
	const created = await attached();
	created.cli.request({ op: "observe" }, "req-observe");
	assert.ok(created.port.lastOperation());
	created.cli.cancel("req-observe", "req-cancel");
	await flush();
	assert.deepEqual(expectOk(created, "req-cancel").payload, { op: "cancel", cancelled: true, alreadyTerminal: false });
	assert.equal(created.cli.result("req-observe"), undefined, "a confirmed cancel suppresses the original result");
	assert.equal(created.port.countOf("content.cancel"), 1, "the page must be told to stop");
});

test("focus and scroll answers are forwarded with their outcomes", async () => {
	const created = await attached();
	const observationId = await observe(created, "req-obs");
	created.cli.request({ op: "focus", observationId, ref: "n1" }, "req-focus");
	await flush();
	const focus = created.port.lastOperation();
	assert.ok(focus && focus.t === "content.focus");
	created.port.answerAction(focus.requestId as string, "applied");
	await flush();
	assert.deepEqual(expectOk(created, "req-focus").payload, { op: "focus", outcome: "applied" });
	created.cli.request({ op: "scroll", observationId, deltaY: 200 }, "req-scroll");
	await flush();
	const scroll = created.port.lastOperation();
	assert.ok(scroll && scroll.t === "content.scroll");
	created.port.answerAction(scroll.requestId as string, "applied", { scrollX: 0, scrollY: 200 });
	await flush();
	assert.deepEqual(expectOk(created, "req-scroll").payload, {
		op: "scroll",
		outcome: "applied",
		scrollX: 0,
		scrollY: 200,
	});
});

test("a bounded content failure is relayed with its code", async () => {
	const created = await attached();
	created.cli.request({ op: "observe" }, "req-fail");
	const operation = created.port.lastOperation();
	assert.ok(operation);
	created.port.fail(operation.requestId as string, "CAPTURE_FAILED");
	await flush();
	assert.equal(expectError(created, "req-fail").error.code, "CAPTURE_FAILED");
});

test("losing the content port revokes the grant and closes the native session", async () => {
	const created = await attached();
	created.port.onDisconnect.emit();
	await flush();
	assert.equal(created.cli.event()?.reason, "navigated");
	assert.equal(created.cli.event()?.grant, null);
	assert.equal(created.platform.native.lastFrame()?.type, "bye");
	const state = (await created.popup({ t: "popup.state" })) as { state: { phase: string; granted: string[] } };
	assert.equal(state.state.phase, "detached");
	assert.deepEqual(state.state.granted, []);
	// Reattaching is explicit; nothing was replayed.
	await created.attach(ALL_CAPABILITIES);
	assert.equal(created.ports.length, 2);
});

test("closing the tab, replacing the document and expiring the grant each end authority", async () => {
	const closed = await attached();
	closed.platform.removed(TAB_ID);
	assert.equal(closed.cli.event()?.reason, "closed");

	const replaced = await attached();
	replaced.platform.replaced(TAB_ID);
	assert.equal(replaced.cli.event()?.reason, "navigated");

	const discarded = await attached();
	discarded.platform.updated(TAB_ID, { discarded: true });
	assert.equal(discarded.cli.event()?.reason, "navigated");

	const expired = await attached();
	expired.clock.advance(10 * 60 * 1000 + 1);
	assert.equal(expired.cli.event()?.reason, "expired");

	const disconnected = await attached();
	disconnected.platform.native.onDisconnect.emit();
	await flush();
	const state = (await disconnected.popup({ t: "popup.state" })) as { state: { phase: string; granted: string[] } };
	assert.equal(state.state.phase, "detached", "a CLI disconnect ends authority locally");
	assert.deepEqual(state.state.granted, []);
});

test("detach ends the grant and closes the page channel", async () => {
	const created = await attached();
	const response = (await created.popup({ t: "popup.detach" })) as { state: { phase: string; granted: string[] } };
	assert.equal(response.state.phase, "detached");
	assert.deepEqual(response.state.granted, []);
	assert.equal(created.cli.event()?.reason, "detached");
	assert.equal(created.port.countOf("content.detach"), 1);
	assert.equal(created.port.disconnected, true);
});

test("a CLI event that carries a grant is a protocol violation, not an authorization", async () => {
	const created = await attached();
	created.cli.eventWithGrant(created.grant);
	await flush();
	const frames = created.platform.native.frames();
	const bye = frames.filter((frame) => frame.type === "bye").at(-1);
	assert.ok(bye && bye.type === "bye");
	assert.equal(bye.reason, "protocol-error");
	assert.equal(created.cli.event()?.grant, null);
});

test("a pending port-loss identity check cannot revoke a grant created after it", async () => {
	const created = await attached({ gateTabLookup: true });
	// Port loss starts an asynchronous tab lookup that is still pending when a new attach completes.
	created.port.onDisconnect.emit();
	await flush();
	await created.popup({ t: "popup.detach" });
	await created.attach(ALL_CAPABILITIES);
	const secondGrant = created.cli.event()?.grant ?? null;
	assert.ok(secondGrant, "the second attach must announce its grant");
	created.platform.releaseTabLookup();
	await flush();
	assert.equal(
		created.cli.event()?.grant?.grantId,
		secondGrant.grantId,
		"a stale port-loss continuation must not clear a newer grant",
	);
	const state = (await created.popup({ t: "popup.state" })) as { state: { phase: string } };
	assert.equal(state.state.phase, "attached");
});

test("popup state reports the bound tab without query strings or fragments", async () => {
	const created = await attached();
	const response = (await created.popup({ t: "popup.state" })) as {
		state: { phase: string; granted: string[]; tab?: { tabId: number; url?: string } };
	};
	assert.equal(response.state.phase, "attached");
	assert.deepEqual(response.state.granted, ALL_CAPABILITIES);
	assert.equal(response.state.tab?.tabId, TAB_ID);
	assert.equal(response.state.tab?.url, "https://example.test/page");
});
