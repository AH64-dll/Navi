/**
 * Native-messaging and local-socket framing for the NAVI bridge.
 *
 * One frame is a 4-byte unsigned length in native byte order followed by exactly that many UTF-8 bytes
 * containing one JSON object. The browser stdio hop and the local POSIX socket hop use the same framing:
 * both ends of each hop run on the same machine, so native byte order is unambiguous.
 *
 * Allocated size is bounded by `MAX_ENVELOPE_BYTES` before a body buffer is created, and a decoder that
 * fails never resynchronises. Node-only: the browser worker receives parsed objects from its message port.
 */
import { endianness } from "node:os";
import { MAX_ENVELOPE_BYTES } from "../protocol/limits.ts";
import type { NaviErrorCode } from "../protocol/types.ts";

export type ByteOrder = "LE" | "BE";

const HEADER_BYTES = 4;

/** Transport-local failure. `code` is one of the frozen transport codes and is never sent as a frame. */
export class TransportError extends Error {
	readonly code: NaviErrorCode;

	constructor(code: NaviErrorCode, message: string) {
		super(message);
		this.name = "TransportError";
		this.code = code;
	}
}

function describe(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function isJsonObject(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Native byte order of this host. Both hops are machine-local, so both peers agree on it. */
export function nativeByteOrder(): ByteOrder {
	return endianness();
}

/** Serialises one JSON object into a length-prefixed frame. Empty or oversized payloads are rejected. */
export function encodeFrame(value: unknown, byteOrder: ByteOrder = endianness()): Uint8Array {
	if (!isJsonObject(value)) throw new TransportError("NOT_JSON_OBJECT", "A frame must be a JSON object");

	let json: string;
	try {
		json = JSON.stringify(value);
	} catch (error) {
		throw new TransportError("BAD_JSON", `Frame is not serialisable: ${describe(error)}`);
	}
	if (json === undefined) throw new TransportError("BAD_JSON", "Frame serialised to nothing");

	const body = Buffer.from(json, "utf8");
	if (body.byteLength > MAX_ENVELOPE_BYTES) {
		throw new TransportError(
			"ENVELOPE_TOO_LARGE",
			`Frame body is ${body.byteLength} bytes, above the ${MAX_ENVELOPE_BYTES} byte cap`,
		);
	}

	const frame = Buffer.allocUnsafe(HEADER_BYTES + body.byteLength);
	if (byteOrder === "LE") frame.writeUInt32LE(body.byteLength, 0);
	else frame.writeUInt32BE(body.byteLength, 0);
	body.copy(frame, HEADER_BYTES);
	return frame;
}

/**
 * Incremental reader for length-prefixed frames.
 *
 * `push` returns every complete frame in a chunk and tolerates arbitrary fragmentation, coalescing and
 * one-byte chunks. Any framing, UTF-8 or JSON failure closes the decoder permanently: the caller must
 * disconnect instead of resynchronising on the byte stream.
 */
export class FrameDecoder {
	private readonly byteOrder: ByteOrder;
	private readonly header = Buffer.alloc(HEADER_BYTES);
	private headerBytes = 0;
	private body: Buffer | null = null;
	private bodyBytes = 0;
	private failure: TransportError | null = null;
	private finished = false;

	constructor(byteOrder: ByteOrder = endianness()) {
		this.byteOrder = byteOrder;
	}

	/** Bytes buffered for the current partial frame. Zero when the decoder is aligned on a header. */
	get pendingBytes(): number {
		return this.headerBytes + this.bodyBytes;
	}

	get failed(): boolean {
		return this.failure !== null;
	}

	push(chunk: Uint8Array): unknown[] {
		if (this.failure) throw this.failure;
		if (this.finished) throw new Error("Frame decoder is already finished");
		if (!(chunk instanceof Uint8Array)) throw new TypeError("Frame chunk must be a Uint8Array");

		const frames: unknown[] = [];
		let offset = 0;
		try {
			while (offset < chunk.byteLength) {
				if (this.body === null) {
					const take = Math.min(HEADER_BYTES - this.headerBytes, chunk.byteLength - offset);
					for (let index = 0; index < take; index += 1) {
						this.header[this.headerBytes + index] = chunk[offset + index];
					}
					offset += take;
					this.headerBytes += take;
					if (this.headerBytes < HEADER_BYTES) continue;

					const length = this.byteOrder === "LE" ? this.header.readUInt32LE(0) : this.header.readUInt32BE(0);
					this.headerBytes = 0;
					if (length === 0) throw new TransportError("BAD_FRAME_LENGTH", "Frame length must be positive");
					if (length > MAX_ENVELOPE_BYTES) {
						throw new TransportError(
							"FRAME_TOO_LARGE",
							`Declared frame length ${length} exceeds the ${MAX_ENVELOPE_BYTES} byte cap`,
						);
					}
					this.body = Buffer.allocUnsafe(length);
					this.bodyBytes = 0;
				}

				const take = Math.min(this.body.byteLength - this.bodyBytes, chunk.byteLength - offset);
				this.body.set(chunk.subarray(offset, offset + take), this.bodyBytes);
				offset += take;
				this.bodyBytes += take;
				if (this.bodyBytes < this.body.byteLength) continue;

				const body = this.body;
				this.body = null;
				this.bodyBytes = 0;
				frames.push(decodeBody(body));
			}
			return frames;
		} catch (error) {
			const failure =
				error instanceof TransportError
					? error
					: new TransportError("BAD_JSON", `Frame decoding failed: ${describe(error)}`);
			this.failure = failure;
			this.body = null;
			this.bodyBytes = 0;
			this.headerBytes = 0;
			throw failure;
		}
	}

	/** Declares end of input. A partial header or body at EOF is a protocol error, not a short frame. */
	finish(): void {
		if (this.failure) throw this.failure;
		if (this.finished) throw new Error("Frame decoder is already finished");
		this.finished = true;
		const missing = this.body ? this.body.byteLength - this.bodyBytes : 0;
		if (this.pendingBytes !== 0 || missing !== 0) {
			throw new TransportError(
				"TRUNCATED_FRAME",
				`Input ended with ${this.pendingBytes + missing} bytes of a partial frame`,
			);
		}
	}
}

/** Strict UTF-8 plus JSON-object check for one frame body. */
function decodeBody(body: Buffer): unknown {
	let json: string;
	try {
		json = new TextDecoder("utf-8", { fatal: true }).decode(body);
	} catch (error) {
		throw new TransportError("BAD_UTF8", `Frame body is not valid UTF-8: ${describe(error)}`);
	}

	let value: unknown;
	try {
		value = JSON.parse(json);
	} catch (error) {
		throw new TransportError("BAD_JSON", `Frame body is not JSON: ${describe(error)}`);
	}
	if (!isJsonObject(value)) throw new TransportError("NOT_JSON_OBJECT", "A frame must be a JSON object");
	return value;
}
