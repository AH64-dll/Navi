/**
 * Frame text codec.
 *
 * The wire unit is one UTF-8 JSON object string. Byte transports add their own length prefix and
 * enforce the budget before allocation; `encodeFrame` and `decodeFrame` are the browser-safe halves
 * that both peers share.
 *
 * Browser-safe: no Node APIs, no side effects.
 */
import { MAX_ENVELOPE_BYTES } from "./limits.ts";
import type { Frame } from "./types.ts";
import { ProtocolError, parseFrame, utf8TextByteLength } from "./validate.ts";

/** Serializes one frame as a single JSON object string, after validating it. */
export function encodeFrame(frame: Frame): string {
	const validated = parseFrame(frame);
	const text = JSON.stringify(validated);
	if (utf8TextByteLength(text) > MAX_ENVELOPE_BYTES) {
		throw new ProtocolError("ENVELOPE_TOO_LARGE", "", `Frame exceeds ${MAX_ENVELOPE_BYTES} bytes`);
	}
	return text;
}

/** Parses one JSON object string into a validated frame. */
export function decodeFrame(text: string): Frame {
	if (typeof text !== "string") throw new ProtocolError("BAD_JSON", "", "Expected frame text");
	let value: unknown;
	try {
		value = JSON.parse(text);
	} catch {
		throw new ProtocolError("BAD_JSON", "", "Frame text is not valid JSON");
	}
	return parseFrame(value);
}

/** UTF-8 byte length of a frame as it would be sent, without serializing it twice. */
export function frameByteLength(frame: Frame): number {
	return utf8TextByteLength(encodeFrame(frame));
}
