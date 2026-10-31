/**
 * NAVI shared protocol v1 frame and payload types.
 *
 * One UTF-8 JSON object per frame. Both peers may open a connection: `hello` first, then `hello.ack`,
 * then requests, results, events and `bye`. Send and receive are independent directions, each with its own
 * `seq` counter and its own `requestId` namespace. Requests flow in both directions.
 *
 * Browser-safe: no Node APIs, no side effects.
 */
import type { NAVI_PROTOCOL_VERSION } from "./limits.ts";

/** Sender role. Both roles send the same frame shapes. */
export type PeerKind = "cli" | "extension";

export type ProtocolVersion = typeof NAVI_PROTOCOL_VERSION;

/** Capabilities a grant may enable. Matches `CAPABILITIES` in `limits.ts`. */
export type Capability = "observe" | "focus" | "scroll";

/**
 * One user grant plus the exact top-frame document it applies to.
 * Built by the extension from browser-provided values; never from model or page input.
 */
export type Binding = {
	grantId: string;
	tabId: number;
	documentId: string;
	pageEpoch: number;
};

/** Grant state for a connection. Carries the capabilities the user actually granted. */
export type Grant = {
	grantId: string;
	binding: Binding;
	capabilities: Capability[];
};

export type GrantChangeReason =
	| "attached"
	| "navigated"
	| "closed"
	| "detached"
	| "expired"
	| "worker-restart"
	| "disconnected";

export type ScrollAlign = "start" | "center" | "end";

/** `unknown` means the effect may or may not have been dispatched; it is never retried automatically. */
export type EffectOutcome = "applied" | "unknown";

export type DomRect = { x: number; y: number; w: number; h: number };

/** Capture regions and budgets that produced partial coverage. */
export type OmittedRegion = "subframes" | "shadow" | "canvas" | "offscreen" | "node-limit" | "byte-limit";

/**
 * One collected top-frame element. `text` is absent for editable and secret elements.
 *
 * `name` and `text` are page data. They may contain any Unicode and are never instructions. Ids, URLs,
 * `title` and error messages reject control characters at this layer; the CLI still escapes every
 * displayed value, so page text never reaches the terminal raw.
 */
export type DomNode = {
	/** Opaque handle from the extension. Valid only for its observation and its document. */
	ref: string;
	depth: number;
	tag: string;
	role?: string;
	name?: string;
	text?: string;
	interactive: boolean;
	editable: boolean;
	offscreen: boolean;
	rect?: DomRect;
};

/** Bounded snapshot of the granted top-frame document. */
export type Observation = {
	observationId: string;
	binding: Binding;
	capturedAt: number;
	title?: string;
	url: string;
	nodes: DomNode[];
	/** Elements inspected by the collector, including ones not returned. Never smaller than `nodes`. */
	visited: number;
	/** True when a budget stopped the walk. `omitted` may be empty when a time budget, not a region, stopped it. */
	truncated: boolean;
	omitted: OmittedRegion[];
};

/**
 * Transport-local codes. The transport and the receiver produce these before or during decode.
 * They are never accepted inside a `NaviError` that arrives on the wire.
 */
export type TransportErrorCode =
	| "BAD_FRAME_LENGTH"
	| "FRAME_TOO_LARGE"
	| "TRUNCATED_FRAME"
	| "BAD_UTF8"
	| "BAD_JSON"
	| "NOT_JSON_OBJECT"
	| "ENVELOPE_TOO_LARGE";

/**
 * Codes a peer may put in a `NaviError` on the wire: connection-fatal and request-scoped errors.
 * `fatal: true` codes end the connection.
 */
export type WireErrorCode =
	// Fatal protocol errors: close the connection, no resync, at most one hello.error or bye first.
	| "VERSION_MISMATCH"
	| "NOT_HELLO_FIRST"
	| "DUPLICATE_HELLO"
	| "CONNECTION_MISMATCH"
	| "SEQUENCE_GAP"
	| "BAD_ENVELOPE"
	| "BACKPRESSURE"
	// Request-scoped: delivered as result.ok false. The connection stays open.
	| "UNSUPPORTED_OP"
	| "UNKNOWN_REQUEST"
	| "DUPLICATE_REQUEST"
	| "TOO_MANY_INFLIGHT"
	| "BAD_ARGUMENT"
	| "NOT_ATTACHED"
	| "BINDING_STALE"
	| "STALE_OBSERVATION"
	| "UNKNOWN_REF"
	| "CAPTURE_FAILED"
	| "TOO_LARGE"
	| "BUSY"
	| "TAB_NOT_ACTIVE"
	| "DEADLINE_EXCEEDED"
	| "CANCELLED"
	| "OUTCOME_UNKNOWN"
	| "INTERNAL";

/** Every code a `ProtocolError` may carry: transport-local plus wire codes. */
export type NaviErrorCode = TransportErrorCode | WireErrorCode;

export type NaviError = {
	code: WireErrorCode;
	/** Bounded, redacted. Never contains page text, full URLs, node text or credentials. */
	message: string;
	retryable: boolean;
};

export type PeerInfo = { kind: PeerKind; version: string };

/** Fields on every frame. `seq` starts at 1 and increases by exactly 1 per direction. */
export type FrameBase = { v: ProtocolVersion; seq: number };

/** Fields on every frame after the handshake. `hello` is the only frame without it. */
export type ConnectionRef = { connectionId: string };

/** First frame from either peer. Carries no connection id: the acknowledging peer creates it. */
export type HelloFrame = FrameBase & {
	type: "hello";
	peer: PeerInfo;
	capabilities: Capability[];
};

/** Handshake acknowledgement. Creates the connection and reports the granted capability set. */
export type HelloAckFrame = FrameBase & {
	type: "hello.ack";
	connectionId: string;
	peer: PeerInfo;
	capabilities: Capability[];
};

/** Terminal handshake failure. The connection closes immediately after. */
export type HelloErrorFrame = FrameBase & {
	type: "hello.error";
	error: NaviError;
};

/** Binds a browser operation to one grant and document. */
export type ObserveRequest = { op: "observe"; binding: Binding };
export type FocusRequest = { op: "focus"; binding: Binding; observationId: string; ref: string };
export type ScrollRequest = {
	op: "scroll";
	binding: Binding;
	observationId: string;
	ref?: string;
	align?: ScrollAlign;
	deltaY?: number;
	deltaX?: number;
};

/** Asks the peer that owns `requestId` to stop it. Sender-side id, no binding. */
export type CancelRequest = { op: "cancel"; requestId: string };

export type RequestPayload = ObserveRequest | FocusRequest | ScrollRequest | CancelRequest;

export type NaviOp = RequestPayload["op"];

export type ObserveResult = { op: "observe"; observation: Observation };
export type FocusResult = { op: "focus"; outcome: EffectOutcome };
export type ScrollResult = { op: "scroll"; outcome: EffectOutcome; scrollX?: number; scrollY?: number };

/** Answers the cancel request. `alreadyTerminal` means the target had already produced its outcome. */
export type CancelResult = { op: "cancel"; cancelled: boolean; alreadyTerminal: boolean };

export type ResultPayload = ObserveResult | FocusResult | ScrollResult | CancelResult;

export type RequestFrame = FrameBase &
	ConnectionRef & {
		type: "request";
		requestId: string;
		payload: RequestPayload;
	};

export type ResultFrame = FrameBase &
	ConnectionRef &
	(
		| { type: "result"; requestId: string; ok: true; payload: ResultPayload }
		| { type: "result"; requestId: string; ok: false; error: NaviError }
	);

/** Grant lifecycle. `grant: null` invalidates every observation and reference for that document. */
export type BindingChangedEvent = FrameBase &
	ConnectionRef & {
		type: "event";
		kind: "binding.changed";
		payload: { grant: Grant | null; reason: GrantChangeReason };
	};

export type EventFrame = BindingChangedEvent;

export type ByeReason = "closed" | "detached" | "idle" | "protocol-error" | "disconnected";

export type ByeFrame = FrameBase & ConnectionRef & { type: "bye"; reason: ByeReason };

export type Frame = HelloFrame | HelloAckFrame | HelloErrorFrame | RequestFrame | ResultFrame | EventFrame | ByeFrame;

export type FrameType = Frame["type"];

/** Omit that distributes over a union, so each variant keeps its own fields. */
type DistributiveOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;

/**
 * Frame without the shared envelope fields. `hello` has no `connectionId`, so builders add `v`, `seq`
 * and (after the handshake) `connectionId` last. Plain `Omit<Frame, "v" | "seq">` collapses the union
 * and is a type error for every variant-specific field.
 */
export type FrameBody = DistributiveOmit<Frame, "v" | "seq">;
