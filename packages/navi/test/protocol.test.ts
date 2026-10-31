import assert from "node:assert/strict";
import test from "node:test";
import { decodeFrame, encodeFrame, frameByteLength } from "../src/protocol/codec.ts";
import {
	CAPABILITIES,
	MAX_ENVELOPE_BYTES,
	MAX_INFLIGHT_BROWSER_OPS,
	MAX_NODES,
	MAX_OBSERVATION_BYTES,
	MAX_SCROLL_DELTA,
	MAX_TEXT_CHARS,
	MAX_VISITED_ELEMENTS,
	NAVI_PROTOCOL_VERSION,
} from "../src/protocol/limits.ts";
import type {
	Binding,
	DomNode,
	EventFrame,
	Frame,
	HelloAckFrame,
	HelloFrame,
	Observation,
	RequestFrame,
	ResultFrame,
} from "../src/protocol/types.ts";
import {
	isCapability,
	NAVI_ERROR_CODES_ARE_EXHAUSTIVE,
	NAVI_WIRE_ERROR_CODES_ARE_EXHAUSTIVE,
	ProtocolError,
	parseFrame,
} from "../src/protocol/validate.ts";

const binding: Binding = { grantId: "grant-1", tabId: 7, documentId: "doc-1", pageEpoch: 3 };

const node: DomNode = {
	ref: "n1",
	depth: 0,
	tag: "p",
	interactive: false,
	editable: false,
	offscreen: false,
};

const observation: Observation = {
	observationId: "obs-1",
	binding,
	capturedAt: 1_700_000_000_000,
	title: "Example",
	url: "https://example.com/a",
	nodes: [node],
	visited: 1,
	truncated: false,
	omitted: [],
};

const hello: HelloFrame = {
	v: NAVI_PROTOCOL_VERSION,
	seq: 1,
	type: "hello",
	peer: { kind: "cli", version: "0.0.1" },
	capabilities: ["observe", "focus"],
};
const helloAck: HelloAckFrame = {
	v: NAVI_PROTOCOL_VERSION,
	seq: 1,
	type: "hello.ack",
	connectionId: "connection-1",
	peer: { kind: "extension", version: "0.0.1" },
	capabilities: ["observe"],
};
const observeRequest: RequestFrame = {
	v: NAVI_PROTOCOL_VERSION,
	seq: 2,
	type: "request",
	connectionId: "connection-1",
	requestId: "req-1",
	payload: { op: "observe", binding },
};
const observeResult: ResultFrame = {
	v: NAVI_PROTOCOL_VERSION,
	seq: 2,
	type: "result",
	connectionId: "connection-1",
	requestId: "req-1",
	ok: true,
	payload: { op: "observe", observation },
};
const failedResult: ResultFrame = {
	v: NAVI_PROTOCOL_VERSION,
	seq: 2,
	type: "result",
	connectionId: "connection-1",
	requestId: "req-8",
	ok: false,
	error: { code: "NOT_ATTACHED", message: "No grant", retryable: false },
};
const bindingChanged: EventFrame = {
	v: NAVI_PROTOCOL_VERSION,
	seq: 3,
	type: "event",
	connectionId: "connection-1",
	kind: "binding.changed",
	payload: { grant: { grantId: "grant-1", binding, capabilities: ["observe", "scroll"] }, reason: "attached" },
};

function rejects(code: string, value: unknown, path?: string): ProtocolError {
	let thrown: unknown;
	try {
		parseFrame(value);
	} catch (error) {
		thrown = error;
	}
	assert.ok(thrown instanceof ProtocolError, `expected ProtocolError for ${code}`);
	assert.equal(thrown.code, code);
	if (path !== undefined) assert.equal(thrown.path, path);
	return thrown;
}

function frame(overrides: Record<string, unknown>): unknown {
	return { ...observeRequest, ...overrides };
}

test("round-trips every frame type", () => {
	const frames: Frame[] = [
		hello,
		helloAck,
		{
			v: NAVI_PROTOCOL_VERSION,
			seq: 2,
			type: "hello.error",
			error: { code: "VERSION_MISMATCH", message: "no", retryable: false },
		},
		observeRequest,
		{ ...observeRequest, requestId: "req-2", payload: { op: "focus", binding, observationId: "obs-1", ref: "n1" } },
		{
			...observeRequest,
			requestId: "req-3",
			payload: { op: "scroll", binding, observationId: "obs-1", ref: "n1", align: "center" },
		},
		{
			...observeRequest,
			requestId: "req-3b",
			payload: { op: "scroll", binding, observationId: "obs-1", deltaY: 200 },
		},
		{ ...observeRequest, requestId: "req-4", payload: { op: "cancel", requestId: "req-2" } },
		observeResult,
		{ ...observeResult, requestId: "req-5", payload: { op: "focus", outcome: "unknown" } },
		{ ...observeResult, requestId: "req-6", payload: { op: "scroll", outcome: "applied", scrollY: 120 } },
		{ ...observeResult, requestId: "req-7", payload: { op: "cancel", cancelled: true, alreadyTerminal: false } },
		failedResult,
		bindingChanged,
		{ ...bindingChanged, seq: 4, payload: { grant: null, reason: "navigated" } },
		{ v: NAVI_PROTOCOL_VERSION, seq: 5, type: "bye", connectionId: "connection-1", reason: "detached" },
	];
	for (const candidate of frames) {
		const text = encodeFrame(candidate);
		assert.deepEqual(decodeFrame(text), candidate);
		assert.equal(frameByteLength(candidate), Buffer.byteLength(text, "utf8"));
		assert.equal(parseFrame(candidate), candidate);
	}
});

test("frozen limits match the agreed contract", () => {
	assert.equal(NAVI_PROTOCOL_VERSION, 1);
	assert.deepEqual([...CAPABILITIES], ["observe", "focus", "scroll"]);
	assert.equal(MAX_ENVELOPE_BYTES, 64 * 1024);
	assert.equal(MAX_OBSERVATION_BYTES, 16 * 1024);
	assert.equal(MAX_NODES, 100);
	assert.equal(MAX_TEXT_CHARS, 200);
	assert.equal(MAX_VISITED_ELEMENTS, 1500);
	assert.equal(MAX_INFLIGHT_BROWSER_OPS, 1);
	assert.equal(MAX_SCROLL_DELTA, 1000);
	assert.equal(NAVI_ERROR_CODES_ARE_EXHAUSTIVE, true);
	assert.equal(NAVI_WIRE_ERROR_CODES_ARE_EXHAUSTIVE, true);
	assert.ok(isCapability("scroll"));
	assert.ok(!isCapability("click"));
});

test("rejects values that are not JSON objects", () => {
	for (const value of [null, 5, "hello", true, [], undefined]) rejects("NOT_JSON_OBJECT", value);
});

test("rejects a foreign protocol version and bad sequence numbers", () => {
	rejects("VERSION_MISMATCH", { ...hello, v: 2 }, "v");
	rejects("VERSION_MISMATCH", { ...hello, v: "1" }, "v");
	for (const seq of [0, -1, 1.5, "1", Number.MAX_SAFE_INTEGER + 1]) rejects("BAD_ENVELOPE", { ...hello, seq }, "seq");
	rejects("BAD_ENVELOPE", { ...hello, type: "hello_ack" }, "type");
});

test("rejects unknown fields, missing fields and undefined values", () => {
	rejects("BAD_ENVELOPE", { ...hello, extra: 1 }, "extra");
	rejects("NOT_JSON_OBJECT", { ...hello, capabilities: undefined }, "capabilities");
	rejects("NOT_JSON_OBJECT", { ...hello, peer: undefined }, "peer");
	const { capabilities: _dropped, ...withoutCapabilities } = hello;
	void _dropped;
	rejects("BAD_ENVELOPE", withoutCapabilities, "capabilities");
	rejects("BAD_ENVELOPE", { ...observeRequest, connectionId: "" }, "connectionId");
	rejects("TOO_LARGE", { ...helloAck, connectionId: "x".repeat(65) }, "connectionId");
});

test("rejects an oversized envelope before inspecting its schema", () => {
	rejects("ENVELOPE_TOO_LARGE", { ...hello, blob: "x".repeat(70_000) });
	rejects("ENVELOPE_TOO_LARGE", {
		...observeResult,
		payload: { op: "observe", observation: { ...observation, url: "y".repeat(70_000) } },
	});
});

test("requires a non-empty, duplicate-free, known capability list", () => {
	rejects("BAD_ENVELOPE", { ...hello, capabilities: [] }, "capabilities");
	rejects("BAD_ENVELOPE", { ...hello, capabilities: ["observe", "observe"] }, "capabilities[1]");
	rejects("BAD_ENVELOPE", { ...hello, capabilities: ["click"] }, "capabilities[0]");
	rejects("BAD_ENVELOPE", { ...helloAck, capabilities: ["fill"] }, "capabilities[0]");
});

test("validates the request payload union", () => {
	parseFrame({ ...observeRequest, payload: { op: "observe", binding } });
	parseFrame({ ...observeRequest, payload: { op: "focus", binding, observationId: "obs-1", ref: "n1" } });
	parseFrame({ ...observeRequest, payload: { op: "scroll", binding, observationId: "obs-1", deltaX: -10 } });
	parseFrame({ ...observeRequest, payload: { op: "cancel", requestId: "req-9" } });
	rejects("UNSUPPORTED_OP", frame({ payload: { op: "click", binding } }), "payload.op");
	rejects("BAD_ENVELOPE", frame({ payload: { op: "observe" } }), "payload.binding");
	rejects("BAD_ARGUMENT", frame({ payload: { op: "scroll", binding, observationId: "obs-1" } }), "payload");
	rejects("BAD_ENVELOPE", frame({ payload: { op: "focus", binding, observationId: "obs-1" } }), "payload.ref");
	rejects("BAD_ENVELOPE", frame({ payload: { op: "cancel", requestId: "req-2", extra: 1 } }), "payload.extra");
	rejects(
		"BAD_ENVELOPE",
		frame({ payload: { op: "scroll", binding, observationId: "obs-1", align: "middle" } }),
		"payload.align",
	);
	rejects("BAD_ENVELOPE", frame({ payload: { op: "observe", binding, requestId: "req-9" } }), "payload.requestId");
});

test("result frames accept exactly one of payload or error", () => {
	rejects("NOT_JSON_OBJECT", { ...observeResult, payload: undefined }, "payload");
	rejects("BAD_ENVELOPE", { ...observeResult, error: { code: "INTERNAL", message: "no", retryable: false } }, "error");
	rejects("BAD_ENVELOPE", { ...failedResult, payload: { op: "focus", outcome: "applied" } }, "payload");
	rejects("NOT_JSON_OBJECT", { ...observeResult, ok: undefined }, "ok");
	rejects("BAD_ENVELOPE", { ...observeResult, requestId: "" }, "requestId");
	rejects("UNSUPPORTED_OP", { ...observeResult, payload: { op: "activate" } }, "payload.op");
});

test("validates bindings and grant linkage", () => {
	const { pageEpoch: _dropped, ...bindingWithoutEpoch } = binding;
	void _dropped;
	rejects(
		"BAD_ENVELOPE",
		frame({ payload: { op: "observe", binding: bindingWithoutEpoch } }),
		"payload.binding.pageEpoch",
	);
	rejects(
		"BAD_ENVELOPE",
		frame({ payload: { op: "observe", binding: { ...binding, tabId: -1 } } }),
		"payload.binding.tabId",
	);
	rejects(
		"BAD_ENVELOPE",
		frame({ payload: { op: "observe", binding: { ...binding, pageEpoch: 1.5 } } }),
		"payload.binding.pageEpoch",
	);
	rejects(
		"BAD_ENVELOPE",
		frame({ payload: { op: "observe", binding: { ...binding, grantId: "" } } }),
		"payload.binding.grantId",
	);
	parseFrame(bindingChanged);
	rejects(
		"BAD_ARGUMENT",
		{
			...bindingChanged,
			payload: { grant: { grantId: "other", binding, capabilities: ["observe"] }, reason: "attached" },
		},
		"payload.grant.grantId",
	);
	rejects("BAD_ENVELOPE", { ...bindingChanged, payload: { grant: null, reason: "navigated-away" } }, "payload.reason");
	rejects("BAD_ENVELOPE", { ...bindingChanged, kind: "node.added" }, "kind");
});

test("bounds observations and nodes", () => {
	const manyNodes = Array.from({ length: MAX_NODES + 1 }, (_value, index) => ({ ...node, ref: `n${index}` }));
	rejects(
		"TOO_LARGE",
		{ ...observeResult, payload: { op: "observe", observation: { ...observation, nodes: manyNodes } } },
		"payload.observation.nodes",
	);
	rejects(
		"BAD_ENVELOPE",
		{
			...observeResult,
			payload: { op: "observe", observation: { ...observation, nodes: [{ ...node, depth: MAX_NODES + 1 }] } },
		},
		"payload.observation.nodes[0].depth",
	);
	rejects(
		"TOO_LARGE",
		{
			...observeResult,
			payload: {
				op: "observe",
				observation: { ...observation, nodes: [{ ...node, text: "t".repeat(MAX_TEXT_CHARS + 1) }] },
			},
		},
		"payload.observation.nodes[0].text",
	);
	rejects(
		"BAD_ENVELOPE",
		{
			...observeResult,
			payload: {
				op: "observe",
				observation: { ...observation, nodes: [{ ...node, editable: true, text: "value" }] },
			},
		},
		"payload.observation.nodes[0].text",
	);
	rejects(
		"BAD_ENVELOPE",
		{
			...observeResult,
			payload: {
				op: "observe",
				observation: { ...observation, nodes: [{ ...node, rect: { x: 0, y: 0, w: "10", h: 4 } }] },
			},
		},
		"payload.observation.nodes[0].rect.w",
	);
	rejects(
		"BAD_ENVELOPE",
		{
			...observeResult,
			payload: { op: "observe", observation: { ...observation, visited: MAX_VISITED_ELEMENTS + 1 } },
		},
		"payload.observation.visited",
	);
	rejects(
		"BAD_ENVELOPE",
		{ ...observeResult, payload: { op: "observe", observation: { ...observation, omitted: ["iframes"] } } },
		"payload.observation.omitted[0]",
	);
	rejects(
		"BAD_ENVELOPE",
		{ ...observeResult, payload: { op: "observe", observation: { ...observation, truncated: "no" } } },
		"payload.observation.truncated",
	);
	rejects(
		"NOT_JSON_OBJECT",
		{
			...observeResult,
			payload: { op: "observe", observation: { ...observation, nodes: [{ ...node, role: undefined }] } },
		},
		"payload.observation.nodes[0].role",
	);
});

test("bounds the serialized observation independently of the envelope budget", () => {
	const fat = Array.from({ length: MAX_NODES }, (_value, index) => ({
		...node,
		ref: `n${index}`,
		name: "n".repeat(MAX_TEXT_CHARS),
		text: "t".repeat(MAX_TEXT_CHARS),
	}));
	const value = {
		...observeResult,
		payload: { op: "observe", observation: { ...observation, nodes: fat, visited: MAX_NODES } },
	};
	assert.ok(
		JSON.stringify((value.payload as { observation: Observation }).observation).length > MAX_OBSERVATION_BYTES,
	);
	rejects("TOO_LARGE", value, "payload.observation");
});

test("validates error values", () => {
	rejects("BAD_ENVELOPE", { ...failedResult, error: { code: "OOPS", message: "x", retryable: false } }, "error.code");
	rejects(
		"BAD_ENVELOPE",
		{ ...failedResult, error: { code: "INTERNAL", message: "", retryable: false } },
		"error.message",
	);
	rejects(
		"TOO_LARGE",
		{ ...failedResult, error: { code: "INTERNAL", message: "x".repeat(201), retryable: false } },
		"error.message",
	);
	rejects(
		"BAD_ENVELOPE",
		{ ...failedResult, error: { code: "INTERNAL", message: "x", retryable: "no" } },
		"error.retryable",
	);
	parseFrame(failedResult);
});

test("decodeFrame reports malformed text and non-object JSON", () => {
	let thrown: unknown;
	try {
		decodeFrame("{");
	} catch (error) {
		thrown = error;
	}
	assert.ok(thrown instanceof ProtocolError);
	assert.equal(thrown.code, "BAD_JSON");
	assert.equal(decodeFrame(encodeFrame(hello)).type, "hello");
	for (const text of ["[1]", "null", "5", '"hello"']) rejects("NOT_JSON_OBJECT", JSON.parse(text));
});

test("encodeFrame validates before sending and reports the offending path", () => {
	let thrown: unknown;
	try {
		encodeFrame({ ...observeRequest, payload: { op: "observe", binding: { ...binding, pageEpoch: -1 } } } as Frame);
	} catch (error) {
		thrown = error;
	}
	assert.ok(thrown instanceof ProtocolError);
	assert.equal(thrown.code, "BAD_ENVELOPE");
	assert.equal(thrown.path, "payload.binding.pageEpoch");
	assert.match(thrown.message, /payload\.binding\.pageEpoch/);
});

test("rejects transport-local error codes on the wire", () => {
	const transportCodes = [
		"BAD_FRAME_LENGTH",
		"FRAME_TOO_LARGE",
		"TRUNCATED_FRAME",
		"BAD_UTF8",
		"BAD_JSON",
		"NOT_JSON_OBJECT",
		"ENVELOPE_TOO_LARGE",
	] as const;
	for (const code of transportCodes) {
		rejects(
			"BAD_ENVELOPE",
			{ ...failedResult, error: { code, message: "local only", retryable: false } },
			"error.code",
		);
	}
	for (const code of ["BAD_ENVELOPE", "BINDING_STALE", "CANCELLED", "OUTCOME_UNKNOWN", "INTERNAL"] as const) {
		parseFrame({ ...failedResult, error: { code, message: "wire", retryable: false } });
	}
});

test("rejects control and bidi characters in ids, titles, messages and urls", () => {
	rejects("BAD_ENVELOPE", frame({ requestId: "a\n\u001b[31mFORGED" }), "requestId");
	rejects("BAD_ENVELOPE", frame({ connectionId: "c\u001b]0;pwned\u0007" }), "connectionId");
	rejects(
		"BAD_ENVELOPE",
		{ ...failedResult, error: { code: "INTERNAL", message: "x\n\u001b[2J\u001b[H", retryable: false } },
		"error.message",
	);
	rejects(
		"BAD_ENVELOPE",
		frame({ payload: { op: "observe", binding: { ...binding, grantId: "g\u202e" } } }),
		"payload.binding.grantId",
	);
	rejects(
		"BAD_ENVELOPE",
		frame({ payload: { op: "focus", binding, observationId: "obs-1", ref: "n1\u001b[2J" } }),
		"payload.ref",
	);
	rejects(
		"BAD_ENVELOPE",
		{ ...observeResult, payload: { op: "observe", observation: { ...observation, title: "ok\n\u001b[2Jpwned" } } },
		"payload.observation.title",
	);
	rejects(
		"BAD_ENVELOPE",
		{ ...observeResult, payload: { op: "observe", observation: { ...observation, title: "reordered\u202e" } } },
		"payload.observation.title",
	);
	rejects(
		"BAD_ENVELOPE",
		{ ...observeResult, payload: { op: "observe", observation: { ...observation, url: "https://h/p\u001b[31m" } } },
		"payload.observation.url",
	);
	// Page text is data, not a control string: it may carry any Unicode, including bidi marks.
	parseFrame({
		...observeResult,
		payload: {
			op: "observe",
			observation: {
				...observation,
				nodes: [{ ...node, name: "\u200fRTL label", text: "line\u2028break\u001b[0m" }],
			},
		},
	});
});

test("requires a canonical, credential-free http(s) display URL", () => {
	const withUrl = (url: string): unknown => ({
		...observeResult,
		payload: { op: "observe", observation: { ...observation, url } },
	});
	for (const url of [
		"",
		"https://user:pw@host.test/p",
		"https://host.test/p?token=SECRET",
		"https://host.test/p#frag",
		"javascript:alert(1)",
		"file:///etc/passwd",
		"data:text/html,hi",
		"chrome://settings/",
		"https://host.test",
		"HTTPS://HOST.TEST/p",
	]) {
		rejects("BAD_ENVELOPE", withUrl(url), "payload.observation.url");
	}
	for (const url of [
		"https://host.test/",
		"https://host.test/a/b",
		"http://[::1]/",
		"https://xn--bcher-kva.example/%E2%82%AC",
	]) {
		parseFrame(withUrl(url));
	}
});

test("accepts only unambiguous scroll shapes", () => {
	const scroll = (payload: Record<string, unknown>): unknown =>
		frame({ payload: { op: "scroll", binding, observationId: "obs-1", ...payload } });
	parseFrame(scroll({ ref: "n1" }));
	parseFrame(scroll({ ref: "n1", align: "end" }));
	parseFrame(scroll({ deltaY: MAX_SCROLL_DELTA }));
	parseFrame(scroll({ deltaX: -MAX_SCROLL_DELTA, deltaY: 10 }));
	rejects("BAD_ARGUMENT", scroll({ align: "end" }), "payload.ref");
	rejects("BAD_ARGUMENT", scroll({ ref: "n1", deltaY: 500 }), "payload");
	rejects("BAD_ARGUMENT", scroll({ ref: "n1", align: "center", deltaX: 1 }), "payload");
	rejects("BAD_ARGUMENT", scroll({}), "payload");
	rejects("BAD_ENVELOPE", scroll({ deltaY: MAX_SCROLL_DELTA + 1 }), "payload.deltaY");
});

test("keeps node tags and roles to lowercase tokens", () => {
	const withNode = (patch: Record<string, unknown>): unknown => ({
		...observeResult,
		payload: { op: "observe", observation: { ...observation, nodes: [{ ...node, ...patch }] } },
	});
	parseFrame(withNode({ tag: "custom-element", role: "listitem" }));
	rejects("BAD_ENVELOPE", withNode({ tag: "DIV" }), "payload.observation.nodes[0].tag");
	rejects("BAD_ENVELOPE", withNode({ tag: "div\u001b" }), "payload.observation.nodes[0].tag");
	rejects("BAD_ENVELOPE", withNode({ role: "list item" }), "payload.observation.nodes[0].role");
});

test("visited covers the returned nodes and truncation may name no region", () => {
	const withObservation = (patch: Record<string, unknown>): unknown => ({
		...observeResult,
		payload: { op: "observe", observation: { ...observation, ...patch } },
	});
	rejects(
		"BAD_ENVELOPE",
		withObservation({ nodes: [node, { ...node, ref: "n2" }], visited: 1 }),
		"payload.observation.visited",
	);
	rejects("BAD_ENVELOPE", withObservation({ visited: 0 }), "payload.observation.visited");
	parseFrame(withObservation({ visited: 5 }));
	// A time budget can stop the walk before any named region is skipped, so omitted stays empty.
	parseFrame(withObservation({ truncated: true, omitted: [] }));
	parseFrame(withObservation({ truncated: true, omitted: ["offscreen"] }));
});
