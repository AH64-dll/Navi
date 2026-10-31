/**
 * Security boundary tests for the browser side: worker authority, sender authentication,
 * grant/document binding, revocation, replay, active-tab guards and content-port discipline.
 *
 * Owned by: foundation-security. Evidence type: API-level tests against the landed worker APIs with a
 * narrow fake platform. They do NOT prove live Chrome behavior. Real native registration, real popup
 * sender identity, BFCache/discard/replacement, worker restart and real site focus/scroll side effects
 * remain a manual browser gate (docs/navi-foundation/architecture.md, "Required verification").
 *
 * Deterministic by construction: no real timers and no sleeping. The fake clock is advanced explicitly,
 * the fake native host answers the handshake, and every awaited promise settles through microtasks.
 */
import assert from "node:assert/strict";
import test from "node:test";
import type { Clock } from "../../src/core/ports.ts";
import { MAX_SCROLL_DELTA } from "../../src/protocol/limits.ts";
import { createRefStore, createRefTable } from "../content/refs.ts";
import type { MessageSender, NativePortApi, Platform, RuntimePortApi } from "../platform/types.ts";
import { GRANT_IDLE_MS } from "../worker/grant.ts";
import {
	CONTENT_PORT_NAME,
	LocalMessageError,
	parseContentInbound,
	parseContentOutbound,
	parsePopupRequest,
} from "../worker/local-channel.ts";
import { CONTENT_BIND_TIMEOUT_MS, createWorker, type Worker } from "../worker/worker.ts";

class FakeClock implements Clock {
	private time = 0;
	private order = 0;
	private timers: { at: number; order: number; callback: () => void }[] = [];

	now(): number {
		return this.time;
	}

	schedule(callback: () => void, delayMs: number): () => void {
		const timer = { at: this.time + delayMs, order: this.order, callback };
		this.order += 1;
		this.timers.push(timer);
		return () => {
			this.timers = this.timers.filter((candidate) => candidate !== timer);
		};
	}

	advance(ms: number): void {
		const target = this.time + ms;
		for (;;) {
			const due = this.timers
				.slice()
				.sort((left, right) => left.at - right.at || left.order - right.order)
				.find((timer) => timer.at <= target);
			if (due === undefined) break;
			this.timers = this.timers.filter((timer) => timer !== due);
			this.time = Math.max(this.time, due.at);
			due.callback();
		}
		this.time = target;
	}
}

/** Lets every queued microtask run. The fakes resolve synchronously, so this is enough. */
async function pump(times = 20): Promise<void> {
	for (let index = 0; index < times; index++) await Promise.resolve();
}

/** The CLI side of the native hop. Answers `hello` and records every frame the extension sends. */
class FakeNativePort {
	readonly posted: Record<string, unknown>[] = [];
	readonly connectionId: string;
	readonly port: NativePortApi;
	disconnected = false;
	ackCapabilities: string[] = ["observe"];
	private seq = 0;
	private readonly messageListeners: ((value: unknown) => void)[] = [];
	private readonly disconnectListeners: (() => void)[] = [];

	constructor(connectionId: string) {
		this.connectionId = connectionId;
		this.port = {
			postMessage: (message: unknown): void => this.onExtensionFrame(message),
			disconnect: (): void => this.disconnectFromExtension(),
			onMessage: {
				addListener: (listener: (value: unknown) => void): void => void this.messageListeners.push(listener),
			},
			onDisconnect: {
				addListener: (listener: () => void): void => void this.disconnectListeners.push(listener),
			},
		};
	}

	private onExtensionFrame(message: unknown): void {
		const frame = message as Record<string, unknown>;
		this.posted.push(frame);
		if (frame.type !== "hello") return;
		queueMicrotask(() => {
			this.deliver({
				v: 1,
				seq: ++this.seq,
				type: "hello.ack",
				connectionId: this.connectionId,
				peer: { kind: "cli", version: "0.0.1" },
				capabilities: this.ackCapabilities,
			});
		});
	}

	private disconnectFromExtension(): void {
		if (this.disconnected) return;
		this.disconnected = true;
		for (const listener of this.disconnectListeners) listener();
	}

	deliver(value: unknown): void {
		for (const listener of this.messageListeners) listener(value);
	}

	sendRequest(requestId: string, payload: Record<string, unknown>): void {
		this.deliver({ v: 1, seq: ++this.seq, type: "request", connectionId: this.connectionId, requestId, payload });
	}

	sendEvent(payload: Record<string, unknown>): void {
		this.deliver({
			v: 1,
			seq: ++this.seq,
			type: "event",
			connectionId: this.connectionId,
			kind: "binding.changed",
			payload,
		});
	}

	framesOfType(type: string): Record<string, unknown>[] {
		return this.posted.filter((frame) => frame.type === type);
	}

	results(): Record<string, unknown>[] {
		return this.framesOfType("result");
	}

	resultFor(requestId: string): Record<string, unknown> | undefined {
		return this.results().find((frame) => frame.requestId === requestId);
	}
}

/** A fake injected content script port. */
class FakeContentPort {
	readonly name: string;
	readonly sender: MessageSender;
	readonly port: RuntimePortApi;
	readonly received: Record<string, unknown>[] = [];
	disconnected = false;
	private readonly messageListeners: ((value: unknown) => void)[] = [];
	private readonly disconnectListeners: (() => void)[] = [];

	constructor(options: { sender: MessageSender; name?: string }) {
		this.name = options.name ?? CONTENT_PORT_NAME;
		this.sender = options.sender;
		this.port = {
			name: this.name,
			sender: this.sender,
			postMessage: (message: unknown): void => void this.received.push(message as Record<string, unknown>),
			disconnect: (): void => this.simulateDisconnect(),
			onMessage: {
				addListener: (listener: (value: unknown) => void): void => void this.messageListeners.push(listener),
			},
			onDisconnect: {
				addListener: (listener: () => void): void => void this.disconnectListeners.push(listener),
			},
		};
	}

	private simulateDisconnect(): void {
		if (this.disconnected) return;
		this.disconnected = true;
		for (const listener of this.disconnectListeners) listener();
	}

	/** Simulates Chrome ending the port: navigation, script teardown or tab close. */
	drop(): void {
		this.simulateDisconnect();
	}

	deliver(value: unknown): void {
		for (const listener of this.messageListeners) listener(value);
	}

	of(name: string): Record<string, unknown>[] {
		return this.received.filter((message) => message.t === name);
	}
}

type PlatformKnobs = {
	activeTabId: number;
	windowFocused: boolean;
	injection: { frameId: number; documentId?: string }[];
	connectNativeThrows: boolean;
	tabGetPending: boolean;
	releaseTabGet: () => void;
	tabGetCalls: number;
	connectNativeCalls: number;
};

type Harness = {
	worker: Worker;
	clock: FakeClock;
	knobs: PlatformKnobs;
	hosts: FakeNativePort[];
	lastPort: () => FakeContentPort | null;
	host: () => FakeNativePort;
	contentSender: (tabId: number, documentId?: string) => MessageSender;
	/** Starts an attach, delivers the injected port, then awaits the popup response. */
	attach: (options?: {
		capabilities?: string[];
		documentId?: string;
		portDocumentId?: string | undefined;
		portTabId?: number;
	}) => Promise<{
		response: { state: { phase: string; error?: { code: string }; granted: string[] } };
		port: FakeContentPort;
	}>;
	/** Starts an attach and stops before the content port exists, so the caller can race it. */
	beginAttach: (options?: { capabilities?: string[] }) => Promise<unknown>;
	observe: (
		requestId?: string,
	) => Promise<{ binding: Record<string, unknown>; observationId: string; port: FakeContentPort }>;
	grantFromEvent: () => Record<string, unknown>;
	popupSender: MessageSender;
	detach: () => Promise<unknown>;
};

const POPUP_SENDER: MessageSender = { id: "ext-1", url: "chrome-extension://ext-1/popup.html", frameId: 0 };

function observationDraft(observationId: string): Record<string, unknown> {
	return {
		observationId,
		capturedAt: 1,
		url: "https://example.test/page",
		nodes: [
			{ ref: "n1", depth: 0, tag: "p", interactive: false, editable: false, offscreen: false, text: "page text" },
		],
		visited: 1,
		truncated: false,
		omitted: [],
	};
}

function createHarness(options: { ackCapabilities?: string[] } = {}): Harness {
	const clock = new FakeClock();
	const hosts: FakeNativePort[] = [];
	let lastPort: FakeContentPort | null = null;
	const knobs: PlatformKnobs = {
		activeTabId: 7,
		windowFocused: true,
		injection: [{ frameId: 0, documentId: "doc-A" }],
		connectNativeThrows: false,
		tabGetPending: false,
		releaseTabGet: () => undefined,
		tabGetCalls: 0,
		connectNativeCalls: 0,
	};

	const platform: Platform = {
		runtime: {
			id: "ext-1",
			getURL: (path: string) => `chrome-extension://ext-1/${path}`,
			connectNative: () => {
				knobs.connectNativeCalls += 1;
				if (knobs.connectNativeThrows) throw new Error("no connector is registered");
				const host = new FakeNativePort(`conn-${hosts.length + 1}`);
				host.ackCapabilities = options.ackCapabilities ?? ["observe"];
				hosts.push(host);
				return host.port;
			},
			onMessage: { addListener: () => undefined },
			onConnect: { addListener: () => undefined },
		},
		tabs: {
			get: async (tabId: number) => {
				knobs.tabGetCalls += 1;
				if (knobs.tabGetPending)
					await new Promise<void>((resolve) => {
						knobs.releaseTabGet = resolve;
					});
				return { id: tabId, active: tabId === knobs.activeTabId };
			},
			query: async () => [
				{ id: knobs.activeTabId, active: true, url: "https://example.test/page", title: "Example" },
			],
			onRemoved: { addListener: () => undefined },
			onUpdated: { addListener: () => undefined },
			onReplaced: { addListener: () => undefined },
		},
		windows: {
			getLastFocused: async () => ({ id: 1, focused: knobs.windowFocused }),
			onFocusChanged: { addListener: () => undefined },
		},
		scripting: {
			executeScript: async () => knobs.injection.map((entry) => ({ ...entry })),
		},
	};

	let idCounter = 0;
	const worker = createWorker({ platform, clock, newId: () => `id-${++idCounter}` });

	function contentSender(tabId: number, documentId?: string): MessageSender {
		const sender: MessageSender = {
			id: "ext-1",
			frameId: 0,
			tab: { id: tabId, windowId: 1, active: true },
			url: "https://example.test/page",
		};
		if (documentId !== undefined) sender.documentId = documentId;
		return sender;
	}

	function host(): FakeNativePort {
		const current = hosts[hosts.length - 1];
		assert.ok(current, "no native session was opened");
		return current;
	}

	function grantFromEvent(): Record<string, unknown> {
		const event = host().framesOfType("event")[0];
		assert.ok(event, "the extension must announce the grant");
		return (event.payload as { grant: Record<string, unknown> }).grant;
	}

	async function beginAttach(attachOptions: { capabilities?: string[] } = {}): Promise<unknown> {
		return await worker.handlePopupMessage(
			{ t: "popup.attach", capabilities: attachOptions.capabilities ?? ["observe"] },
			POPUP_SENDER,
		);
	}

	async function attach(
		attachOptions: {
			capabilities?: string[];
			documentId?: string;
			portDocumentId?: string | undefined;
			portTabId?: number;
		} = {},
	): Promise<{
		response: { state: { phase: string; error?: { code: string }; granted: string[] } };
		port: FakeContentPort;
	}> {
		const pending = beginAttach({ capabilities: attachOptions.capabilities });
		// Let the tab lookup, the native handshake and the injection settle while attach waits for the port.
		await pump();
		const documentId = attachOptions.documentId ?? "doc-A";
		const portDocumentId = "portDocumentId" in attachOptions ? attachOptions.portDocumentId : documentId;
		const port = new FakeContentPort({
			sender: contentSender(attachOptions.portTabId ?? 7, portDocumentId),
		});
		lastPort = port;
		worker.handleContentPort(port.port);
		const response = (await pending) as { state: { phase: string; error?: { code: string }; granted: string[] } };
		return { response, port };
	}

	async function observe(
		requestId = "cli-observe-1",
	): Promise<{ binding: Record<string, unknown>; observationId: string; port: FakeContentPort }> {
		const session = host();
		const binding = (grantFromEvent().binding ?? {}) as Record<string, unknown>;
		session.sendRequest(requestId, { op: "observe", binding });
		await pump();
		const port = lastPort;
		assert.ok(port, "no content port was adopted");
		const ask = port.of("content.observe")[0];
		assert.ok(ask, "the worker must ask the page to observe");
		const contentRequestId = String(ask.requestId);
		port.deliver({ t: "content.observation", requestId: contentRequestId, draft: observationDraft("obs-1") });
		await pump();
		assert.ok(session.resultFor(requestId), "the worker must answer the observe request");
		return { binding, observationId: "obs-1", port };
	}

	return {
		worker,
		clock,
		knobs,
		hosts,
		lastPort: () => lastPort,
		host,
		contentSender,
		attach,
		beginAttach,
		observe,
		grantFromEvent,
		popupSender: POPUP_SENDER,
		detach: async () => await worker.handlePopupMessage({ t: "popup.detach" }, POPUP_SENDER),
	};
}

test("only the extension's own popup page can drive attach", async () => {
	const harness = createHarness();
	const impostors: [string, MessageSender][] = [
		["foreign extension id", { ...POPUP_SENDER, id: "ext-other" }],
		["content-script sender", { id: "ext-1", url: "https://example.test/page", frameId: 0, tab: { id: 7 } }],
		["another extension page", { ...POPUP_SENDER, url: "chrome-extension://ext-1/other.html" }],
		["another extension's popup path", { ...POPUP_SENDER, url: "chrome-extension://ext-other/popup.html" }],
	];
	for (const [label, sender] of impostors) {
		const response = (await harness.worker.handlePopupMessage(
			{ t: "popup.attach", capabilities: ["observe"] },
			sender,
		)) as { state: { phase: string; granted: string[] } };
		assert.equal(response.state.phase, "detached", `${label} must not attach`);
		assert.deepEqual(response.state.granted, [], `${label} must not receive a grant`);
	}
	assert.equal(harness.knobs.connectNativeCalls, 0, "an unrecognized sender must not open the native channel");
	assert.equal(harness.knobs.tabGetCalls, 0, "an unrecognized sender must not trigger browser work");
	assert.equal(harness.worker.state().phase, "detached");

	const attached = await harness.attach();
	assert.equal(attached.response.state.phase, "attached", "the real popup still attaches");
	assert.equal(harness.knobs.connectNativeCalls, 1);
});

test("a content port is rejected unless it is our own top-frame content script", async () => {
	const harness = createHarness();
	await harness.attach();
	const before = harness.host().framesOfType("event").length;

	const impostors: [string, FakeContentPort][] = [
		["wrong port name", new FakeContentPort({ sender: harness.contentSender(7, "doc-A"), name: "navi.other" })],
		[
			"foreign extension id",
			new FakeContentPort({ sender: { ...harness.contentSender(7, "doc-A"), id: "ext-other" } }),
		],
		["non top frame", new FakeContentPort({ sender: { ...harness.contentSender(7, "doc-A"), frameId: 3 } })],
		[
			"sender without a tab",
			new FakeContentPort({ sender: { id: "ext-1", frameId: 0, url: "chrome-extension://ext-1/x.html" } }),
		],
		["no sender at all", new FakeContentPort({ sender: { id: undefined, frameId: 0 } })],
	];
	for (const [label, port] of impostors) {
		harness.worker.handleContentPort(port.port);
		assert.equal(port.disconnected, true, `${label} must be disconnected`);
	}
	assert.equal(harness.worker.state().phase, "attached", "the real grant is unaffected");
	assert.equal(harness.host().framesOfType("event").length, before, "an impostor must not produce a grant");
});

test("a port for another document never satisfies the bind for the injected document", async () => {
	const harness = createHarness();
	const pending = harness.beginAttach();
	await pump();
	const stale = new FakeContentPort({ sender: harness.contentSender(7, "doc-B") });
	harness.worker.handleContentPort(stale.port);
	assert.equal(harness.worker.state().phase, "attaching", "an unmatched port must not complete the bind");
	assert.equal(stale.of("content.observe").length, 0, "an unmatched port receives no work");
	harness.clock.advance(CONTENT_BIND_TIMEOUT_MS + 1);
	const response = (await pending) as { state: { phase: string; error?: { code: string } } };
	assert.equal(response.state.phase, "detached", "a port for another document must not complete the bind");
	assert.equal(response.state.error?.code, "CAPTURE_FAILED");
	assert.equal(stale.of("content.observe").length, 0, "the foreign port must receive no work");
	assert.equal(stale.of("content.focus").length, 0);
	assert.equal(harness.host().framesOfType("event").length, 0, "no grant may be announced");
});

test("injection must report exactly one top-frame document", async () => {
	const cases: [string, { frameId: number; documentId?: string }[]][] = [
		["no results", []],
		[
			"two results",
			[
				{ frameId: 0, documentId: "doc-A" },
				{ frameId: 0, documentId: "doc-A" },
			],
		],
		["a subframe result", [{ frameId: 1, documentId: "doc-A" }]],
		["a missing document id", [{ frameId: 0 }]],
		["an empty document id", [{ frameId: 0, documentId: "" }]],
		["an over-long document id", [{ frameId: 0, documentId: "d".repeat(65) }]],
	];
	for (const [label, injection] of cases) {
		const harness = createHarness();
		harness.knobs.injection = injection;
		const response = (await harness.beginAttach()) as { state: { phase: string; error?: { code: string } } };
		assert.equal(response.state.phase, "detached", `${label} must not attach`);
		assert.equal(response.state.error?.code, "CAPTURE_FAILED", label);
		assert.equal(harness.host().framesOfType("event").length, 0, `${label} must not announce a grant`);
	}
});

test("the CLI acknowledgement is a ceiling, never a grant", async () => {
	const harness = createHarness({ ackCapabilities: ["observe", "focus", "scroll"] });
	const attached = await harness.attach({ capabilities: ["observe"] });
	assert.equal(attached.response.state.phase, "attached");
	assert.deepEqual(attached.response.state.granted, ["observe"], "the user's choice caps the acknowledged set");

	const { binding, observationId } = await harness.observe();
	harness.host().sendRequest("cli-focus-1", { op: "focus", binding, observationId, ref: "n1" });
	await pump();
	const result = harness.host().resultFor("cli-focus-1") as { ok: boolean; error?: { code: string } };
	assert.equal(result.ok, false);
	assert.equal(result.error?.code, "UNSUPPORTED_OP", "focus was never granted");
	assert.equal(attached.port.of("content.focus").length, 0, "the page must not be asked to focus");
});

test("an acknowledgement without observe cannot produce a grant", async () => {
	const harness = createHarness({ ackCapabilities: ["focus"] });
	const response = (await harness.beginAttach({ capabilities: ["observe", "focus"] })) as {
		state: { phase: string; error?: { code: string }; granted: string[] };
	};
	assert.equal(response.state.phase, "detached");
	assert.equal(response.state.error?.code, "BAD_ARGUMENT");
	assert.deepEqual(response.state.granted, []);
});

test("navigation revokes the grant, the references and the native session", async () => {
	const harness = createHarness();
	await harness.attach();
	await harness.observe();
	const session = harness.host();
	const port = harness.lastPort();
	assert.ok(port);

	port.deliver({ t: "content.navigated" });
	await pump();
	assert.equal(harness.worker.state().phase, "detached", "navigation must end the grant");
	assert.deepEqual((harness.worker.state() as { granted: string[] }).granted, []);
	assert.equal(session.disconnected, true, "the native session must close");
	assert.equal(port.disconnected, true, "the page channel must close");
});

test("tab close and discard revoke the grant", async () => {
	const closed = createHarness();
	await closed.attach();
	closed.worker.handleTabRemoved(8);
	assert.equal(closed.worker.state().phase, "attached", "another tab must not revoke this grant");
	closed.worker.handleTabRemoved(7);
	assert.equal(closed.worker.state().phase, "detached");

	const discarded = createHarness();
	await discarded.attach();
	discarded.worker.handleTabUpdated(7, { status: "complete" });
	assert.equal(discarded.worker.state().phase, "attached", "an ordinary update must not revoke");
	discarded.worker.handleTabUpdated(7, { discarded: true });
	assert.equal(discarded.worker.state().phase, "detached");
});

test("an idle grant expires on the injected clock", async () => {
	const harness = createHarness();
	await harness.attach();
	harness.clock.advance(GRANT_IDLE_MS - 1);
	assert.equal(harness.worker.state().phase, "attached");
	harness.clock.advance(2);
	assert.equal(harness.worker.state().phase, "detached", "the grant must expire without page activity");
});

test("focus is refused while the attached tab is not active and the tab is never activated", async () => {
	const harness = createHarness({ ackCapabilities: ["observe", "focus"] });
	await harness.attach({ capabilities: ["observe", "focus"] });
	const { binding, observationId, port } = await harness.observe();
	harness.knobs.activeTabId = 99;

	harness.host().sendRequest("cli-focus-2", { op: "focus", binding, observationId, ref: "n1" });
	await pump();
	const result = harness.host().resultFor("cli-focus-2") as { ok: boolean; error?: { code: string } };
	assert.equal(result.error?.code, "TAB_NOT_ACTIVE");
	assert.equal(port.of("content.focus").length, 0, "NAVI must not ask the page for a background focus");
	assert.equal(harness.knobs.activeTabId, 99, "NAVI must never change the active tab");
});

test("focus is refused when the browser window is not focused", async () => {
	const harness = createHarness({ ackCapabilities: ["observe", "focus"] });
	await harness.attach({ capabilities: ["observe", "focus"] });
	const { binding, observationId, port } = await harness.observe();
	harness.knobs.windowFocused = false;

	harness.host().sendRequest("cli-focus-3", { op: "focus", binding, observationId, ref: "n1" });
	await pump();
	const result = harness.host().resultFor("cli-focus-3") as { ok: boolean; error?: { code: string } };
	assert.equal(result.error?.code, "TAB_NOT_ACTIVE");
	assert.equal(port.of("content.focus").length, 0);
});

test("a replayed CLI request id is refused and reaches the page once", async () => {
	const harness = createHarness();
	await harness.attach();
	const { binding, port } = await harness.observe("cli-observe-9");
	assert.equal(port.of("content.observe").length, 1);

	harness.host().sendRequest("cli-observe-9", { op: "observe", binding });
	await pump();
	const results = harness
		.host()
		.results()
		.filter((frame) => frame.requestId === "cli-observe-9");
	assert.equal(results.length, 2, "the replay must be answered, not silently dropped");
	const last = results[results.length - 1] as { ok: boolean; error?: { code: string } };
	assert.equal(last.ok, false);
	assert.equal(last.error?.code, "DUPLICATE_REQUEST");
	assert.equal(port.of("content.observe").length, 1, "the page must be asked exactly once");
});

test("a CLI event cannot set a grant", async () => {
	const harness = createHarness();
	await harness.attach();
	const session = harness.host();

	session.sendEvent({
		grant: {
			grantId: "g-cli",
			binding: { grantId: "g-cli", tabId: 7, documentId: "doc-A", pageEpoch: 1 },
			capabilities: ["observe", "focus", "scroll"],
		},
		reason: "attached",
	});
	await pump();
	assert.equal(harness.worker.state().phase, "detached", "the session ends instead of adopting a CLI grant");
	assert.equal(session.disconnected, true);
	assert.deepEqual(
		(harness.worker.state() as { granted: string[] }).granted,
		[],
		"no CLI-supplied authority survives",
	);
});

test("no malformed or ambiguous scroll request reaches the page", async () => {
	const cases: [string, (binding: Record<string, unknown>, observationId: string) => Record<string, unknown>][] = [
		[
			"a reference plus a delta",
			(binding, observationId) => ({ op: "scroll", binding, observationId, ref: "n1", deltaY: 10 }),
		],
		[
			"an over-limit delta",
			(binding, observationId) => ({ op: "scroll", binding, observationId, deltaY: MAX_SCROLL_DELTA + 1 }),
		],
		[
			"alignment without a reference",
			(binding, observationId) => ({ op: "scroll", binding, observationId, align: "end" }),
		],
		["no target at all", (binding, observationId) => ({ op: "scroll", binding, observationId })],
	];
	for (const [label, build] of cases) {
		const harness = createHarness({ ackCapabilities: ["observe", "scroll"] });
		await harness.attach({ capabilities: ["observe", "scroll"] });
		const { binding, observationId, port } = await harness.observe();
		harness.host().sendRequest("cli-scroll-1", build(binding, observationId));
		await pump();
		const session = harness.host();
		const result = session.resultFor("cli-scroll-1") as { ok: boolean; error?: { code: string } } | undefined;
		assert.equal(
			result !== undefined || session.disconnected,
			true,
			`${label} must be refused by a result or a closed link, not ignored`,
		);
		if (result !== undefined) assert.equal(result.ok, false, label);
		assert.equal(port.of("content.scroll").length, 0, `${label} must not reach the page`);
	}
});

test("a content port buffered before an attach is never adopted by that attach", async () => {
	// A port can only be buffered while it is unmatched. Binding it to a later grant would give the
	// grant a channel into a document this attach never injected into.
	const harness = createHarness();
	const stale = new FakeContentPort({ sender: harness.contentSender(7, undefined) });
	harness.worker.handleContentPort(stale.port);
	assert.equal(harness.worker.state().phase, "detached");

	const pending = harness.beginAttach();
	await pump();
	harness.clock.advance(CONTENT_BIND_TIMEOUT_MS + 1);
	const response = (await pending) as { state: { phase: string; error?: { code: string } } };
	assert.equal(response.state.phase, "detached", "a pre-existing port must not satisfy the bind");
	assert.equal(response.state.error?.code, "CAPTURE_FAILED");
	assert.equal(stale.of("content.observe").length, 0, "the stale port must receive no work");
});

test("a pending port-loss check cannot revoke a grant created after it", async () => {
	const harness = createHarness();
	await harness.attach();
	const first = harness.lastPort();
	assert.ok(first);

	harness.knobs.tabGetPending = true;
	first.drop();
	await pump();
	assert.equal(harness.knobs.tabGetCalls, 1, "port loss starts one tab check");

	await harness.detach();
	harness.knobs.activeTabId = 9;
	harness.knobs.injection = [{ frameId: 0, documentId: "doc-C" }];
	const second = await harness.attach({ documentId: "doc-C", portTabId: 9 });
	assert.equal(second.response.state.phase, "attached", "the new attach must stand on its own");

	harness.knobs.tabGetPending = false;
	harness.knobs.releaseTabGet();
	await pump();
	assert.equal(
		harness.worker.state().phase,
		"attached",
		"a stale port-loss continuation must not revoke the current grant",
	);
	assert.equal(
		harness.host().framesOfType("event").length,
		1,
		"the current grant must not be revoked and re-announced",
	);
});

test("references are opaque handles, never selectors or ids", async () => {
	const table = createRefTable("obs-1");
	const element = { isConnected: true } as unknown as Element;
	const ref = table.put(element);
	assert.equal(ref, "n1");
	assert.equal(table.get(ref), element);
	for (const candidate of ["main", "#main", ".content", "n0", "n1 ", "n1x", "n01", "[id=main]", "n".repeat(70)]) {
		assert.equal(table.get(candidate), undefined, `${candidate} must not resolve`);
	}
	(element as unknown as { isConnected: boolean }).isConnected = false;
	assert.equal(table.get(ref), undefined, "a detached element must stop resolving");
	table.revoke();
	assert.equal(table.get(ref), undefined, "a revoked table must resolve nothing");

	const store = createRefStore();
	const firstTable = store.begin("obs-1");
	const firstRef = firstTable.put({ isConnected: true } as unknown as Element);
	store.begin("obs-2");
	assert.equal(store.get(firstRef), undefined, "a new observation must invalidate older handles");
	assert.equal(firstTable.get(firstRef), undefined);
});

test("local worker/content schemas reject unknown fields, capabilities and oversized text", () => {
	assert.throws(() => parsePopupRequest({ t: "popup.attach", capabilities: ["click"] }), LocalMessageError);
	assert.throws(
		() => parsePopupRequest({ t: "popup.attach", capabilities: ["observe", "observe"] }),
		LocalMessageError,
	);
	assert.throws(
		() => parsePopupRequest({ t: "popup.attach", capabilities: ["observe"], approved: true }),
		LocalMessageError,
	);
	assert.throws(() => parsePopupRequest({ t: "popup.detach", capabilities: ["observe"] }), LocalMessageError);
	assert.throws(
		() => parseContentOutbound({ t: "content.observation", requestId: "r1", draft: {}, extra: 1 }),
		LocalMessageError,
	);
	assert.throws(() => parseContentOutbound({ t: "content.navigated", x: 1 }), LocalMessageError);
	assert.throws(
		() => parseContentOutbound({ t: "content.failure", requestId: "r1", code: "INTERNAL", message: "x".repeat(201) }),
		LocalMessageError,
	);
	assert.throws(
		() => parseContentInbound({ t: "content.focus", requestId: "r1", ref: "n1", approved: true }),
		LocalMessageError,
	);
	assert.throws(() => parseContentInbound({ t: "content.observe" }), LocalMessageError);
	// The valid shapes still parse, so the rejection is about the exact schema.
	assert.deepEqual(parsePopupRequest({ t: "popup.attach", capabilities: ["observe"] }), {
		t: "popup.attach",
		capabilities: ["observe"],
	});
	assert.equal(parseContentInbound({ t: "content.cancel", requestId: "r1" }).t, "content.cancel");
});
