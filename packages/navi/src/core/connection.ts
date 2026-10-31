/**
 * Protocol connection state for one link: handshake, per-direction sequence, request correlation,
 * deadlines, cancellation and terminal close handling.
 *
 * Browser-neutral: no Node, Chrome or DOM imports. Every inbound value passes `parseFrame`, so schema
 * and byte-budget checks happen here, once, for the CLI and for the worker.
 *
 * Enforced rules:
 * - `hello` is first; the responder mints the connection id and answers `hello.ack`.
 * - Outbound `seq` starts at 1 and increases by exactly 1; inbound `seq` must do the same.
 * - One browser operation in flight. `cancel` is a control request and is exempt from that bound.
 * - Work that may have been dispatched never reports success: deadline or disconnect yields
 *   `OUTCOME_UNKNOWN`, and nothing is retried automatically.
 * - Request-id memory is bounded; after a fixed count the link closes instead of growing a set.
 * - A peer that confirms a cancel sends no result for the cancelled request; the requester settles it.
 * - A cancel for an id this connection never saw is refused with `UNKNOWN_REQUEST`; `alreadyTerminal` is
 *   reported only for an id that did exist and already produced its terminal result.
 * - A handler that misses its deadline answers `OUTCOME_UNKNOWN` for focus/scroll and
 *   `DEADLINE_EXCEEDED` for observe, matching the requesting side.
 */
import {
	HANDSHAKE_TIMEOUT_MS,
	INTERACTION_TIMEOUT_MS,
	MAX_INFLIGHT_BROWSER_OPS,
	NAVI_PROTOCOL_VERSION,
	OBSERVE_TIMEOUT_MS,
} from "../protocol/limits.ts";
import type {
	BindingChangedEvent,
	ByeReason,
	CancelRequest,
	CancelResult,
	Capability,
	EventFrame,
	Frame,
	NaviError,
	NaviErrorCode,
	PeerInfo,
	RequestFrame,
	RequestPayload,
	ResultFrame,
	ResultPayload,
} from "../protocol/types.ts";
import { ProtocolError, parseFrame } from "../protocol/validate.ts";
import { naviError, toWireErrorCode } from "./errors.ts";
import type { ChannelCloseReason, Clock, FrameChannel } from "./ports.ts";
import { NaviRequestError } from "./ports.ts";

export type ConnectionRole = "initiator" | "responder";
export type ConnectionState = "handshaking" | "ready" | "closed";

export type ConnectionLimits = {
	handshakeTimeoutMs: number;
	observeTimeoutMs: number;
	interactionTimeoutMs: number;
	/** Recently settled request ids kept per direction, so duplicates and late results are detectable. */
	trackedRequestIds: number;
	/** Distinct inbound request ids accepted on one connection before it closes. */
	maxRequestIdsPerConnection: number;
};

export const DEFAULT_CONNECTION_LIMITS: ConnectionLimits = {
	handshakeTimeoutMs: HANDSHAKE_TIMEOUT_MS,
	observeTimeoutMs: OBSERVE_TIMEOUT_MS,
	interactionTimeoutMs: INTERACTION_TIMEOUT_MS,
	trackedRequestIds: 32,
	maxRequestIdsPerConnection: 512,
};

export type ReadyInfo = { connectionId: string; peer: PeerInfo; capabilities: Capability[] };

/** Answers one inbound request. Resolve with a result payload or an error; do not throw. */
export type RequestHandler = (request: RequestFrame, signal: AbortSignal) => Promise<ResultPayload | NaviError>;

export type ConnectionOptions = {
	role: ConnectionRole;
	channel: FrameChannel;
	clock: Clock;
	/** Our own peer descriptor and the capabilities we support. */
	peer: PeerInfo;
	capabilities: Capability[];
	limits?: Partial<ConnectionLimits>;
	newConnectionId?: () => string;
	newRequestId?: () => string;
	onReady?: (info: ReadyInfo) => void;
	onEvent?: (event: EventFrame) => void;
	onStateChange?: (state: ConnectionState, lastError?: NaviError) => void;
	handleRequest?: RequestHandler;
};

type DistributiveOmit<T, K extends PropertyKey> = T extends unknown ? Omit<T, K> : never;

type PendingEntry = {
	readonly requestId: string;
	readonly op: RequestPayload["op"];
	readonly resolve: (payload: ResultPayload) => void;
	readonly reject: (error: NaviRequestError) => void;
	readonly cancelTimer: () => void;
	readonly signal?: AbortSignal;
	readonly onAbort?: () => void;
};

type InboundOp = { readonly requestId: string; readonly controller: AbortController; readonly cancelTimer: () => void };

function defaultConnectionId(): string {
	const webcrypto = globalThis.crypto;
	if (webcrypto !== undefined && typeof webcrypto.randomUUID === "function") return `c-${webcrypto.randomUUID()}`;
	return `c-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 10)}`;
}

/** Effect operations can reach the page. A lost confirmation is an unknown outcome, not a failure. */
function isEffectOp(op: RequestPayload["op"]): boolean {
	return op === "focus" || op === "scroll";
}

export class Connection {
	private readonly options: ConnectionOptions;
	private readonly limits: ConnectionLimits;
	private readonly channel: FrameChannel;
	private readonly clock: Clock;
	private readonly pending = new Map<string, PendingEntry>();
	private readonly completedInboundIds: string[] = [];
	private readonly completedOutboundIds: string[] = [];

	private stateValue: ConnectionState = "handshaking";
	private connectionIdValue: string | undefined;
	private peerValue: PeerInfo | undefined;
	private peerCapabilitiesValue: Capability[] = [];
	private lastErrorValue: NaviError | undefined;
	/** Precise local code, which may be transport-local. Only the wire code is ever sent or displayed. */
	private lastErrorCodeValue: NaviErrorCode | undefined;
	private outboundSeq = 0;
	private lastInboundSeq = 0;
	private handshakeTimer: (() => void) | undefined;
	private requestCounter = 0;
	private inboundRequestIds = 0;
	private unknownResults = 0;
	private inboundOp: InboundOp | undefined;

	constructor(options: ConnectionOptions) {
		this.options = options;
		this.limits = { ...DEFAULT_CONNECTION_LIMITS, ...options.limits };
		this.channel = options.channel;
		this.clock = options.clock;
		this.channel.onMessage((raw) => this.handleRaw(raw));
		this.channel.onClose((reason) => this.handleChannelClose(reason));
	}

	get state(): ConnectionState {
		return this.stateValue;
	}

	get connectionId(): string | undefined {
		return this.connectionIdValue;
	}

	get peer(): PeerInfo | undefined {
		return this.peerValue;
	}

	get peerCapabilities(): Capability[] {
		return this.peerCapabilitiesValue;
	}

	get lastError(): NaviError | undefined {
		return this.lastErrorValue;
	}

	/** `lastError.code` is always a wire code; this keeps the precise local code for diagnostics. */
	get lastErrorDetail(): NaviErrorCode | undefined {
		return this.lastErrorCodeValue;
	}

	/** Request id of the single in-flight browser operation, when one exists. */
	get inFlightRequestId(): string | undefined {
		for (const entry of this.pending.values()) {
			if (entry.op !== "cancel") return entry.requestId;
		}
		return undefined;
	}

	/** Starts the handshake. The initiator sends `hello`; the responder waits for it. */
	open(): void {
		if (this.stateValue !== "handshaking") return;
		this.handshakeTimer = this.clock.schedule(() => {
			this.fatal("DEADLINE_EXCEEDED", "Handshake timed out");
		}, this.limits.handshakeTimeoutMs);
		if (this.options.role === "initiator") {
			this.send({ type: "hello", peer: this.options.peer, capabilities: this.options.capabilities });
		}
	}

	request(
		payload: RequestPayload,
		options: { signal?: AbortSignal; deadlineMs?: number } = {},
	): Promise<ResultPayload> {
		const connectionId = this.connectionIdValue;
		if (this.stateValue !== "ready" || connectionId === undefined) {
			return Promise.reject(
				new NaviRequestError(naviError("INTERNAL", "Connection is closed or still handshaking")),
			);
		}
		if (payload.op !== "cancel" && this.countPendingOps() >= MAX_INFLIGHT_BROWSER_OPS) {
			return Promise.reject(
				new NaviRequestError(naviError("BUSY", "Another browser operation is already in flight")),
			);
		}
		if (options.signal?.aborted === true) {
			return Promise.reject(
				new NaviRequestError(naviError("CANCELLED", "Request was cancelled before it was sent")),
			);
		}
		const requestId = this.mintRequestId();
		const deadlineMs =
			options.deadlineMs ??
			(payload.op === "observe" ? this.limits.observeTimeoutMs : this.limits.interactionTimeoutMs);
		return new Promise<ResultPayload>((resolve, reject) => {
			const cancelTimer = this.clock.schedule(() => {
				this.settlePending(requestId, new NaviRequestError(this.timeoutError(payload.op)));
			}, deadlineMs);
			const signal = options.signal;
			const onAbort =
				signal === undefined
					? undefined
					: () => {
							this.settlePending(requestId, new NaviRequestError(this.abortError(payload.op)));
							if (payload.op !== "cancel") this.sendCancelFor(requestId);
						};
			if (signal !== undefined && onAbort !== undefined) signal.addEventListener("abort", onAbort, { once: true });
			this.pending.set(requestId, { requestId, op: payload.op, resolve, reject, cancelTimer, signal, onAbort });
			this.send({ type: "request", connectionId, requestId, payload });
		});
	}

	/**
	 * Ask the peer to stop the in-flight operation. Cancelling does not undo a page effect: a confirmed
	 * cancel of an effect operation reports `OUTCOME_UNKNOWN`.
	 */
	async cancelInFlight(options: { signal?: AbortSignal } = {}): Promise<CancelResult> {
		const target = this.inFlightRequestId;
		if (target === undefined) return { op: "cancel", cancelled: false, alreadyTerminal: true };
		const result = await this.request({ op: "cancel", requestId: target }, options);
		if (result.op !== "cancel") {
			throw new NaviRequestError(naviError("INTERNAL", "Peer answered cancel with the wrong payload"));
		}
		const entry = this.pending.get(target);
		if (result.cancelled && entry !== undefined) {
			// The peer confirmed the cancel, so it sends no result for that request.
			this.settlePending(target, new NaviRequestError(this.abortError(entry.op)));
		}
		return result;
	}

	sendEvent(payload: BindingChangedEvent["payload"]): void {
		const connectionId = this.connectionIdValue;
		if (this.stateValue !== "ready" || connectionId === undefined) return;
		this.send({ type: "event", connectionId, kind: "binding.changed", payload });
	}

	close(reason: ByeReason, error?: NaviError): void {
		if (this.stateValue === "closed") return;
		this.cancelHandshakeTimer();
		const connectionId = this.connectionIdValue;
		if (connectionId !== undefined) {
			try {
				this.send({ type: "bye", connectionId, reason });
			} catch {
				// A full or broken queue is not a reason to fail the close path.
			}
		}
		this.finish(error ?? naviError("CANCELLED", `Connection closed: ${reason}`));
		this.channel.close();
	}

	/** Best-effort cancel frame for a request this side already settled locally. */
	private sendCancelFor(requestId: string): void {
		const connectionId = this.connectionIdValue;
		if (this.stateValue !== "ready" || connectionId === undefined) return;
		const cancelId = this.mintRequestId();
		const cancelTimer = this.clock.schedule(() => {
			this.settlePending(cancelId);
		}, this.limits.interactionTimeoutMs);
		this.pending.set(cancelId, {
			requestId: cancelId,
			op: "cancel",
			resolve: () => undefined,
			reject: () => undefined,
			cancelTimer,
		});
		this.send({ type: "request", connectionId, requestId: cancelId, payload: { op: "cancel", requestId } });
	}

	private countPendingOps(): number {
		let count = 0;
		for (const entry of this.pending.values()) {
			if (entry.op !== "cancel") count += 1;
		}
		return count;
	}

	private mintRequestId(): string {
		this.requestCounter += 1;
		return this.options.newRequestId?.() ?? `r${this.requestCounter}`;
	}

	private timeoutError(op: RequestPayload["op"]): NaviError {
		if (isEffectOp(op)) {
			return naviError("OUTCOME_UNKNOWN", "No confirmation before the deadline; the effect may have been applied");
		}
		return naviError("DEADLINE_EXCEEDED", "No answer before the deadline");
	}

	private abortError(op: RequestPayload["op"]): NaviError {
		if (isEffectOp(op)) {
			return naviError("OUTCOME_UNKNOWN", "Operation cancelled after dispatch; the effect may have been applied");
		}
		return naviError("CANCELLED", "Operation cancelled before it completed");
	}

	private send(frame: DistributiveOmit<Frame, "v" | "seq">): void {
		this.outboundSeq += 1;
		const full = { ...frame, v: NAVI_PROTOCOL_VERSION, seq: this.outboundSeq } as Frame;
		try {
			this.channel.send(full);
		} catch {
			this.fatal("BACKPRESSURE", "Outbound queue refused the frame");
		}
	}

	private handleRaw(raw: unknown): void {
		if (this.stateValue === "closed") return;
		let frame: Frame;
		try {
			frame = parseFrame(raw);
		} catch (error) {
			const code: NaviErrorCode = error instanceof ProtocolError ? error.code : "BAD_ENVELOPE";
			this.fatal(code, error instanceof Error ? error.message : "Malformed frame");
			return;
		}
		if (frame.seq !== this.lastInboundSeq + 1) {
			this.fatal("SEQUENCE_GAP", `Expected inbound seq ${this.lastInboundSeq + 1}`);
			return;
		}
		this.lastInboundSeq = frame.seq;
		if (this.stateValue === "handshaking") {
			this.handleHandshake(frame);
			return;
		}
		if (frame.type === "hello") {
			this.fatal("DUPLICATE_HELLO", "A second hello is not allowed on an established connection");
			return;
		}
		if (frame.type === "hello.ack" || frame.type === "hello.error") {
			this.fatal("BAD_ENVELOPE", `${frame.type} is only valid during the handshake`);
			return;
		}
		if (frame.connectionId !== this.connectionIdValue) {
			this.fatal("CONNECTION_MISMATCH", "Frame connection id does not match this connection");
			return;
		}
		switch (frame.type) {
			case "request":
				this.handleInboundRequest(frame);
				return;
			case "result":
				this.handleInboundResult(frame);
				return;
			case "event":
				this.options.onEvent?.(frame);
				return;
			case "bye":
				this.finish(naviError("CANCELLED", `Peer closed the connection: ${frame.reason}`));
				this.channel.close();
				return;
		}
	}

	private handleHandshake(frame: Frame): void {
		if (this.options.role === "responder") {
			if (frame.type !== "hello") {
				this.fatal("NOT_HELLO_FIRST", "First frame from the peer must be hello");
				return;
			}
			this.peerValue = frame.peer;
			this.peerCapabilitiesValue = [...frame.capabilities];
			const connectionId = this.options.newConnectionId?.() ?? defaultConnectionId();
			this.connectionIdValue = connectionId;
			this.send({
				type: "hello.ack",
				connectionId,
				peer: this.options.peer,
				capabilities: this.options.capabilities,
			});
			this.becomeReady();
			return;
		}
		if (frame.type === "hello.error") {
			this.peerValue = undefined;
			this.fatal(frame.error.code, frame.error.message);
			return;
		}
		if (frame.type !== "hello.ack") {
			this.fatal("NOT_HELLO_FIRST", "First frame from the peer must be hello.ack");
			return;
		}
		this.peerValue = frame.peer;
		this.peerCapabilitiesValue = [...frame.capabilities];
		this.connectionIdValue = frame.connectionId;
		this.becomeReady();
	}

	private becomeReady(): void {
		this.cancelHandshakeTimer();
		this.stateValue = "ready";
		const connectionId = this.connectionIdValue;
		const peer = this.peerValue;
		if (connectionId !== undefined && peer !== undefined) {
			this.options.onReady?.({ connectionId, peer, capabilities: this.peerCapabilitiesValue });
		}
		this.options.onStateChange?.("ready", undefined);
	}

	private handleInboundRequest(frame: RequestFrame): void {
		if (frame.payload.op === "cancel") {
			this.handleInboundCancel(frame);
			return;
		}
		if (this.completedInboundIds.includes(frame.requestId)) {
			this.sendResult(
				frame.requestId,
				naviError("DUPLICATE_REQUEST", "Request id was already used on this connection"),
			);
			return;
		}
		if (this.inboundOp !== undefined) {
			this.sendResult(frame.requestId, naviError("TOO_MANY_INFLIGHT", "One browser operation may be in flight"));
			return;
		}
		if (this.inboundRequestIds >= this.limits.maxRequestIdsPerConnection) {
			this.fatal("BAD_ENVELOPE", "Request id budget for this connection is exhausted; open a new connection");
			return;
		}
		this.inboundRequestIds += 1;
		const controller = new AbortController();
		const requestId = frame.requestId;
		const op = frame.payload.op;
		const cancelTimer = this.clock.schedule(
			() => {
				this.inboundOp = undefined;
				this.rememberInboundId(requestId);
				// Same rule as the requesting side: read-only work may report a deadline, while an action
				// whose confirmation was lost is an unknown outcome and is never retried automatically.
				this.sendResult(requestId, this.timeoutError(op));
			},
			op === "observe" ? this.limits.observeTimeoutMs : this.limits.interactionTimeoutMs,
		);
		this.inboundOp = { requestId, controller, cancelTimer };
		const handler = this.options.handleRequest;
		if (handler === undefined) {
			this.finishInbound(requestId, naviError("UNSUPPORTED_OP", "No handler is registered for this connection"));
			return;
		}
		handler(frame, controller.signal)
			.then((result) => this.finishInbound(requestId, result))
			.catch(() => this.finishInbound(requestId, naviError("INTERNAL", "Handler failed")));
	}

	private handleInboundCancel(frame: RequestFrame): void {
		const target = (frame.payload as CancelRequest).requestId;
		const op = this.inboundOp;
		if (op !== undefined && op.requestId === target) {
			op.cancelTimer();
			op.controller.abort();
			this.inboundOp = undefined;
			this.rememberInboundId(op.requestId);
			// The cancelled request gets no result: its requester settles it locally.
			this.sendResult(frame.requestId, { op: "cancel", cancelled: true, alreadyTerminal: false });
			return;
		}
		if (this.completedInboundIds.includes(target)) {
			this.sendResult(frame.requestId, { op: "cancel", cancelled: false, alreadyTerminal: true });
			return;
		}
		// Never seen, or evicted from the bounded id memory. This side must not claim that the request
		// already finished, so the cancel is refused instead of answered with `alreadyTerminal`.
		this.sendResult(frame.requestId, naviError("UNKNOWN_REQUEST", "No such request id on this connection"));
	}

	private finishInbound(requestId: string, result: ResultPayload | NaviError): void {
		const op = this.inboundOp;
		if (op === undefined || op.requestId !== requestId) return;
		this.inboundOp = undefined;
		op.cancelTimer();
		this.rememberInboundId(requestId);
		this.sendResult(requestId, result);
	}

	private sendResult(requestId: string, result: ResultPayload | NaviError): void {
		const connectionId = this.connectionIdValue;
		if (connectionId === undefined) return;
		if ("code" in result) {
			this.send({ type: "result", connectionId, requestId, ok: false, error: result });
			return;
		}
		this.send({ type: "result", connectionId, requestId, ok: true, payload: result });
	}

	private handleInboundResult(frame: ResultFrame): void {
		const entry = this.pending.get(frame.requestId);
		if (entry === undefined) {
			if (this.completedOutboundIds.includes(frame.requestId)) return;
			this.unknownResults += 1;
			if (this.unknownResults > this.limits.trackedRequestIds) {
				this.fatal("BAD_ENVELOPE", "Too many results for unknown requests");
			}
			return;
		}
		this.settlePending(frame.requestId);
		if (frame.ok) entry.resolve(frame.payload);
		else entry.reject(new NaviRequestError(frame.error));
	}

	private settlePending(requestId: string, error?: NaviRequestError): void {
		const entry = this.pending.get(requestId);
		if (entry === undefined) return;
		this.pending.delete(requestId);
		entry.cancelTimer();
		if (entry.signal !== undefined && entry.onAbort !== undefined) {
			entry.signal.removeEventListener("abort", entry.onAbort);
		}
		this.rememberOutboundId(requestId);
		if (error !== undefined) entry.reject(error);
	}

	private rememberInboundId(requestId: string): void {
		this.completedInboundIds.push(requestId);
		if (this.completedInboundIds.length > this.limits.trackedRequestIds) this.completedInboundIds.shift();
	}

	private rememberOutboundId(requestId: string): void {
		this.completedOutboundIds.push(requestId);
		if (this.completedOutboundIds.length > this.limits.trackedRequestIds) this.completedOutboundIds.shift();
	}

	private cancelHandshakeTimer(): void {
		if (this.handshakeTimer === undefined) return;
		this.handshakeTimer();
		this.handshakeTimer = undefined;
	}

	private handleChannelClose(reason: ChannelCloseReason): void {
		if (this.stateValue === "closed") return;
		this.finish(naviError("CANCELLED", `Link closed (${reason}) before the connection finished`));
	}

	private fatal(code: NaviErrorCode, message: string): void {
		if (this.stateValue === "closed") return;
		const error = naviError(toWireErrorCode(code), message);
		this.lastErrorCodeValue = code;
		this.sendFatalNotice(error);
		this.finish(error);
		this.channel.close();
	}

	/** Best effort: the peer may still receive one terminal notice before the link drops. */
	private sendFatalNotice(error: NaviError): void {
		try {
			if (this.stateValue === "handshaking" && this.options.role === "responder") {
				this.send({ type: "hello.error", error });
				return;
			}
			const connectionId = this.connectionIdValue;
			if (connectionId !== undefined) this.send({ type: "bye", connectionId, reason: "protocol-error" });
		} catch {
			// The notice is optional; the close below is not.
		}
	}

	private finish(error: NaviError): void {
		if (this.stateValue === "closed") return;
		this.cancelHandshakeTimer();
		this.stateValue = "closed";
		this.lastErrorValue = this.lastErrorValue ?? error;
		const inbound = this.inboundOp;
		if (inbound !== undefined) {
			inbound.cancelTimer();
			inbound.controller.abort();
			this.inboundOp = undefined;
		}
		const entries = [...this.pending.values()];
		this.pending.clear();
		for (const entry of entries) {
			entry.cancelTimer();
			if (entry.signal !== undefined && entry.onAbort !== undefined) {
				entry.signal.removeEventListener("abort", entry.onAbort);
			}
			entry.reject(new NaviRequestError(this.disconnectError(entry.op)));
		}
		this.options.onStateChange?.("closed", this.lastErrorValue);
	}

	private disconnectError(op: RequestPayload["op"]): NaviError {
		if (isEffectOp(op)) {
			return naviError("OUTCOME_UNKNOWN", "The link closed after dispatch; the effect may have been applied");
		}
		return naviError("CANCELLED", "The link closed before the answer arrived");
	}
}
