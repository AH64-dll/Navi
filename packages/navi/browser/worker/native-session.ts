/**
 * Native channel and connection session for the browser side.
 *
 * The `Connection` from `src/core` owns the handshake, per-direction sequence, request correlation,
 * deadlines and cancellation, so the worker does not duplicate that state machine. This module only
 * adapts the Chrome native port to the `FrameChannel` seam and reports identity facts the worker needs.
 *
 * Chrome passes JSON objects on this hop. A frame is never stringified here, so the native process sees
 * the envelope once instead of a string containing JSON.
 */
import { Connection, type ConnectionState, type ReadyInfo, type RequestHandler } from "../../src/core/connection.ts";
import { naviError } from "../../src/core/errors.ts";
import type { ChannelCloseReason, Clock, FrameChannel } from "../../src/core/ports.ts";
import type { ByeReason, Capability, EventFrame, NaviError } from "../../src/protocol/types.ts";
import type { NativePortApi, Platform } from "../platform/types.ts";

/** Native host name registered with the browser. Must match the generated host manifest. */
export const NATIVE_HOST_NAME = "dev.navi.bridge";

/** Version reported in `hello`. Bounded by the protocol's `MAX_VERSION_CHARS`. */
export const EXTENSION_VERSION = "0.0.1";

/** Frames buffered if the native host answers before the connection registered its handler. */
const EARLY_MESSAGE_BUFFER = 4;

/** Adapts one native messaging port to the browser-neutral frame channel. */
export function createNativeChannel(port: NativePortApi): FrameChannel {
	let closed = false;
	let handler: ((raw: unknown) => void) | null = null;
	let closeReason: ChannelCloseReason | null = null;
	const early: unknown[] = [];
	const closeHandlers: Array<(reason: ChannelCloseReason) => void> = [];

	port.onMessage.addListener((value) => {
		if (handler) handler(value);
		else if (early.length < EARLY_MESSAGE_BUFFER) early.push(value);
	});
	port.onDisconnect.addListener(() => {
		if (closed) return;
		closed = true;
		const reason: ChannelCloseReason = "peer-closed";
		closeReason = reason;
		for (const closeHandler of closeHandlers) closeHandler(reason);
	});

	return {
		send(frame) {
			if (closed) throw new Error("native channel is closed");
			port.postMessage(frame);
		},
		onMessage(next) {
			handler = next;
			const queued = early.splice(0, early.length);
			for (const value of queued) next(value);
		},
		onClose(next) {
			closeHandlers.push(next);
			if (closeReason !== null) next(closeReason);
		},
		close() {
			if (closed) return;
			closed = true;
			try {
				port.disconnect();
			} catch {
				// Already gone.
			}
		},
	};
}

export type NativeSession = {
	readonly connection: Connection;
	close(reason: ByeReason): void;
};

export type SessionEvents = {
	onReady(info: ReadyInfo): void;
	onStateChange(state: ConnectionState, error?: NaviError): void;
	onEvent(event: EventFrame): void;
	handleRequest: RequestHandler;
};

/**
 * Opens the native port and starts the handshake. Returns an error instead of throwing so the caller can
 * report a setup failure instead of a stack trace.
 */
export function openNativeSession(input: {
	platform: Platform;
	clock: Clock;
	capabilities: Capability[];
	newRequestId: () => string;
	events: SessionEvents;
}): NativeSession | NaviError {
	let port: NativePortApi;
	try {
		port = input.platform.runtime.connectNative(NATIVE_HOST_NAME);
	} catch {
		return naviError("NOT_ATTACHED", "NAVI connector is not registered or the CLI is not running");
	}
	const channel = createNativeChannel(port);
	const connection = new Connection({
		role: "initiator",
		channel,
		clock: input.clock,
		peer: { kind: "extension", version: EXTENSION_VERSION },
		capabilities: input.capabilities,
		newRequestId: input.newRequestId,
		handleRequest: input.events.handleRequest,
		onReady: input.events.onReady,
		onStateChange: input.events.onStateChange,
		onEvent: input.events.onEvent,
	});
	connection.open();
	return {
		connection,
		close(reason) {
			connection.close(reason);
		},
	};
}
