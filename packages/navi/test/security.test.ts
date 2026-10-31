/**
 * Security boundary tests (Node, protocol level).
 *
 * Owned by: foundation-security. Evidence type: API-level mock/contract tests.
 * Live behavior (native registration, real popup sender identity, BFCache/discard/replacement,
 * worker restart, real site focus/scroll side effects) stays a manual browser gate - see
 * docs/navi-foundation/architecture.md "Required verification".
 *
 * Tests marked `todo` are confirmed defect repros reported to the parent; they are not red so the
 * suite stays usable, but they must go green once the owning layer is fixed.
 */
import assert from "node:assert/strict";
import test from "node:test";
import { decodeFrame, encodeFrame, frameByteLength } from "../src/protocol/codec.ts";
import { MAX_ENVELOPE_BYTES, MAX_OBSERVATION_BYTES, MAX_SCROLL_DELTA } from "../src/protocol/limits.ts";
import { ProtocolError, parseFrame, utf8TextByteLength } from "../src/protocol/validate.ts";

type FrameValue = Record<string, unknown>;

const BINDING = { grantId: "g1", tabId: 7, documentId: "d1", pageEpoch: 2 };

function binding(over: Partial<typeof BINDING> = {}) {
	return { ...BINDING, ...over };
}

function node(over: Record<string, unknown> = {}) {
	return { ref: "n1", depth: 0, tag: "p", interactive: false, editable: false, offscreen: false, ...over };
}

function observation(over: Record<string, unknown> = {}) {
	return {
		observationId: "o1",
		binding: binding(),
		capturedAt: 1,
		url: "https://example.test/page",
		nodes: [],
		visited: 0,
		truncated: false,
		omitted: [],
		...over,
	};
}

function observeResult(observationValue: unknown): FrameValue {
	return {
		v: 1,
		seq: 1,
		type: "result",
		connectionId: "c1",
		requestId: "r1",
		ok: true,
		payload: { op: "observe", observation: observationValue },
	};
}

function request(payload: Record<string, unknown>): FrameValue {
	return { v: 1, seq: 1, type: "request", connectionId: "c1", requestId: "r1", payload };
}

/** Parses and returns the thrown ProtocolError, failing the test when the value was accepted. */
function protocolError(value: unknown): ProtocolError {
	try {
		parseFrame(value);
	} catch (error) {
		assert.ok(error instanceof ProtocolError, `expected ProtocolError, got ${String(error)}`);
		return error;
	}
	assert.fail("expected parseFrame to reject the value");
}

test("rejects unknown frame types instead of defaulting to permissive handling", () => {
	const error = protocolError({ v: 1, seq: 1, type: "exec", connectionId: "c1", payload: {} });
	assert.equal(error.code, "BAD_ENVELOPE");
});

test("rejects unknown fields, including model-shaped authority claims such as approved", () => {
	// A field named `approved` in untrusted input is never authorization: it must not even parse.
	const withApproved = protocolError(request({ op: "observe", binding: binding(), approved: true }));
	assert.equal(withApproved.code, "BAD_ENVELOPE");
	assert.match(withApproved.message, /approved/);

	const withCapabilities = protocolError(
		request({ op: "focus", binding: binding(), observationId: "o1", ref: "n1", capabilities: ["scroll"] }),
	);
	assert.equal(withCapabilities.code, "BAD_ENVELOPE");

	const withGrant = protocolError(request({ op: "observe", binding: binding(), grant: { grantId: "g1" } }));
	assert.equal(withGrant.code, "BAD_ENVELOPE");
});

test("rejects prototype-shaped keys without polluting Object.prototype", () => {
	const keys = ["__proto__", "constructor", "prototype"];
	for (const key of keys) {
		const raw = JSON.parse(`{"v":1,"seq":1,"type":"bye","connectionId":"c1","reason":"closed","${key}":{"x":1}}`);
		const error = protocolError(raw);
		assert.equal(error.code, "BAD_ENVELOPE", `${key} must be rejected`);
	}
	assert.equal(({} as Record<string, unknown>).x, undefined);
	assert.equal(Object.hasOwn(Object.prototype, "x"), false);
});

test("capability list is an allowlist, unique, and non-empty", () => {
	assert.equal(
		protocolError({
			v: 1,
			seq: 1,
			type: "hello",
			peer: { kind: "extension", version: "0.1" },
			capabilities: ["click"],
		}).code,
		"BAD_ENVELOPE",
	);
	assert.equal(
		protocolError({
			v: 1,
			seq: 1,
			type: "hello",
			peer: { kind: "extension", version: "0.1" },
			capabilities: ["observe", "observe"],
		}).code,
		"BAD_ENVELOPE",
	);
	// Empty list is unrepresentable, so a handshake always claims at least one capability. The
	// extension must therefore treat the acknowledged set as a ceiling, never as authority.
	const empty = protocolError({
		v: 1,
		seq: 1,
		type: "hello",
		peer: { kind: "extension", version: "0.1" },
		capabilities: [],
	});
	assert.equal(empty.code, "BAD_ENVELOPE");
});

test("rejects version and sequence deviations", () => {
	for (const v of [2, "1", 0, null, undefined]) {
		const error = protocolError({ v, seq: 1, type: "bye", connectionId: "c1", reason: "closed" });
		assert.ok(error.code === "VERSION_MISMATCH" || error.code === "BAD_ENVELOPE", `v=${String(v)} -> ${error.code}`);
	}
	for (const seq of [0, -1, 1.5, Number.MAX_SAFE_INTEGER + 1, "1", null]) {
		assert.equal(
			protocolError({ v: 1, seq, type: "bye", connectionId: "c1", reason: "closed" }).code,
			"BAD_ENVELOPE",
			`seq=${String(seq)} must be rejected`,
		);
	}
});

test("handshake frames cannot carry a connection id before one exists", () => {
	const error = protocolError({
		v: 1,
		seq: 1,
		type: "hello",
		connectionId: "c1",
		peer: { kind: "extension", version: "0.1" },
		capabilities: ["observe"],
	});
	assert.equal(error.code, "BAD_ENVELOPE");
	assert.equal(
		protocolError({ v: 1, seq: 1, type: "hello", peer: { kind: "page", version: "0.1" }, capabilities: ["observe"] })
			.code,
		"BAD_ENVELOPE",
	);
});

test("binding fields are strictly shaped", () => {
	const cases: [string, unknown][] = [
		["negative tabId", binding({ tabId: -1 })],
		["fractional pageEpoch", binding({ pageEpoch: 1.5 })],
		["empty documentId", binding({ documentId: "" })],
		["oversized grantId", binding({ grantId: "g".repeat(65) })],
	];
	for (const [label, value] of cases) {
		const code = protocolError(request({ op: "observe", binding: value })).code;
		assert.ok(code === "BAD_ENVELOPE" || code === "TOO_LARGE", `${label} -> ${code}`);
	}
	assert.equal(protocolError(request({ op: "observe", binding: { ...BINDING, extra: 1 } })).code, "BAD_ENVELOPE");
	assert.equal(
		protocolError(request({ op: "observe", binding: { grantId: "g1", tabId: 7, documentId: "d1" } })).code,
		"BAD_ENVELOPE",
	);
});

test("grant events must not let the grant id and its binding disagree", () => {
	const frame = {
		v: 1,
		seq: 1,
		type: "event",
		connectionId: "c1",
		kind: "binding.changed",
		payload: {
			grant: { grantId: "g1", binding: binding({ grantId: "g2" }), capabilities: ["observe"] },
			reason: "attached",
		},
	};
	assert.equal(protocolError(frame).code, "BAD_ARGUMENT");
});

test("a nullable grant is how authority is withdrawn", () => {
	const frame = {
		v: 1,
		seq: 1,
		type: "event",
		connectionId: "c1",
		kind: "binding.changed",
		payload: { grant: null, reason: "navigated" },
	};
	const parsed = parseFrame(frame);
	assert.equal(parsed.type, "event");
});

test("editable and secret elements must not carry text through the protocol", () => {
	const leaked = protocolError(
		observeResult(
			observation({ nodes: [node({ tag: "input", editable: true, interactive: true, text: "typed-secret" })] }),
		),
	);
	assert.equal(leaked.code, "BAD_ENVELOPE");
	assert.match(leaked.message, /Editable/);

	// Non-editable prose is allowed but stays untrusted page data.
	const prose = parseFrame(observeResult(observation({ nodes: [node({ text: "page text" })], visited: 1 })));
	assert.equal(prose.type, "result");
});

test("payload budgets are enforced independently of the transport and before deep field checks", () => {
	// The envelope budget must be checked before per-field validation, so an oversized frame cannot
	// force full deep validation of attacker-controlled structure.
	const oversized = protocolError({
		v: 1,
		seq: 1,
		type: "bye",
		connectionId: "c1",
		reason: "closed",
		junk: "x".repeat(MAX_ENVELOPE_BYTES + 4096),
	});
	assert.equal(oversized.code, "ENVELOPE_TOO_LARGE");

	const manyNodes = Array.from({ length: 101 }, (_, index) => node({ ref: `n${index}` }));
	assert.equal(
		protocolError(observeResult(observation({ nodes: manyNodes, visited: manyNodes.length }))).code,
		"TOO_LARGE",
	);

	const wide = Array.from({ length: 100 }, (_, index) => node({ ref: `n${index}`, text: "t".repeat(200) }));
	assert.equal(protocolError(observeResult(observation({ nodes: wide, visited: wide.length }))).code, "TOO_LARGE");
	assert.ok(MAX_OBSERVATION_BYTES < MAX_ENVELOPE_BYTES);

	assert.equal(protocolError(observeResult(observation({ visited: 1501 }))).code, "BAD_ENVELOPE");
	assert.equal(
		protocolError(observeResult(observation({ nodes: [node({ depth: 101 })], visited: 1 }))).code,
		"BAD_ENVELOPE",
	);
	assert.equal(protocolError(observeResult(observation({ omitted: ["everything"] }))).code, "BAD_ENVELOPE");
});

test("free-form error text is bounded so it cannot become a bulk channel", () => {
	const frame = (message: string): FrameValue => ({
		v: 1,
		seq: 1,
		type: "result",
		connectionId: "c1",
		requestId: "r1",
		ok: false,
		error: { code: "INTERNAL", message, retryable: false },
	});
	assert.equal(protocolError(frame("x".repeat(201))).code, "TOO_LARGE");
	assert.equal(parseFrame(frame("x".repeat(200))).type, "result");
});

test("focus and scroll requests require current binding, observation and reference", () => {
	const missingRef = protocolError(request({ op: "focus", binding: binding(), observationId: "o1" }));
	assert.equal(missingRef.code, "BAD_ENVELOPE");
	assert.equal(
		protocolError(request({ op: "scroll", binding: binding(), observationId: "o1", deltaY: 10, extra: 1 })).code,
		"BAD_ENVELOPE",
	);
	assert.equal(
		protocolError(request({ op: "scroll", binding: binding(), observationId: "o1", deltaY: Number.NaN })).code,
		"BAD_ENVELOPE",
	);
	assert.equal(
		protocolError(request({ op: "scroll", binding: binding(), observationId: "o1", deltaY: 1_000_001 })).code,
		"BAD_ENVELOPE",
	);
	const empty = protocolError(request({ op: "scroll", binding: binding(), observationId: "o1" }));
	assert.equal(empty.code, "BAD_ARGUMENT");
	assert.equal(protocolError(request({ op: "unknown-op", binding: binding() })).code, "UNSUPPORTED_OP");
});

test("cancel carries only a request id, never a binding", () => {
	const parsed = parseFrame(request({ op: "cancel", requestId: "r1" }));
	assert.equal(parsed.type, "request");
	assert.equal(protocolError(request({ op: "cancel", requestId: "r1", binding: binding() })).code, "BAD_ENVELOPE");
});

test("codec round-trips validated frames and rejects non-object JSON text", () => {
	const frame = parseFrame(observeResult(observation({ nodes: [node({ text: "hello" })], visited: 1 })));
	const text = encodeFrame(frame);
	assert.equal(frameByteLength(frame), utf8TextByteLength(text));
	assert.equal(parseFrame(decodeFrame(text)).type, "result");
	assert.equal(
		(() => {
			try {
				decodeFrame("{oops");
			} catch (error) {
				return (error as ProtocolError).code;
			}
		})(),
		"BAD_JSON",
	);
	assert.equal(
		(() => {
			try {
				decodeFrame("[]");
			} catch (error) {
				return (error as ProtocolError).code;
			}
		})(),
		"NOT_JSON_OBJECT",
	);
	assert.equal(
		(() => {
			try {
				decodeFrame("null");
			} catch (error) {
				return (error as ProtocolError).code;
			}
		})(),
		"NOT_JSON_OBJECT",
	);
});

test("wire errors reject transport-local codes", () => {
	// types.ts documents framing/decoding codes as local to a byte transport; they must never be
	// accepted from a peer, or a peer could fake a local failure.
	for (const code of [
		"BAD_FRAME_LENGTH",
		"FRAME_TOO_LARGE",
		"TRUNCATED_FRAME",
		"BAD_UTF8",
		"BAD_JSON",
		"NOT_JSON_OBJECT",
	]) {
		const frame = {
			v: 1,
			seq: 1,
			type: "result",
			connectionId: "c1",
			requestId: "r1",
			ok: false,
			error: { code, message: "m", retryable: false },
		};
		assert.equal(protocolError(frame).code, "BAD_ENVELOPE", code);
	}
	assert.equal(
		protocolError({
			v: 1,
			seq: 1,
			type: "result",
			connectionId: "c1",
			requestId: "r1",
			ok: false,
			error: { code: "NOT_A_CODE", message: "m", retryable: false },
		}).code,
		"BAD_ENVELOPE",
	);
});

test("identifiers and displayed text cannot carry control characters or ANSI escapes", () => {
	// These values reach the CLI terminal. A page-influenced ref, title or error message must not be
	// able to forge log lines or emit escapes.
	const escapes = ["a\nb", "a\u001b[2Jb", "a\u0007b", "a\u009bb", "a\u202eb"];
	for (const value of escapes) {
		assert.equal(
			protocolError(request({ op: "observe", binding: binding({ grantId: value }) })).code,
			"BAD_ENVELOPE",
			`grantId ${JSON.stringify(value)}`,
		);
		assert.equal(
			protocolError({
				v: 1,
				seq: 1,
				type: "result",
				connectionId: "c1",
				requestId: value,
				ok: false,
				error: { code: "INTERNAL", message: "m", retryable: false },
			}).code,
			"BAD_ENVELOPE",
			`requestId ${JSON.stringify(value)}`,
		);
		assert.equal(
			protocolError({
				v: 1,
				seq: 1,
				type: "result",
				connectionId: "c1",
				requestId: "r1",
				ok: false,
				error: { code: "INTERNAL", message: value, retryable: false },
			}).code,
			"BAD_ENVELOPE",
			`error.message ${JSON.stringify(value)}`,
		);
		assert.equal(
			protocolError(observeResult(observation({ title: value }))).code,
			"BAD_ENVELOPE",
			`title ${JSON.stringify(value)}`,
		);
	}
	// Ordinary page text stays allowed: the protocol must not reject legitimate content.
	const prose = parseFrame(
		observeResult(observation({ title: "Ünïcödé — text", nodes: [node({ text: "line one line two" })], visited: 1 })),
	);
	assert.equal(prose.type, "result");
});

test("observation urls are shape-checked so redaction has a backstop", () => {
	for (const url of [
		"https://user:pw@host.test/p",
		"https://host.test/p?token=SECRET",
		"https://host.test/p#frag",
		"javascript:alert(1)",
		"data:text/html,<p>x",
		"file:///etc/passwd",
		"https://host.test/\u001b[31m",
	]) {
		assert.equal(protocolError(observeResult(observation({ url }))).code, "BAD_ENVELOPE", url);
	}
	assert.equal(parseFrame(observeResult(observation({ url: "https://host.test/path/to/page" }))).type, "result");
});

test("scroll requests are unambiguous and bounded by the shared limit", () => {
	assert.equal(
		protocolError(request({ op: "scroll", binding: binding(), observationId: "o1", align: "end" })).code,
		"BAD_ARGUMENT",
	);
	assert.equal(
		protocolError(request({ op: "scroll", binding: binding(), observationId: "o1", ref: "n1", deltaY: 500 })).code,
		"BAD_ARGUMENT",
	);
	assert.equal(
		protocolError(request({ op: "scroll", binding: binding(), observationId: "o1", deltaY: MAX_SCROLL_DELTA + 1 }))
			.code,
		"BAD_ENVELOPE",
	);
	const accepted = parseFrame(
		request({ op: "scroll", binding: binding(), observationId: "o1", deltaY: -MAX_SCROLL_DELTA }),
	);
	assert.equal(accepted.type, "request");
});

test("capture completeness cannot claim fewer inspected elements than returned nodes", () => {
	// `visited` counts inspected elements including ones not returned, so it cannot be lower than the
	// node count. A frame that reports otherwise misrepresents the capture as complete.
	assert.equal(protocolError(observeResult(observation({ nodes: [node()], visited: 0 }))).code, "BAD_ENVELOPE");
	assert.equal(parseFrame(observeResult(observation({ nodes: [node()], visited: 1 }))).type, "result");
	// A time-budget truncation has no OmittedRegion code in the frozen enum, so `truncated: true` with an
	// empty `omitted` list is legal by design and is accepted here on purpose.
	assert.equal(parseFrame(observeResult(observation({ truncated: true, omitted: [], visited: 0 }))).type, "result");
});
