/**
 * Core contract tests: handshake, sequence, correlation, one-in-flight, deadlines, cancellation,
 * disconnect outcomes, bounded request-id memory, the grant lifecycle and the CLI client.
 *
 * Deterministic: a fake channel and a fake clock replace the socket and real timers.
 */
import assert from "node:assert/strict";
import test from "node:test";
import { BrowserClient } from "../src/core/client.ts";
import type { ConnectionLimits, RequestHandler } from "../src/core/connection.ts";
import { Connection } from "../src/core/connection.ts";
import { capabilityGranted, GrantManager } from "../src/core/grants.ts";
import type { ChannelCloseReason, Clock } from "../src/core/ports.ts";
import { NaviRequestError } from "../src/core/ports.ts";
import { NAVI_PROTOCOL_VERSION } from "../src/protocol/limits.ts";
import type {
	Binding,
	ByeFrame,
	EventFrame,
	Frame,
	Grant,
	GrantChangeReason,
	HelloAckFrame,
	HelloErrorFrame,
	HelloFrame,
	NaviError,
	Observation,
	RequestFrame,
	RequestPayload,
	ResultPayload,
} from "../src/protocol/types.ts";

const WORKER_PEER = { kind: "extension", version: "0.0.1" } as const;

class FakeClock implements Clock {
	private time = 0;
	private seq = 0;
	private timers: { at: number; order: number; callback: () => void }[] = [];

	now(): number {
		return this.time;
	}

	schedule(callback: () => void, delayMs: number): () => void {
		const timer = { at: this.time + delayMs, order: this.seq, callback };
		this.seq += 1;
		this.timers.push(timer);
		return () => {
			this.timers = this.timers.filter((candidate) => candidate !== timer);
		};
	}

	advance(ms: number): void {
		const target = this.time + ms;
		for (;;) {
			const due = this.timers
				.filter((timer) => timer.at <= target)
				.sort((left, right) => left.at - right.at || left.order - right.order)[0];
			if (due === undefined) break;
			this.timers = this.timers.filter((timer) => timer !== due);
			this.time = Math.max(this.time, due.at);
			due.callback();
		}
		this.time = target;
	}
}

class FakeChannel {
	readonly sent: Frame[] = [];
	closed = false;
	private messageHandlers: ((raw: unknown) => void)[] = [];
	private closeHandlers: ((reason: ChannelCloseReason) => void)[] = [];

	send(frame: Frame): void {
		if (this.closed) throw new Error("channel is closed");
		this.sent.push(frame);
	}

	onMessage(handler: (raw: unknown) => void): void {
		this.messageHandlers.push(handler);
	}

	onClose(handler: (reason: ChannelCloseReason) => void): void {
		this.closeHandlers.push(handler);
	}

	close(): void {
		if (this.closed) return;
		this.closed = true;
		for (const handler of this.closeHandlers) handler("local-closed");
	}

	/** Delivers a peer frame while the link stays open. */
	deliver(raw: unknown): void {
		for (const handler of this.messageHandlers) handler(raw);
	}

	/** Simulates the link failing under an open connection. */
	fail(reason: ChannelCloseReason = "error"): void {
		if (this.closed) return;
		this.closed = true;
		for (const handler of this.closeHandlers) handler(reason);
	}
}

function requests(channel: FakeChannel): RequestFrame[] {
	return channel.sent.filter((frame): frame is RequestFrame => frame.type === "request");
}

function byes(channel: FakeChannel): ByeFrame[] {
	return channel.sent.filter((frame): frame is ByeFrame => frame.type === "bye");
}

function resultFor(channel: FakeChannel, requestId: string): Frame | undefined {
	return channel.sent.find((frame) => frame.type === "result" && frame.requestId === requestId);
}

function errorCodeOf(frame: Frame | undefined): string | undefined {
	return frame !== undefined && frame.type === "result" && frame.ok === false ? frame.error.code : undefined;
}

function payloadOf(frame: Frame | undefined): ResultPayload | undefined {
	return frame !== undefined && frame.type === "result" && frame.ok ? frame.payload : undefined;
}

function helloFrame(seq: number): HelloFrame {
	return { v: NAVI_PROTOCOL_VERSION, seq, type: "hello", peer: { ...WORKER_PEER }, capabilities: ["observe"] };
}

function ackFrame(
	seq: number,
	connectionId: string,
	capabilities: HelloAckFrame["capabilities"] = ["observe"],
): HelloAckFrame {
	return { v: NAVI_PROTOCOL_VERSION, seq, type: "hello.ack", connectionId, peer: { ...WORKER_PEER }, capabilities };
}

function errorFrame(seq: number, code: NaviError["code"]): HelloErrorFrame {
	return { v: NAVI_PROTOCOL_VERSION, seq, type: "hello.error", error: { code, message: "nope", retryable: false } };
}

function requestFrame(seq: number, connectionId: string, requestId: string, payload: RequestPayload): Frame {
	return { v: NAVI_PROTOCOL_VERSION, seq, type: "request", connectionId, requestId, payload };
}

function resultFrame(seq: number, connectionId: string, requestId: string, payload: ResultPayload): Frame {
	return { v: NAVI_PROTOCOL_VERSION, seq, type: "result", connectionId, requestId, ok: true, payload };
}

function errorResultFrame(seq: number, connectionId: string, requestId: string, error: NaviError): Frame {
	return { v: NAVI_PROTOCOL_VERSION, seq, type: "result", connectionId, requestId, ok: false, error };
}

function eventFrame(seq: number, connectionId: string, grant: Grant | null, reason: GrantChangeReason): EventFrame {
	return {
		v: NAVI_PROTOCOL_VERSION,
		seq,
		type: "event",
		connectionId,
		kind: "binding.changed",
		payload: { grant, reason },
	};
}

function bindingFixture(): Binding {
	return { grantId: "g1", tabId: 7, documentId: "doc-1", pageEpoch: 1 };
}

function grantFixture(capabilities: Grant["capabilities"] = ["observe", "focus", "scroll"]): Grant {
	const binding = bindingFixture();
	return { grantId: binding.grantId, binding, capabilities };
}

function observationFixture(binding: Binding, observationId = "obs-1"): Observation {
	return {
		observationId,
		binding,
		capturedAt: 1,
		url: "https://example.com/docs",
		nodes: [{ ref: "n1", depth: 0, tag: "h1", interactive: false, editable: false, offscreen: false, text: "Docs" }],
		visited: 3,
		truncated: false,
		omitted: [],
	};
}

type CliHarness = { clock: FakeClock; channel: FakeChannel; connection: Connection; ack: HelloAckFrame };

function cliHarness(options: { limits?: Partial<ConnectionLimits>; handleRequest?: RequestHandler } = {}): CliHarness {
	const clock = new FakeClock();
	const channel = new FakeChannel();
	const connection = new Connection({
		role: "responder",
		channel,
		clock,
		peer: { kind: "cli", version: "0.0.1" },
		capabilities: ["observe", "focus", "scroll"],
		newConnectionId: () => "conn-1",
		...(options.limits === undefined ? {} : { limits: options.limits }),
		...(options.handleRequest === undefined ? {} : { handleRequest: options.handleRequest }),
	});
	connection.open();
	channel.deliver(helloFrame(1));
	const ack = channel.sent[0];
	assert.equal(ack?.type, "hello.ack");
	return { clock, channel, connection, ack: ack as HelloAckFrame };
}

function clientHarness(): { clock: FakeClock; channel: FakeChannel; client: BrowserClient } {
	const clock = new FakeClock();
	const channel = new FakeChannel();
	const client = new BrowserClient({ channel, clock, newConnectionId: () => "conn-1", peerVersion: "0.0.1" });
	client.open();
	channel.deliver(helloFrame(1));
	return { clock, channel, client };
}

async function rejection(promise: Promise<unknown>): Promise<unknown> {
	return await promise.then(
		() => undefined,
		(error: unknown) => error,
	);
}

function codeOf(error: unknown): string | undefined {
	return error instanceof NaviRequestError ? error.code : undefined;
}

test("handshake: hello is answered once with a minted connection id and our capabilities", () => {
	const { channel, connection, ack } = cliHarness();
	assert.equal(connection.state, "ready");
	assert.equal(ack.connectionId, "conn-1");
	assert.equal(ack.peer.kind, "cli");
	assert.deepStrictEqual(ack.capabilities, ["observe", "focus", "scroll"]);
	assert.equal(ack.seq, 1);
	assert.equal(connection.peer?.kind, "extension");
	assert.equal(channel.sent.filter((frame) => frame.type === "hello.ack").length, 1);
});

test("handshake: a first frame that is not hello closes the link and answers hello.error", () => {
	const clock = new FakeClock();
	const channel = new FakeChannel();
	const connection = new Connection({
		role: "responder",
		channel,
		clock,
		peer: { kind: "cli", version: "0.0.1" },
		capabilities: ["observe"],
		newConnectionId: () => "conn-2",
	});
	connection.open();
	channel.deliver(requestFrame(1, "conn-2", "r1", { op: "observe", binding: bindingFixture() }));
	assert.equal(connection.state, "closed");
	assert.equal(connection.lastError?.code, "NOT_HELLO_FIRST");
	assert.equal(channel.sent[0]?.type, "hello.error");
	assert.equal(channel.closed, true);
});

test("handshake: a second hello on an established connection is rejected", () => {
	const { channel, connection } = cliHarness();
	channel.deliver(helloFrame(2));
	assert.equal(connection.state, "closed");
	assert.equal(connection.lastError?.code, "DUPLICATE_HELLO");
});

test("handshake: the initiator learns the connection id from hello.ack and stops on hello.error", () => {
	const clock = new FakeClock();
	const channel = new FakeChannel();
	const initiator = new Connection({
		role: "initiator",
		channel,
		clock,
		peer: { kind: "extension", version: "0.0.1" },
		capabilities: ["observe"],
	});
	initiator.open();
	assert.equal(channel.sent[0]?.type, "hello");
	assert.equal(channel.sent[0]?.seq, 1);
	channel.deliver(ackFrame(1, "conn-cli"));
	assert.equal(initiator.state, "ready");
	assert.equal(initiator.connectionId, "conn-cli");

	const refusing = new FakeChannel();
	const blocked = new Connection({
		role: "initiator",
		channel: refusing,
		clock,
		peer: { kind: "extension", version: "0.0.1" },
		capabilities: ["observe"],
	});
	blocked.open();
	refusing.deliver(errorFrame(1, "VERSION_MISMATCH"));
	assert.equal(blocked.state, "closed");
	assert.equal(blocked.lastError?.code, "VERSION_MISMATCH");
});

test("handshake: an unanswered hello closes on the handshake deadline", () => {
	const clock = new FakeClock();
	const channel = new FakeChannel();
	const connection = new Connection({
		role: "responder",
		channel,
		clock,
		peer: { kind: "cli", version: "0.0.1" },
		capabilities: ["observe"],
	});
	connection.open();
	clock.advance(5_000);
	assert.equal(connection.state, "closed");
	assert.equal(connection.lastError?.code, "DEADLINE_EXCEEDED");
});

test("sequence: outbound frames increase by exactly one from 1", () => {
	const { channel, connection } = cliHarness();
	connection.sendEvent({ grant: null, reason: "detached" });
	connection.sendEvent({ grant: null, reason: "detached" });
	assert.deepStrictEqual(
		channel.sent.map((frame) => frame.seq),
		[1, 2, 3],
	);
});

test("sequence: an inbound gap closes the connection", () => {
	const { channel, connection } = cliHarness();
	channel.deliver(requestFrame(3, "conn-1", "r1", { op: "observe", binding: bindingFixture() }));
	assert.equal(connection.state, "closed");
	assert.equal(connection.lastError?.code, "SEQUENCE_GAP");
});

test("binding: a frame with another connection id is rejected", () => {
	const { channel, connection } = cliHarness();
	channel.deliver(requestFrame(2, "other-connection", "r1", { op: "observe", binding: bindingFixture() }));
	assert.equal(connection.state, "closed");
	assert.equal(connection.lastError?.code, "CONNECTION_MISMATCH");
});

test("schema gate: malformed inbound values are rejected before dispatch", () => {
	const missing = cliHarness();
	missing.channel.deliver({ v: NAVI_PROTOCOL_VERSION, seq: 2, type: "request" });
	assert.equal(missing.connection.lastError?.code, "BAD_ENVELOPE");

	const notAnObject = cliHarness();
	notAnObject.channel.deliver("not-an-object");
	// A wire error never carries a transport-local code; the precise code stays local for diagnostics.
	assert.equal(notAnObject.connection.lastError?.code, "BAD_ENVELOPE");
	assert.equal(notAnObject.connection.lastErrorDetail, "NOT_JSON_OBJECT");

	const extraField = cliHarness();
	extraField.channel.deliver({
		v: NAVI_PROTOCOL_VERSION,
		seq: 2,
		type: "request",
		connectionId: "conn-1",
		requestId: "r1",
		payload: { op: "observe", binding: bindingFixture() },
		extra: 1,
	});
	assert.equal(extraField.connection.lastError?.code, "BAD_ENVELOPE");

	const version = cliHarness();
	version.channel.deliver({ ...helloFrame(2), v: 99 });
	assert.equal(version.connection.lastError?.code, "VERSION_MISMATCH");
});

test("correlation: a result resolves its request and clears the in-flight slot", async () => {
	const { channel, connection } = cliHarness();
	const binding = bindingFixture();
	const pending = connection.request({ op: "observe", binding });
	const sent = requests(channel);
	assert.equal(sent.length, 1);
	assert.equal(sent[0]?.requestId, "r1");
	assert.equal(connection.inFlightRequestId, "r1");
	channel.deliver(resultFrame(2, "conn-1", "r1", { op: "observe", observation: observationFixture(binding) }));
	const payload = await pending;
	assert.equal(payload.op, "observe");
	assert.equal(connection.inFlightRequestId, undefined);
	assert.equal(connection.state, "ready");
});

test("correlation: an error result rejects with the peer code and no retry", async () => {
	const { channel, connection } = cliHarness();
	const pending = rejection(connection.request({ op: "observe", binding: bindingFixture() }));
	channel.deliver(errorResultFrame(2, "conn-1", "r1", { code: "BINDING_STALE", message: "stale", retryable: false }));
	const error = await pending;
	assert.equal(codeOf(error), "BINDING_STALE");
	assert.equal(requests(channel).length, 1);
});

test("inflight: a second browser operation is refused locally without a frame", async () => {
	const { channel, connection } = cliHarness();
	const pending = connection.request({ op: "observe", binding: bindingFixture() });
	const refused = await rejection(
		connection.request({ op: "focus", binding: bindingFixture(), observationId: "obs-1", ref: "n1" }),
	);
	assert.equal(codeOf(refused), "BUSY");
	assert.equal(requests(channel).length, 1);
	channel.deliver(
		resultFrame(2, "conn-1", "r1", { op: "observe", observation: observationFixture(bindingFixture()) }),
	);
	await pending;
});

test("inflight: inbound overrun is refused, then a repeat of that id is named and answered", async () => {
	let resolveFirst: ((payload: ResultPayload | NaviError) => void) | undefined;
	const handler: RequestHandler = () =>
		new Promise<ResultPayload | NaviError>((resolve) => {
			resolveFirst = resolve;
		});
	const { channel, connection } = cliHarness({ handleRequest: handler });
	channel.deliver(requestFrame(2, "conn-1", "a1", { op: "observe", binding: bindingFixture() }));
	channel.deliver(requestFrame(3, "conn-1", "a2", { op: "observe", binding: bindingFixture() }));
	assert.equal(errorCodeOf(resultFor(channel, "a2")), "TOO_MANY_INFLIGHT");
	await Promise.resolve();
	resolveFirst?.({ op: "observe", observation: observationFixture(bindingFixture()) });
	await Promise.resolve();
	await Promise.resolve();
	assert.equal(payloadOf(resultFor(channel, "a1"))?.op, "observe");
	channel.deliver(requestFrame(4, "conn-1", "a1", { op: "observe", binding: bindingFixture() }));
	assert.equal(errorCodeOf(channel.sent.filter((frame) => frame.type === "result")[2]), "DUPLICATE_REQUEST");
	assert.equal(connection.state, "ready");
});

test("deadline: an observe timeout reports DEADLINE_EXCEEDED and sends no retry", async () => {
	const { clock, channel, connection } = cliHarness();
	const settled = rejection(connection.request({ op: "observe", binding: bindingFixture() }));
	clock.advance(4_000);
	const error = await settled;
	assert.equal(codeOf(error), "DEADLINE_EXCEEDED");
	assert.equal(requests(channel).length, 1);
	assert.equal(connection.inFlightRequestId, undefined);
});

test("deadline: an effect timeout reports OUTCOME_UNKNOWN, never success", async () => {
	const { clock, channel, connection } = cliHarness();
	const settled = rejection(
		connection.request({ op: "scroll", binding: bindingFixture(), observationId: "obs-1", deltaY: 400 }),
	);
	clock.advance(2_000);
	const error = await settled;
	assert.equal(codeOf(error), "OUTCOME_UNKNOWN");
	assert.equal(requests(channel).length, 1);
});

test("disconnect: an in-flight effect is unknown, an in-flight observe is cancelled", async () => {
	const first = cliHarness();
	const effect = rejection(
		first.connection.request({ op: "focus", binding: bindingFixture(), observationId: "obs-1", ref: "n1" }),
	);
	first.channel.fail("error");
	assert.equal(codeOf(await effect), "OUTCOME_UNKNOWN");

	const second = cliHarness();
	const observe = rejection(second.connection.request({ op: "observe", binding: bindingFixture() }));
	second.channel.fail("peer-closed");
	assert.equal(codeOf(await observe), "CANCELLED");
	assert.equal(second.connection.state, "closed");
	assert.equal(
		codeOf(await rejection(second.connection.request({ op: "observe", binding: bindingFixture() }))),
		"INTERNAL",
	);
});

test("cancel: a confirmed cancel settles locally and a late result is ignored", async () => {
	const { channel, connection } = cliHarness();
	const settled = rejection(
		connection.request({ op: "focus", binding: bindingFixture(), observationId: "obs-1", ref: "n1" }),
	);
	const cancel = connection.cancelInFlight();
	const cancelFrame = requests(channel)[1];
	assert.equal(cancelFrame?.payload.op, "cancel");
	assert.equal(cancelFrame?.payload.op === "cancel" ? cancelFrame.payload.requestId : "", "r1");
	channel.deliver(resultFrame(2, "conn-1", "r2", { op: "cancel", cancelled: true, alreadyTerminal: false }));
	assert.equal((await cancel).cancelled, true);
	assert.equal(codeOf(await settled), "OUTCOME_UNKNOWN");
	channel.deliver(resultFrame(3, "conn-1", "r1", { op: "focus", outcome: "applied" }));
	assert.equal(connection.state, "ready");
	assert.equal(byes(channel).length, 0);
});

test("cancel: nothing in flight returns alreadyTerminal without a frame", async () => {
	const { channel, connection } = cliHarness();
	const before = channel.sent.length;
	assert.deepStrictEqual(await connection.cancelInFlight(), { op: "cancel", cancelled: false, alreadyTerminal: true });
	assert.equal(channel.sent.length, before);
});

test("cancel: an inbound cancel aborts the handler and suppresses its result", async () => {
	let handlerSignal: AbortSignal | undefined;
	let release: ((payload: ResultPayload | NaviError) => void) | undefined;
	const handler: RequestHandler = (_request, signal) => {
		handlerSignal = signal;
		return new Promise<ResultPayload | NaviError>((resolve) => {
			release = resolve;
		});
	};
	const { channel } = cliHarness({ handleRequest: handler });
	channel.deliver(
		requestFrame(2, "conn-1", "a1", { op: "focus", binding: bindingFixture(), observationId: "obs-1", ref: "n1" }),
	);
	await Promise.resolve();
	assert.equal(handlerSignal?.aborted, false);
	channel.deliver(requestFrame(3, "conn-1", "c1", { op: "cancel", requestId: "a1" }));
	assert.equal(handlerSignal?.aborted, true);
	assert.deepStrictEqual(payloadOf(resultFor(channel, "c1")), {
		op: "cancel",
		cancelled: true,
		alreadyTerminal: false,
	});
	release?.({ op: "focus", outcome: "applied" });
	await Promise.resolve();
	await Promise.resolve();
	assert.equal(resultFor(channel, "a1"), undefined);
});

test("deadline: an inbound focus without confirmation is OUTCOME_UNKNOWN, never a plain deadline", async () => {
	// The handler never settles, so only the inward deadline can answer.
	const handler: RequestHandler = () => new Promise<ResultPayload | NaviError>(() => undefined);
	const { clock, channel } = cliHarness({ handleRequest: handler });
	const binding = bindingFixture();
	channel.deliver(requestFrame(2, "conn-1", "action-1", { op: "focus", binding, observationId: "obs-1", ref: "n1" }));
	assert.equal(errorCodeOf(resultFor(channel, "action-1")), undefined);
	clock.advance(1_999);
	assert.equal(errorCodeOf(resultFor(channel, "action-1")), undefined);
	clock.advance(1);
	assert.equal(errorCodeOf(resultFor(channel, "action-1")), "OUTCOME_UNKNOWN");
	// Read-only work keeps the plain deadline: nothing reached the page.
	channel.deliver(requestFrame(3, "conn-1", "read-1", { op: "observe", binding }));
	clock.advance(3_999);
	assert.equal(errorCodeOf(resultFor(channel, "read-1")), undefined);
	clock.advance(1);
	assert.equal(errorCodeOf(resultFor(channel, "read-1")), "DEADLINE_EXCEEDED");
});

test("cancel: an unknown request id is refused, not reported as already terminal", async () => {
	const handler: RequestHandler = async () => ({ op: "focus", outcome: "applied" });
	const { channel } = cliHarness({ handleRequest: handler });
	channel.deliver(requestFrame(2, "conn-1", "c1", { op: "cancel", requestId: "never-seen" }));
	assert.equal(errorCodeOf(resultFor(channel, "c1")), "UNKNOWN_REQUEST");
	assert.equal(payloadOf(resultFor(channel, "c1")), undefined);
});

test("cancel: an id that already produced its result is reported as already terminal", async () => {
	const handler: RequestHandler = async () => ({ op: "focus", outcome: "applied" });
	const { channel, connection } = cliHarness({ handleRequest: handler });
	channel.deliver(
		requestFrame(2, "conn-1", "a1", { op: "focus", binding: bindingFixture(), observationId: "obs-1", ref: "n1" }),
	);
	await Promise.resolve();
	await Promise.resolve();
	assert.equal(payloadOf(resultFor(channel, "a1"))?.op, "focus");
	channel.deliver(requestFrame(3, "conn-1", "c1", { op: "cancel", requestId: "a1" }));
	assert.deepStrictEqual(payloadOf(resultFor(channel, "c1")), {
		op: "cancel",
		cancelled: false,
		alreadyTerminal: true,
	});
	assert.equal(connection.state, "ready");
});

test("bounded ids: the request-id budget closes the link instead of growing memory", async () => {
	const handler: RequestHandler = async () => ({ op: "observe", observation: observationFixture(bindingFixture()) });
	const { channel, connection } = cliHarness({
		handleRequest: handler,
		limits: { maxRequestIdsPerConnection: 3, trackedRequestIds: 2 },
	});
	for (let index = 0; index < 3; index++) {
		channel.deliver(requestFrame(2 + index, "conn-1", `a${index}`, { op: "observe", binding: bindingFixture() }));
		await Promise.resolve();
		await Promise.resolve();
		await Promise.resolve();
	}
	assert.equal(connection.state, "ready");
	channel.deliver(requestFrame(5, "conn-1", "a9", { op: "observe", binding: bindingFixture() }));
	assert.equal(connection.state, "closed");
	assert.equal(connection.lastError?.code, "BAD_ENVELOPE");
});

test("client: observe without a grant is refused locally and sends no frame", async () => {
	const { channel, client } = clientHarness();
	assert.equal(codeOf(await rejection(client.observe())), "NOT_ATTACHED");
	assert.equal(requests(channel).length, 0);
	assert.equal(client.status().grant, null);
});

test("client: every operation needs its capability", async () => {
	const { channel, client } = clientHarness();
	channel.deliver(eventFrame(2, "conn-1", grantFixture(["focus"]), "attached"));
	assert.equal(codeOf(await rejection(client.observe())), "UNSUPPORTED_OP");
	const observing = clientHarness();
	observing.channel.deliver(eventFrame(2, "conn-1", grantFixture(["observe"]), "attached"));
	assert.equal(codeOf(await rejection(observing.client.focus("n1"))), "UNSUPPORTED_OP");
	assert.equal(codeOf(await rejection(observing.client.scroll({ deltaY: 200 }))), "UNSUPPORTED_OP");
	assert.equal(requests(channel).length, 0);
	assert.equal(requests(observing.channel).length, 0);
});

test("client: observe uses the binding from binding.changed and remembers the observation", async () => {
	const { channel, client } = clientHarness();
	const grant = grantFixture();
	channel.deliver(eventFrame(2, "conn-1", grant, "attached"));
	const pending = client.observe();
	const sent = requests(channel)[0];
	assert.deepStrictEqual(sent?.payload.op === "observe" ? sent.payload.binding : undefined, grant.binding);
	channel.deliver(resultFrame(3, "conn-1", "r1", { op: "observe", observation: observationFixture(grant.binding) }));
	assert.equal((await pending).observationId, "obs-1");
	assert.equal(client.status().observationId, "obs-1");
});

test("client: focus and scroll need a current observation", async () => {
	const { channel, client } = clientHarness();
	channel.deliver(eventFrame(2, "conn-1", grantFixture(), "attached"));
	assert.equal(codeOf(await rejection(client.focus("n1"))), "STALE_OBSERVATION");
	assert.equal(codeOf(await rejection(client.scroll({ deltaY: 100 }))), "STALE_OBSERVATION");
	assert.equal(requests(channel).length, 0);
});

test("client: a result captured under a replaced grant is refused, not stored", async () => {
	const { channel, client } = clientHarness();
	const first = grantFixture();
	channel.deliver(eventFrame(2, "conn-1", first, "attached"));
	const pending = rejection(client.observe());
	const second: Grant = {
		grantId: "g2",
		binding: { ...first.binding, grantId: "g2", pageEpoch: 2 },
		capabilities: ["observe"],
	};
	channel.deliver(eventFrame(3, "conn-1", second, "navigated"));
	// The peer answered the earlier observation before it processed the cancel.
	channel.deliver(resultFrame(4, "conn-1", "r1", { op: "observe", observation: observationFixture(first.binding) }));
	assert.equal(codeOf(await pending), "BINDING_STALE");
	assert.equal(client.status().observationId, undefined);
	assert.equal(client.status().grant?.grantId, "g2");
	const cancelSent = requests(channel)[1]?.payload;
	assert.equal(cancelSent?.op, "cancel");
	assert.equal(cancelSent?.op === "cancel" ? cancelSent.requestId : "", "r1");
});

test("client: a binding change while an effect is pending never reports success", async () => {
	const { channel, client } = clientHarness();
	const grant = grantFixture();
	channel.deliver(eventFrame(2, "conn-1", grant, "attached"));
	const observing = client.observe();
	channel.deliver(resultFrame(3, "conn-1", "r1", { op: "observe", observation: observationFixture(grant.binding) }));
	await observing;
	const pending = rejection(client.focus("n1"));
	assert.equal(requests(channel)[1]?.payload.op, "focus");
	channel.deliver(eventFrame(4, "conn-1", null, "detached"));
	const cancelFrame = requests(channel)[2];
	assert.equal(cancelFrame?.payload.op, "cancel");
	assert.equal(cancelFrame?.payload.op === "cancel" ? cancelFrame.payload.requestId : "", "r2");
	const cancelId = cancelFrame?.requestId ?? "";
	channel.deliver(resultFrame(5, "conn-1", cancelId, { op: "cancel", cancelled: true, alreadyTerminal: false }));
	assert.equal(codeOf(await pending), "OUTCOME_UNKNOWN");
	assert.equal(client.status().grant, null);
});

test("client: a null binding.changed revokes the grant and the observation", async () => {
	const { channel, client } = clientHarness();
	const grant = grantFixture();
	channel.deliver(eventFrame(2, "conn-1", grant, "attached"));
	const pending = client.observe();
	channel.deliver(resultFrame(3, "conn-1", "r1", { op: "observe", observation: observationFixture(grant.binding) }));
	await pending;
	channel.deliver(eventFrame(4, "conn-1", null, "navigated"));
	assert.equal(client.status().grant, null);
	assert.equal(client.status().observationId, undefined);
	assert.equal(codeOf(await rejection(client.focus("n1"))), "NOT_ATTACHED");
});

test("client: an observation for another binding is rejected", async () => {
	const { channel, client } = clientHarness();
	const grant = grantFixture();
	channel.deliver(eventFrame(2, "conn-1", grant, "attached"));
	const pending = rejection(client.observe());
	const other: Binding = { ...grant.binding, documentId: "doc-2" };
	channel.deliver(resultFrame(3, "conn-1", "r1", { op: "observe", observation: observationFixture(other) }));
	assert.equal(codeOf(await pending), "BINDING_STALE");
	assert.equal(client.status().observationId, undefined);
});

test("client: scroll forwards ref, alignment and deltas exactly", async () => {
	const { channel, client } = clientHarness();
	const grant = grantFixture();
	channel.deliver(eventFrame(2, "conn-1", grant, "attached"));
	const observing = client.observe();
	channel.deliver(resultFrame(3, "conn-1", "r1", { op: "observe", observation: observationFixture(grant.binding) }));
	await observing;
	const scrolling = client.scroll({ ref: "n1", align: "center", deltaY: 120 });
	assert.deepStrictEqual(requests(channel)[1]?.payload, {
		op: "scroll",
		binding: grant.binding,
		observationId: "obs-1",
		ref: "n1",
		align: "center",
		deltaY: 120,
	});
	channel.deliver(resultFrame(4, "conn-1", "r2", { op: "scroll", outcome: "applied", scrollY: 900 }));
	assert.deepStrictEqual(await scrolling, { outcome: "applied", scrollY: 900 });
});

test("client: disconnect drops the grant projection and later work is refused", async () => {
	const { channel, client } = clientHarness();
	channel.deliver(eventFrame(2, "conn-1", grantFixture(), "attached"));
	assert.notEqual(client.status().grant, null);
	await client.disconnect("detached");
	assert.equal(byes(channel).length, 1);
	assert.equal(channel.closed, true);
	assert.equal(client.status().grant, null);
	assert.equal(client.status().observationId, undefined);
	assert.equal(codeOf(await rejection(client.observe())), "NOT_ATTACHED");
});

test("client: status listeners see ready, the grant change and the close", () => {
	const { channel, client } = clientHarness();
	const seen: string[] = [];
	client.onStatus((status) => seen.push(`${status.link}:${status.grant === null ? "none" : status.grant.grantId}`));
	channel.deliver(eventFrame(2, "conn-1", grantFixture(), "attached"));
	client.disconnect("closed");
	assert.deepStrictEqual(seen, ["ready:g1", "closed:none"]);
});

test("grants: attach replaces the binding, bumps the generation and clears the observation", () => {
	const clock = new FakeClock();
	const manager = new GrantManager({ clock });
	const grant = grantFixture();
	manager.attach(grant);
	assert.equal(manager.generation, 1);
	assert.equal(manager.grant?.binding.documentId, "doc-1");
	assert.equal(manager.noteObservation(observationFixture(grant.binding)), null);
	assert.equal(manager.observationId, "obs-1");
	const next: Grant = { ...grant, grantId: "g2", binding: { ...grant.binding, grantId: "g2", pageEpoch: 2 } };
	manager.attach(next);
	assert.equal(manager.generation, 2);
	assert.equal(manager.observationId, undefined);
	assert.equal(manager.grant?.grantId, "g2");
});

test("grants: stale binding and stale observation are named exactly", () => {
	const clock = new FakeClock();
	const manager = new GrantManager({ clock });
	assert.equal(manager.checkBinding(bindingFixture())?.code, "NOT_ATTACHED");
	const grant = grantFixture();
	manager.attach(grant);
	assert.equal(manager.checkBinding(grant.binding), null);
	for (const key of ["grantId", "tabId", "documentId", "pageEpoch"] as const) {
		const changed: Binding = { ...grant.binding, [key]: key === "tabId" || key === "pageEpoch" ? 99 : "changed" };
		assert.equal(manager.checkBinding(changed)?.code, "BINDING_STALE");
	}
	assert.equal(manager.checkObservation(grant.binding, "obs-other")?.code, "STALE_OBSERVATION");
	manager.noteObservation(observationFixture(grant.binding));
	assert.equal(manager.checkObservation(grant.binding, "obs-1"), null);
	manager.invalidate();
	assert.equal(manager.checkObservation(grant.binding, "obs-1")?.code, "NOT_ATTACHED");
	assert.equal(manager.generation, 2);
});

test("grants: capability checks and expiry use the injected clock", () => {
	const clock = new FakeClock();
	const manager = new GrantManager({ clock, expiryMs: 5_000 });
	manager.attach(grantFixture(["observe"]));
	assert.throws(() => manager.requireCapability("focus"), /capability not granted/);
	assert.doesNotThrow(() => manager.requireCapability("observe"));
	assert.equal(capabilityGranted(grantFixture(["observe"]), "scroll"), false);
	clock.advance(4_999);
	assert.equal(manager.expireIfDue(), null);
	clock.advance(1);
	assert.equal(manager.expireIfDue(), "expired");
	assert.equal(manager.grant, null);
	assert.equal(manager.generation, 2);
});

test("worker: an initiator connection plus GrantManager serves observe and refuses a stale binding", async () => {
	const clock = new FakeClock();
	const channel = new FakeChannel();
	const grants = new GrantManager({ clock });
	const grant = grantFixture();
	grants.attach(grant);
	const connection = new Connection({
		role: "initiator",
		channel,
		clock,
		peer: { kind: "extension", version: "0.0.1" },
		capabilities: ["observe"],
		handleRequest: async (request) => {
			if (request.payload.op === "cancel") return { op: "cancel", cancelled: false, alreadyTerminal: true };
			const stale = grants.checkBinding(request.payload.binding);
			if (stale !== null) return stale;
			if (request.payload.op === "observe") {
				return { op: "observe", observation: observationFixture(grant.binding) };
			}
			return { op: "focus", outcome: "unknown" };
		},
	});
	connection.open();
	channel.deliver(ackFrame(1, "conn-cli"));
	assert.equal(connection.state, "ready");
	channel.deliver(requestFrame(2, "conn-cli", "req-1", { op: "observe", binding: grant.binding }));
	await Promise.resolve();
	await Promise.resolve();
	assert.equal(payloadOf(resultFor(channel, "req-1"))?.op, "observe");

	const staleBinding: Binding = { ...grant.binding, pageEpoch: 9 };
	channel.deliver(requestFrame(3, "conn-cli", "req-2", { op: "observe", binding: staleBinding }));
	await Promise.resolve();
	await Promise.resolve();
	assert.equal(errorCodeOf(resultFor(channel, "req-2")), "BINDING_STALE");
});
