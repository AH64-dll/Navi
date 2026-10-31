/**
 * Browser-neutral seams for NAVI core.
 *
 * `core/` has no Node, Chrome or DOM imports: it runs unchanged in the CLI process and in the
 * extension service worker. Transport (Node) and the worker facade (Chrome) implement `FrameChannel`
 * and inject their own clock and id factories.
 */
import type { Capability, Frame, Grant, NaviError, Observation, PeerInfo, WireErrorCode } from "../protocol/types.ts";

/** Why the underlying link ended. `error` means the link failed, not that the peer said goodbye. */
export type ChannelCloseReason = "peer-closed" | "local-closed" | "error";

/**
 * One duplex link carrying JSON object frames.
 *
 * Contract: frames are JSON objects, never a JSON string containing serialized JSON. The receiver
 * calls `parseFrame`, so `onMessage` may deliver any untrusted value. `send` may throw when a bounded
 * queue is full; the connection then closes with `BACKPRESSURE`.
 */
export interface FrameChannel {
	send(frame: Frame): void;
	onMessage(handler: (raw: unknown) => void): void;
	onClose(handler: (reason: ChannelCloseReason) => void): void;
	/** Idempotent. Transport decides whether it must drain queued output first. */
	close(): void;
}

/** Injected time source. Tests use a fake clock; nothing in core calls `Date.now` directly. */
export interface Clock {
	now(): number;
	/** Runs `callback` after `delayMs`. The returned function cancels the timer. */
	schedule(callback: () => void, delayMs: number): () => void;
}

/**
 * Monotonic milliseconds for deadlines and grant TTLs. `performance.now()` cannot jump with a system
 * clock change; a wall-clock timestamp (for example `Observation.capturedAt`) is a separate value that
 * the worker or transport supplies.
 */
function monotonicNow(): number {
	const performance = globalThis.performance;
	if (performance !== undefined && typeof performance.now === "function") return performance.now();
	return Date.now();
}

/** Process-local clock. `setTimeout` is a global in both runtimes, not a Node import. */
export const systemClock: Clock = {
	now: () => monotonicNow(),
	schedule: (callback, delayMs) => {
		const handle = setTimeout(callback, delayMs);
		return () => clearTimeout(handle);
	},
};

/** A request failed. `code` is a closed `NaviErrorCode`; `retryable` is always false for browser work. */
export class NaviRequestError extends Error {
	readonly code: WireErrorCode;
	readonly retryable: boolean;

	constructor(error: NaviError) {
		super(error.message);
		this.name = "NaviRequestError";
		this.code = error.code;
		this.retryable = error.retryable;
	}
}

/** Connection state as seen by one peer. There is no automatic reconnect. */
export type LinkState = "handshaking" | "ready" | "closed";

export type BrowserPortStatus = {
	link: LinkState;
	connectionId?: string;
	peer?: PeerInfo;
	/** Capability set advertised by the peer during the handshake. */
	peerCapabilities: Capability[];
	/** Grant the CLI currently believes is attached. The worker remains the authority. */
	grant: Grant | null;
	/** Latest observation id that is still valid for `grant`, if any. */
	observationId?: string;
	inFlightRequestId?: string;
	lastError?: NaviError;
};

export type ScrollArgument = { ref?: string; align?: "start" | "center" | "end"; deltaY?: number; deltaX?: number };

/**
 * The seam the CLI harness consumes. A later model adapter uses the same port; it must never pass
 * grant fields taken from model output.
 */
export interface BrowserPort {
	status(): BrowserPortStatus;
	onStatus(listener: (status: BrowserPortStatus) => void): () => void;
	/** Observe the granted document. Needs the `observe` capability. */
	observe(options?: { signal?: AbortSignal }): Promise<Observation>;
	/** Focus one node from the current observation. Needs the `focus` capability. */
	focus(ref: string, options?: { signal?: AbortSignal }): Promise<"applied" | "unknown">;
	/** Scroll the current observation, or the top frame when `ref` is absent. Needs the `scroll` capability. */
	scroll(
		argument: ScrollArgument,
		options?: { signal?: AbortSignal },
	): Promise<{ outcome: "applied" | "unknown"; scrollX?: number; scrollY?: number }>;
	/** Ask the peer to stop the in-flight operation. A page effect may still have been dispatched. */
	cancel(options?: { signal?: AbortSignal }): Promise<{ cancelled: boolean; alreadyTerminal: boolean }>;
	/** Send `bye`, close the link and settle pending work as incomplete. */
	disconnect(reason?: "closed" | "detached"): Promise<void>;
}
