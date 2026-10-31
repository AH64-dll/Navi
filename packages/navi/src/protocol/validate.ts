/**
 * Runtime validation for NAVI protocol v1 frames.
 *
 * `parseFrame` is the single receiver-side gate: it accepts an untrusted value, checks the exact schema
 * (unknown fields, types, bounds, serialized byte budget) and throws `ProtocolError` on any deviation.
 * It returns the validated object it was given; callers must treat frames as immutable.
 *
 * Browser-safe: no Node APIs, no side effects.
 */
import {
	CAPABILITIES,
	MAX_ENVELOPE_BYTES,
	MAX_ERROR_MESSAGE_CHARS,
	MAX_ID_CHARS,
	MAX_NODES,
	MAX_OBSERVATION_BYTES,
	MAX_SCROLL_DELTA,
	MAX_SEQUENCE,
	MAX_TEXT_CHARS,
	MAX_TITLE_CHARS,
	MAX_URL_CHARS,
	MAX_VERSION_CHARS,
	MAX_VISITED_ELEMENTS,
	NAVI_PROTOCOL_VERSION,
} from "./limits.ts";
import type {
	Binding,
	ByeFrame,
	Capability,
	DomNode,
	DomRect,
	EventFrame,
	Frame,
	GrantChangeReason,
	HelloAckFrame,
	HelloErrorFrame,
	HelloFrame,
	NaviError,
	NaviErrorCode,
	OmittedRegion,
	PeerInfo,
	RequestFrame,
	ResultFrame,
	ScrollAlign,
	WireErrorCode,
} from "./types.ts";

export class ProtocolError extends Error {
	readonly code: NaviErrorCode;
	/** Dotted path of the offending field, for example `payload.observation.nodes[3].ref`. */
	readonly path: string;

	constructor(code: NaviErrorCode, path: string, message: string) {
		super(`${path || "frame"}: ${message}`);
		this.name = "ProtocolError";
		this.code = code;
		this.path = path;
	}
}

const NAVI_ERROR_CODES = [
	"BAD_FRAME_LENGTH",
	"FRAME_TOO_LARGE",
	"TRUNCATED_FRAME",
	"BAD_UTF8",
	"BAD_JSON",
	"NOT_JSON_OBJECT",
	"ENVELOPE_TOO_LARGE",
	"VERSION_MISMATCH",
	"NOT_HELLO_FIRST",
	"DUPLICATE_HELLO",
	"CONNECTION_MISMATCH",
	"SEQUENCE_GAP",
	"BAD_ENVELOPE",
	"BACKPRESSURE",
	"UNSUPPORTED_OP",
	"UNKNOWN_REQUEST",
	"DUPLICATE_REQUEST",
	"TOO_MANY_INFLIGHT",
	"BAD_ARGUMENT",
	"NOT_ATTACHED",
	"BINDING_STALE",
	"STALE_OBSERVATION",
	"UNKNOWN_REF",
	"CAPTURE_FAILED",
	"TOO_LARGE",
	"BUSY",
	"TAB_NOT_ACTIVE",
	"DEADLINE_EXCEEDED",
	"CANCELLED",
	"OUTCOME_UNKNOWN",
	"INTERNAL",
] as const;

/** Codes that may travel inside a `NaviError`. Transport-local codes are rejected on the wire. */
const WIRE_ERROR_CODES = [
	"VERSION_MISMATCH",
	"NOT_HELLO_FIRST",
	"DUPLICATE_HELLO",
	"CONNECTION_MISMATCH",
	"SEQUENCE_GAP",
	"BAD_ENVELOPE",
	"BACKPRESSURE",
	"UNSUPPORTED_OP",
	"UNKNOWN_REQUEST",
	"DUPLICATE_REQUEST",
	"TOO_MANY_INFLIGHT",
	"BAD_ARGUMENT",
	"NOT_ATTACHED",
	"BINDING_STALE",
	"STALE_OBSERVATION",
	"UNKNOWN_REF",
	"CAPTURE_FAILED",
	"TOO_LARGE",
	"BUSY",
	"TAB_NOT_ACTIVE",
	"DEADLINE_EXCEEDED",
	"CANCELLED",
	"OUTCOME_UNKNOWN",
	"INTERNAL",
] as const;

type ListedErrorCode = (typeof NAVI_ERROR_CODES)[number];
type MissingErrorCode = Exclude<NaviErrorCode, ListedErrorCode>;
type ListedWireErrorCode = (typeof WIRE_ERROR_CODES)[number];
type MissingWireErrorCode = Exclude<WireErrorCode, ListedWireErrorCode>;

/** Compile-time guard: adding a code to `NaviErrorCode` without listing it here is a type error. */
export const NAVI_ERROR_CODES_ARE_EXHAUSTIVE: MissingErrorCode extends never ? true : never = true;

/** Compile-time guard: `WIRE_ERROR_CODES` must list exactly the codes a peer may send. */
export const NAVI_WIRE_ERROR_CODES_ARE_EXHAUSTIVE: MissingWireErrorCode extends never ? true : never = true;

/** Conservative tokens. Ids and version strings are ASCII only, so control and bidi characters cannot appear. */
const ID_PATTERN = /^[A-Za-z0-9._:-]+$/;
const TAG_PATTERN = /^[a-z][a-z0-9-]*$/;
const ROLE_PATTERN = /^[a-z][a-z-]*$/;
const VERSION_PATTERN = /^[A-Za-z0-9._:+-]+$/;

/**
 * C0 and C1 controls, DEL, and bidi overrides/isolates. Rejected in ids, URLs, `title` and error
 * messages so a page-influenced value cannot inject terminal escapes or reorder CLI output.
 * Page `name`/`text` are exempt: they are data, and the CLI escapes them at display time.
 */
const FORBIDDEN_TEXT_CHARACTERS = /[\u0000-\u001F\u007F-\u009F\u202A-\u202E\u2066-\u2069]/;

function hasForbiddenCharacters(value: string): boolean {
	return FORBIDDEN_TEXT_CHARACTERS.test(value);
}

const OMITTED_REGIONS: readonly OmittedRegion[] = [
	"subframes",
	"shadow",
	"canvas",
	"offscreen",
	"node-limit",
	"byte-limit",
];
const SCROLL_ALIGNS: readonly ScrollAlign[] = ["start", "center", "end"];
const GRANT_CHANGE_REASONS: readonly GrantChangeReason[] = [
	"attached",
	"navigated",
	"closed",
	"detached",
	"expired",
	"worker-restart",
	"disconnected",
];
const BYE_REASONS = ["closed", "detached", "idle", "protocol-error", "disconnected"] as const;
const OUTCOMES = ["applied", "unknown"] as const;
const FRAME_TYPES = ["hello", "hello.ack", "hello.error", "request", "result", "event", "bye"] as const;

const textEncoder = new TextEncoder();

/** UTF-8 byte length of a text string. */
export function utf8TextByteLength(text: string): number {
	return textEncoder.encode(text).length;
}

/** UTF-8 byte length of the exact serialized value. */
export function utf8ByteLength(value: unknown): number {
	let text: string;
	try {
		text = JSON.stringify(value);
	} catch {
		throw new ProtocolError("BAD_ENVELOPE", "", "Frame is not serializable as JSON");
	}
	if (typeof text !== "string") throw new ProtocolError("BAD_ENVELOPE", "", "Frame is not a JSON value");
	return utf8TextByteLength(text);
}

export function isCapability(value: unknown): value is Capability {
	return typeof value === "string" && (CAPABILITIES as readonly string[]).includes(value);
}

function fail(code: NaviErrorCode, path: string, message: string): never {
	throw new ProtocolError(code, path, message);
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function expectRecord(value: unknown, path: string): Record<string, unknown> {
	if (!isRecord(value)) fail("NOT_JSON_OBJECT", path, "Expected a JSON object");
	return value;
}

/** Rejects unknown fields, missing required fields and explicit `undefined` (not a JSON value). */
function expectKeys(
	value: Record<string, unknown>,
	allowed: readonly string[],
	required: readonly string[],
	path: string,
): void {
	for (const key of Object.keys(value)) {
		const childPath = path ? `${path}.${key}` : key;
		if (!allowed.includes(key)) fail("BAD_ENVELOPE", childPath, "Unknown field");
		if (value[key] === undefined) fail("NOT_JSON_OBJECT", childPath, "Undefined is not a JSON value");
	}
	for (const key of required) {
		if (!(key in value)) fail("BAD_ENVELOPE", path ? `${path}.${key}` : key, "Missing field");
	}
}

function expectString(value: unknown, path: string, maxChars: number, allowEmpty: boolean): string {
	if (typeof value !== "string") fail("BAD_ENVELOPE", path, "Expected a string");
	if (!allowEmpty && value.length === 0) fail("BAD_ENVELOPE", path, "Expected a non-empty string");
	if (value.length > maxChars) fail("TOO_LARGE", path, `String exceeds ${maxChars} characters`);
	return value;
}

function expectId(value: unknown, path: string): string {
	const id = expectString(value, path, MAX_ID_CHARS, false);
	if (!ID_PATTERN.test(id))
		fail("BAD_ENVELOPE", path, "Id must use ASCII letters, digits, dot, underscore, colon or hyphen");
	return id;
}

/** Short ASCII token: ids, tag names, ARIA role tokens and version strings. */
function expectToken(value: unknown, path: string, pattern: RegExp, maxChars: number, description: string): string {
	const token = expectString(value, path, maxChars, false);
	if (!pattern.test(token)) fail("BAD_ENVELOPE", path, description);
	return token;
}

/**
 * Free text from the protocol's own surface: rejects control and bidi-override characters.
 * Page content uses `expectString` instead and is escaped for display by the CLI.
 */
function expectSafeText(value: unknown, path: string, maxChars: number, allowEmpty: boolean): string {
	const text = expectString(value, path, maxChars, allowEmpty);
	if (hasForbiddenCharacters(text)) fail("BAD_ENVELOPE", path, "Text contains control characters");
	return text;
}

/**
 * Display URL: absolute http(s), canonical origin plus path, no credentials, query or fragment.
 * The CLI never sees a page-supplied URL that still carries secrets.
 */
function expectDisplayUrl(value: unknown, path: string): string {
	const text = expectString(value, path, MAX_URL_CHARS, false);
	if (hasForbiddenCharacters(text)) fail("BAD_ENVELOPE", path, "URL contains control characters");
	let parsed: URL | undefined;
	try {
		parsed = new URL(text);
	} catch {
		parsed = undefined;
	}
	if (parsed === undefined) fail("BAD_ENVELOPE", path, "URL must be absolute");
	if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
		fail("BAD_ENVELOPE", path, "Only http and https URLs are allowed");
	}
	if (parsed.username !== "" || parsed.password !== "") fail("BAD_ENVELOPE", path, "URL must not contain credentials");
	if (parsed.search !== "" || parsed.hash !== "")
		fail("BAD_ENVELOPE", path, "URL must not contain a query or fragment");
	if (text !== `${parsed.origin}${parsed.pathname}`) {
		fail("BAD_ENVELOPE", path, "URL must be a canonical origin and path");
	}
	return text;
}

function expectBoolean(value: unknown, path: string): boolean {
	if (typeof value !== "boolean") fail("BAD_ENVELOPE", path, "Expected a boolean");
	return value;
}

function expectInt(value: unknown, path: string, min: number, max: number): number {
	if (typeof value !== "number" || !Number.isSafeInteger(value)) fail("BAD_ENVELOPE", path, "Expected a safe integer");
	if (value < min || value > max) fail("BAD_ENVELOPE", path, `Expected an integer between ${min} and ${max}`);
	return value;
}

function expectOptionalInt(value: unknown, path: string, min: number, max: number): number | undefined {
	return value === undefined ? undefined : expectInt(value, path, min, max);
}

function expectOptionalString(value: unknown, path: string, maxChars: number): string | undefined {
	return value === undefined ? undefined : expectString(value, path, maxChars, true);
}

function expectEnum<T extends string>(value: unknown, path: string, allowed: readonly T[]): T {
	if (typeof value !== "string" || !(allowed as readonly string[]).includes(value)) {
		fail("BAD_ENVELOPE", path, `Expected one of ${allowed.join(", ")}`);
	}
	return value as T;
}

function expectCapabilityList(value: unknown, path: string): Capability[] {
	if (!Array.isArray(value)) fail("BAD_ENVELOPE", path, "Expected an array of capabilities");
	if (value.length === 0) fail("BAD_ENVELOPE", path, "Expected at least one capability");
	const seen = new Set<string>();
	const capabilities: Capability[] = [];
	for (let index = 0; index < value.length; index++) {
		const capability = value[index];
		if (!isCapability(capability)) fail("BAD_ENVELOPE", `${path}[${index}]`, "Unknown capability");
		if (seen.has(capability)) fail("BAD_ENVELOPE", `${path}[${index}]`, "Duplicate capability");
		seen.add(capability);
		capabilities.push(capability);
	}
	return capabilities;
}

function expectBinding(value: unknown, path: string): Binding {
	const binding = expectRecord(value, path);
	expectKeys(
		binding,
		["grantId", "tabId", "documentId", "pageEpoch"],
		["grantId", "tabId", "documentId", "pageEpoch"],
		path,
	);
	return {
		grantId: expectId(binding.grantId, `${path}.grantId`),
		tabId: expectInt(binding.tabId, `${path}.tabId`, 0, Number.MAX_SAFE_INTEGER),
		documentId: expectId(binding.documentId, `${path}.documentId`),
		pageEpoch: expectInt(binding.pageEpoch, `${path}.pageEpoch`, 0, Number.MAX_SAFE_INTEGER),
	};
}

function expectGrant(value: unknown, path: string): void {
	const grant = expectRecord(value, path);
	expectKeys(grant, ["grantId", "binding", "capabilities"], ["grantId", "binding", "capabilities"], path);
	const grantId = expectId(grant.grantId, `${path}.grantId`);
	const binding = expectBinding(grant.binding, `${path}.binding`);
	expectCapabilityList(grant.capabilities, `${path}.capabilities`);
	if (grantId !== binding.grantId) fail("BAD_ARGUMENT", `${path}.grantId`, "Grant id must match its binding");
}

function expectRect(value: unknown, path: string): DomRect {
	const rect = expectRecord(value, path);
	expectKeys(rect, ["x", "y", "w", "h"], ["x", "y", "w", "h"], path);
	return {
		x: expectInt(rect.x, `${path}.x`, -1_000_000, 1_000_000),
		y: expectInt(rect.y, `${path}.y`, -1_000_000, 1_000_000),
		w: expectInt(rect.w, `${path}.w`, 0, 1_000_000),
		h: expectInt(rect.h, `${path}.h`, 0, 1_000_000),
	};
}

function expectNode(value: unknown, path: string): DomNode {
	const node = expectRecord(value, path);
	expectKeys(
		node,
		["ref", "depth", "tag", "role", "name", "text", "interactive", "editable", "offscreen", "rect"],
		["ref", "depth", "tag", "interactive", "editable", "offscreen"],
		path,
	);
	const editable = expectBoolean(node.editable, `${path}.editable`);
	if (editable && node.text !== undefined) {
		fail("BAD_ENVELOPE", `${path}.text`, "Editable elements must not carry text");
	}
	return {
		ref: expectId(node.ref, `${path}.ref`),
		depth: expectInt(node.depth, `${path}.depth`, 0, MAX_NODES),
		tag: expectToken(node.tag, `${path}.tag`, TAG_PATTERN, MAX_ID_CHARS, "Tag must be a lowercase element name"),
		role:
			node.role === undefined
				? undefined
				: expectToken(node.role, `${path}.role`, ROLE_PATTERN, MAX_ID_CHARS, "Role must be a lowercase ARIA token"),
		name: expectOptionalString(node.name, `${path}.name`, MAX_TEXT_CHARS),
		text: expectOptionalString(node.text, `${path}.text`, MAX_TEXT_CHARS),
		interactive: expectBoolean(node.interactive, `${path}.interactive`),
		editable,
		offscreen: expectBoolean(node.offscreen, `${path}.offscreen`),
		rect: node.rect === undefined ? undefined : expectRect(node.rect, `${path}.rect`),
	};
}

function expectObservation(value: unknown, path: string): void {
	const observation = expectRecord(value, path);
	expectKeys(
		observation,
		["observationId", "binding", "capturedAt", "title", "url", "nodes", "visited", "truncated", "omitted"],
		["observationId", "binding", "capturedAt", "url", "nodes", "visited", "truncated", "omitted"],
		path,
	);
	expectId(observation.observationId, `${path}.observationId`);
	expectBinding(observation.binding, `${path}.binding`);
	expectInt(observation.capturedAt, `${path}.capturedAt`, 0, Number.MAX_SAFE_INTEGER);
	if (observation.title !== undefined) {
		expectSafeText(observation.title, `${path}.title`, MAX_TITLE_CHARS, true);
	}
	expectDisplayUrl(observation.url, `${path}.url`);
	const nodes = observation.nodes;
	if (!Array.isArray(nodes)) fail("BAD_ENVELOPE", `${path}.nodes`, "Expected an array of nodes");
	if (nodes.length > MAX_NODES) fail("TOO_LARGE", `${path}.nodes`, `Observation exceeds ${MAX_NODES} nodes`);
	for (let index = 0; index < nodes.length; index++) expectNode(nodes[index], `${path}.nodes[${index}]`);
	const visited = expectInt(observation.visited, `${path}.visited`, 0, MAX_VISITED_ELEMENTS);
	// Returned nodes are a subset of the elements the collector visited.
	if (visited < nodes.length) {
		fail("BAD_ENVELOPE", `${path}.visited`, "Visited count cannot be smaller than the returned node count");
	}
	expectBoolean(observation.truncated, `${path}.truncated`);
	const omitted = observation.omitted;
	if (!Array.isArray(omitted)) fail("BAD_ENVELOPE", `${path}.omitted`, "Expected an array of regions");
	for (let index = 0; index < omitted.length; index++) {
		expectEnum(omitted[index], `${path}.omitted[${index}]`, OMITTED_REGIONS);
	}
	if (utf8ByteLength(observation) > MAX_OBSERVATION_BYTES) {
		fail("TOO_LARGE", path, `Observation exceeds ${MAX_OBSERVATION_BYTES} bytes`);
	}
}

function expectError(value: unknown, path: string): NaviError {
	const error = expectRecord(value, path);
	expectKeys(error, ["code", "message", "retryable"], ["code", "message", "retryable"], path);
	if (typeof error.code !== "string" || !(WIRE_ERROR_CODES as readonly string[]).includes(error.code)) {
		fail("BAD_ENVELOPE", `${path}.code`, "Unknown or transport-local error code");
	}
	return {
		code: error.code as WireErrorCode,
		message: expectSafeText(error.message, `${path}.message`, MAX_ERROR_MESSAGE_CHARS, false),
		retryable: expectBoolean(error.retryable, `${path}.retryable`),
	};
}

function expectPeer(value: unknown, path: string): PeerInfo {
	const peer = expectRecord(value, path);
	expectKeys(peer, ["kind", "version"], ["kind", "version"], path);
	return {
		kind: expectEnum(peer.kind, `${path}.kind`, ["cli", "extension"] as const),
		version: expectToken(
			peer.version,
			`${path}.version`,
			VERSION_PATTERN,
			MAX_VERSION_CHARS,
			"Version must be an ASCII token",
		),
	};
}

function expectEnvelopeHeader(frame: Record<string, unknown>): void {
	if (frame.v !== NAVI_PROTOCOL_VERSION) {
		fail("VERSION_MISMATCH", "v", `Unsupported protocol version ${String(frame.v)}`);
	}
	const seq = frame.seq;
	if (typeof seq !== "number" || !Number.isSafeInteger(seq) || seq < 1 || seq > MAX_SEQUENCE) {
		fail("BAD_ENVELOPE", "seq", "Expected a strictly positive integer sequence number");
	}
	const type = frame.type;
	if (typeof type !== "string" || !(FRAME_TYPES as readonly string[]).includes(type)) {
		fail("BAD_ENVELOPE", "type", "Unknown frame type");
	}
}

function expectRequestPayload(frame: Record<string, unknown>): void {
	const payload = expectRecord(frame.payload, "payload");
	const op = payload.op;
	switch (op) {
		case "observe": {
			expectKeys(payload, ["op", "binding"], ["op", "binding"], "payload");
			expectBinding(payload.binding, "payload.binding");
			return;
		}
		case "focus": {
			expectKeys(
				payload,
				["op", "binding", "observationId", "ref"],
				["op", "binding", "observationId", "ref"],
				"payload",
			);
			expectBinding(payload.binding, "payload.binding");
			expectId(payload.observationId, "payload.observationId");
			expectId(payload.ref, "payload.ref");
			return;
		}
		case "scroll": {
			expectKeys(
				payload,
				["op", "binding", "observationId", "ref", "align", "deltaX", "deltaY"],
				["op", "binding", "observationId"],
				"payload",
			);
			expectBinding(payload.binding, "payload.binding");
			expectId(payload.observationId, "payload.observationId");
			const ref = payload.ref === undefined ? undefined : expectId(payload.ref, "payload.ref");
			const align =
				payload.align === undefined ? undefined : expectEnum(payload.align, "payload.align", SCROLL_ALIGNS);
			const deltaX = expectOptionalInt(payload.deltaX, "payload.deltaX", -MAX_SCROLL_DELTA, MAX_SCROLL_DELTA);
			const deltaY = expectOptionalInt(payload.deltaY, "payload.deltaY", -MAX_SCROLL_DELTA, MAX_SCROLL_DELTA);
			// One unambiguous shape: either scroll to a ref (optionally aligned) or scroll by a delta.
			if (align !== undefined && ref === undefined) {
				fail("BAD_ARGUMENT", "payload.ref", "Align needs a ref to scroll to");
			}
			if (ref !== undefined && (deltaX !== undefined || deltaY !== undefined)) {
				fail("BAD_ARGUMENT", "payload", "Scroll must use either a ref or a delta, not both");
			}
			if (ref === undefined && deltaX === undefined && deltaY === undefined) {
				fail("BAD_ARGUMENT", "payload", "Scroll needs a ref or a delta");
			}
			return;
		}
		case "cancel": {
			expectKeys(payload, ["op", "requestId"], ["op", "requestId"], "payload");
			expectId(payload.requestId, "payload.requestId");
			return;
		}
		default:
			fail("UNSUPPORTED_OP", "payload.op", "Unknown operation");
	}
}

function expectResultPayload(frame: Record<string, unknown>): void {
	const payload = expectRecord(frame.payload, "payload");
	switch (payload.op) {
		case "observe": {
			expectKeys(payload, ["op", "observation"], ["op", "observation"], "payload");
			expectObservation(payload.observation, "payload.observation");
			return;
		}
		case "focus": {
			expectKeys(payload, ["op", "outcome"], ["op", "outcome"], "payload");
			expectEnum(payload.outcome, "payload.outcome", OUTCOMES);
			return;
		}
		case "scroll": {
			expectKeys(payload, ["op", "outcome", "scrollX", "scrollY"], ["op", "outcome"], "payload");
			expectEnum(payload.outcome, "payload.outcome", OUTCOMES);
			expectOptionalInt(payload.scrollX, "payload.scrollX", -1_000_000_000, 1_000_000_000);
			expectOptionalInt(payload.scrollY, "payload.scrollY", -1_000_000_000, 1_000_000_000);
			return;
		}
		case "cancel": {
			expectKeys(payload, ["op", "cancelled", "alreadyTerminal"], ["op", "cancelled", "alreadyTerminal"], "payload");
			expectBoolean(payload.cancelled, "payload.cancelled");
			expectBoolean(payload.alreadyTerminal, "payload.alreadyTerminal");
			return;
		}
		default:
			fail("UNSUPPORTED_OP", "payload.op", "Unknown operation");
	}
}

/**
 * Validates one untrusted frame value.
 * Throws `ProtocolError` for any deviation; a returned frame is safe to handle.
 */
export function parseFrame(value: unknown): Frame {
	const frame = expectRecord(value, "");
	expectEnvelopeHeader(frame);
	// Budget first: a peer may not force deep validation of an oversized payload.
	if (utf8ByteLength(frame) > MAX_ENVELOPE_BYTES) fail("ENVELOPE_TOO_LARGE", "", "Frame exceeds the envelope budget");
	switch (frame.type as string) {
		case "hello": {
			expectKeys(
				frame,
				["v", "seq", "type", "peer", "capabilities"],
				["v", "seq", "type", "peer", "capabilities"],
				"",
			);
			expectPeer(frame.peer, "peer");
			expectCapabilityList(frame.capabilities, "capabilities");
			return frame as unknown as HelloFrame;
		}
		case "hello.ack": {
			expectKeys(
				frame,
				["v", "seq", "type", "connectionId", "peer", "capabilities"],
				["v", "seq", "type", "connectionId", "peer", "capabilities"],
				"",
			);
			expectId(frame.connectionId, "connectionId");
			expectPeer(frame.peer, "peer");
			expectCapabilityList(frame.capabilities, "capabilities");
			return frame as unknown as HelloAckFrame;
		}
		case "hello.error": {
			expectKeys(frame, ["v", "seq", "type", "error"], ["v", "seq", "type", "error"], "");
			expectError(frame.error, "error");
			return frame as unknown as HelloErrorFrame;
		}
		case "request": {
			expectKeys(
				frame,
				["v", "seq", "type", "connectionId", "requestId", "payload"],
				["v", "seq", "type", "connectionId", "requestId", "payload"],
				"",
			);
			expectId(frame.connectionId, "connectionId");
			expectId(frame.requestId, "requestId");
			expectRequestPayload(frame);
			return frame as unknown as RequestFrame;
		}
		case "result": {
			expectKeys(frame, ["v", "seq", "type", "connectionId", "requestId", "ok", "payload", "error"], [], "");
			const ok = expectBoolean(frame.ok, "ok");
			const resultKeys = ["v", "seq", "type", "connectionId", "requestId", "ok"] as const;
			expectKeys(
				frame,
				ok ? [...resultKeys, "payload"] : [...resultKeys, "error"],
				ok ? [...resultKeys, "payload"] : [...resultKeys, "error"],
				"",
			);
			expectId(frame.connectionId, "connectionId");
			expectId(frame.requestId, "requestId");
			if (ok) expectResultPayload(frame);
			else expectError(frame.error, "error");
			return frame as unknown as ResultFrame;
		}
		case "event": {
			expectKeys(
				frame,
				["v", "seq", "type", "connectionId", "kind", "payload"],
				["v", "seq", "type", "connectionId", "kind", "payload"],
				"",
			);
			expectId(frame.connectionId, "connectionId");
			expectEnum(frame.kind, "kind", ["binding.changed"] as const);
			const payload = expectRecord(frame.payload, "payload");
			expectKeys(payload, ["grant", "reason"], ["grant", "reason"], "payload");
			if (payload.grant !== null) expectGrant(payload.grant, "payload.grant");
			expectEnum(payload.reason, "payload.reason", GRANT_CHANGE_REASONS);
			return frame as unknown as EventFrame;
		}
		case "bye": {
			expectKeys(
				frame,
				["v", "seq", "type", "connectionId", "reason"],
				["v", "seq", "type", "connectionId", "reason"],
				"",
			);
			expectId(frame.connectionId, "connectionId");
			expectEnum(frame.reason, "reason", BYE_REASONS);
			return frame as unknown as ByeFrame;
		}
		default:
			fail("BAD_ENVELOPE", "type", "Unknown frame type");
	}
}
