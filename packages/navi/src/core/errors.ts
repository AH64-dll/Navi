/** Small helpers for bounded `NaviError` values. A wire error never carries a transport-local code. */
import type { NaviError, NaviErrorCode, TransportErrorCode, WireErrorCode } from "../protocol/types.ts";

/**
 * Maps any code this side may hold to a code that may travel in a `NaviError`. Transport-local codes
 * describe a broken frame, so they become `BAD_ENVELOPE` for the peer and for the CLI display.
 */
export function toWireErrorCode(code: NaviErrorCode): WireErrorCode {
	const transportOnly: readonly TransportErrorCode[] = [
		"BAD_FRAME_LENGTH",
		"FRAME_TOO_LARGE",
		"TRUNCATED_FRAME",
		"BAD_UTF8",
		"BAD_JSON",
		"NOT_JSON_OBJECT",
		"ENVELOPE_TOO_LARGE",
	];
	return transportOnly.includes(code as TransportErrorCode) ? "BAD_ENVELOPE" : (code as WireErrorCode);
}

/** Codes that end the connection. A request-scoped code never closes the link. */
const FATAL_CODES: readonly NaviErrorCode[] = [
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
];

export function isFatalErrorCode(code: NaviErrorCode): boolean {
	return FATAL_CODES.includes(code);
}

/** Builds a bounded, redacted error. Messages never carry page text, node text or credentials. */
export function naviError(code: WireErrorCode, message: string): NaviError {
	const bounded = message.length > 160 ? `${message.slice(0, 157)}...` : message;
	return { code, message: bounded, retryable: false };
}
