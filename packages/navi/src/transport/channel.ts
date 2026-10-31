/**
 * Bounded frame channel over an ordered byte stream (browser stdio or the local socket).
 *
 * Reads are decoded with `FrameDecoder`; writes are queued and bounded by `MAX_QUEUE_FRAMES`,
 * `MAX_QUEUE_BYTES` and `DRAIN_GRACE_MS`. There is no reconnect, no replay and no unbounded buffer:
 * overflow, a drain stall, a decode failure or a premature stream end closes the channel permanently
 * and reports exactly one `onClose`.
 */
import type { Duplex } from "node:stream";
import { DRAIN_GRACE_MS, MAX_QUEUE_BYTES, MAX_QUEUE_FRAMES } from "../protocol/limits.ts";
import { type ByteOrder, encodeFrame, FrameDecoder, nativeByteOrder, TransportError } from "./framing.ts";

export type FrameChannelHandlers = {
	onFrame(frame: unknown): void;
	/** Exactly one terminal callback per channel. `error` is null only for an orderly close. */
	onClose(error: TransportError | null): void;
};

export type FrameChannelOptions = {
	byteOrder?: ByteOrder;
	maxQueueFrames?: number;
	maxQueueBytes?: number;
	drainGraceMs?: number;
};

export interface FrameChannel {
	readonly closed: boolean;
	/** Frames still waiting for the stream to accept them. */
	readonly pendingFrames: number;
	send(frame: unknown): void;
	/** Orderly close flushes queued frames within the drain grace; an error closes immediately. */
	close(error?: TransportError): void;
}

function assertPositiveInteger(value: number, name: string): number {
	if (!Number.isSafeInteger(value) || value <= 0) throw new RangeError(`${name} must be a positive integer`);
	return value;
}

class BoundedFrameChannel implements FrameChannel {
	private readonly stream: Duplex;
	private readonly handlers: FrameChannelHandlers;
	private readonly decoder: FrameDecoder;
	private readonly byteOrder: ByteOrder;
	private readonly maxQueueFrames: number;
	private readonly maxQueueBytes: number;
	private readonly drainGraceMs: number;
	private readonly swallow = (): void => {};
	private readonly onData: (chunk: Uint8Array) => void;
	private readonly onEnd: () => void;
	private readonly onStreamClose: () => void;
	private readonly onError: (error: Error) => void;
	private readonly onDrain: () => void;
	private queue: Uint8Array[] = [];
	private queuedBytes = 0;
	private awaitingDrain = false;
	private closing = false;
	private failing = false;
	private settled = false;
	private drainTimer: NodeJS.Timeout | null = null;
	private closeTimer: NodeJS.Timeout | null = null;

	constructor(stream: Duplex, handlers: FrameChannelHandlers, options: FrameChannelOptions) {
		this.stream = stream;
		this.handlers = handlers;
		this.byteOrder = options.byteOrder ?? nativeByteOrder();
		this.decoder = new FrameDecoder(this.byteOrder);
		this.maxQueueFrames = assertPositiveInteger(options.maxQueueFrames ?? MAX_QUEUE_FRAMES, "maxQueueFrames");
		this.maxQueueBytes = assertPositiveInteger(options.maxQueueBytes ?? MAX_QUEUE_BYTES, "maxQueueBytes");
		this.drainGraceMs = assertPositiveInteger(options.drainGraceMs ?? DRAIN_GRACE_MS, "drainGraceMs");

		this.onData = (chunk) => this.handleData(chunk);
		this.onEnd = () => this.handleEnd();
		this.onStreamClose = () => this.settle(null);
		this.onError = (error) => this.fail(new TransportError("TRUNCATED_FRAME", `Stream error: ${error.message}`));
		this.onDrain = () => this.handleDrain();

		stream.on("data", this.onData);
		stream.on("end", this.onEnd);
		stream.on("close", this.onStreamClose);
		stream.on("error", this.onError);
		stream.on("drain", this.onDrain);
	}

	get closed(): boolean {
		return this.settled;
	}

	get pendingFrames(): number {
		return this.queue.length;
	}

	send(frame: unknown): void {
		if (this.settled) throw new Error("Frame channel is closed");
		let chunk: Uint8Array;
		try {
			chunk = encodeFrame(frame, this.byteOrder);
		} catch (error) {
			this.fail(error instanceof TransportError ? error : new TransportError("BAD_JSON", String(error)));
			throw error;
		}
		if (this.queue.length + 1 > this.maxQueueFrames || this.queuedBytes + chunk.byteLength > this.maxQueueBytes) {
			const error = new TransportError(
				"BACKPRESSURE",
				`Outbound queue exceeds ${this.maxQueueFrames} frames or ${this.maxQueueBytes} bytes`,
			);
			this.fail(error);
			throw error;
		}
		this.queue.push(chunk);
		this.queuedBytes += chunk.byteLength;
		this.pump();
	}

	close(error?: TransportError): void {
		if (this.settled) return;
		if (error) {
			this.fail(error);
			return;
		}
		this.closing = true;
		if (this.queue.length === 0 && !this.awaitingDrain) this.endStream();
		else this.armDrainTimer();
	}

	private handleData(chunk: Uint8Array): void {
		if (this.settled) return;
		let frames: unknown[];
		try {
			frames = this.decoder.push(chunk);
		} catch (error) {
			this.fail(error instanceof TransportError ? error : new TransportError("BAD_JSON", String(error)));
			return;
		}
		for (const frame of frames) {
			if (this.settled) return;
			try {
				this.handlers.onFrame(frame);
			} catch (error) {
				// A handler failure is bounded like a decode failure: close once instead of throwing into the stream.
				this.fail(
					error instanceof TransportError
						? error
						: new TransportError("BAD_JSON", `Frame handler failed: ${String(error)}`),
				);
				return;
			}
		}
	}

	private handleEnd(): void {
		if (this.settled) return;
		try {
			this.decoder.finish();
		} catch (error) {
			this.fail(error instanceof TransportError ? error : new TransportError("TRUNCATED_FRAME", String(error)));
			return;
		}
		this.closing = true;
		if (this.queue.length === 0 && !this.awaitingDrain) this.endStream();
		else this.armDrainTimer();
	}

	private handleDrain(): void {
		if (this.settled) return;
		this.clearTimers();
		this.awaitingDrain = false;
		this.pump();
	}

	private pump(): void {
		if (this.settled || this.awaitingDrain) return;
		while (this.queue.length > 0) {
			const chunk = this.queue.shift();
			if (!chunk) return;
			this.queuedBytes -= chunk.byteLength;
			let accepted: boolean;
			try {
				accepted = this.stream.write(chunk);
			} catch (error) {
				this.fail(new TransportError("TRUNCATED_FRAME", `Stream write failed: ${String(error)}`));
				return;
			}
			if (!accepted) {
				this.awaitingDrain = true;
				this.armDrainTimer();
				return;
			}
		}
		if (this.closing) this.endStream();
	}

	private endStream(): void {
		if (this.settled) return;
		this.closing = true;
		this.clearTimers();
		try {
			this.stream.end();
		} catch (error) {
			this.fail(new TransportError("TRUNCATED_FRAME", `Stream end failed: ${String(error)}`));
			return;
		}
		this.armCloseTimer();
	}

	private armDrainTimer(): void {
		if (this.drainTimer) return;
		this.drainTimer = setTimeout(() => {
			this.drainTimer = null;
			this.fail(new TransportError("BACKPRESSURE", `Writable did not drain within ${this.drainGraceMs} ms`));
		}, this.drainGraceMs);
		this.drainTimer.unref();
	}

	private armCloseTimer(): void {
		if (this.closeTimer) return;
		this.closeTimer = setTimeout(() => {
			this.closeTimer = null;
			if (this.settled) return;
			this.stream.destroy();
			this.settle(null);
		}, this.drainGraceMs);
		this.closeTimer.unref();
	}

	private clearTimers(): void {
		if (this.drainTimer) clearTimeout(this.drainTimer);
		if (this.closeTimer) clearTimeout(this.closeTimer);
		this.drainTimer = null;
		this.closeTimer = null;
	}

	private fail(error: TransportError): void {
		if (this.settled || this.failing) return;
		this.failing = true;
		this.stream.on("error", this.swallow);
		this.stream.destroy();
		this.settle(error);
	}

	private settle(error: TransportError | null): void {
		if (this.settled) return;
		this.settled = true;
		this.clearTimers();
		this.queue = [];
		this.queuedBytes = 0;
		this.stream.removeListener("data", this.onData);
		this.stream.removeListener("end", this.onEnd);
		this.stream.removeListener("close", this.onStreamClose);
		this.stream.removeListener("drain", this.onDrain);
		this.handlers.onClose(error);
	}
}

/** Wraps a duplex byte stream in a bounded frame channel. Callers own the stream's lifetime. */
export function createFrameChannel(
	stream: Duplex,
	handlers: FrameChannelHandlers,
	options: FrameChannelOptions = {},
): FrameChannel {
	if (typeof stream.write !== "function" || typeof stream.on !== "function") {
		throw new TypeError("createFrameChannel requires a duplex byte stream");
	}
	return new BoundedFrameChannel(stream, handlers, options);
}
