/**
 * Browser-launched native-messaging connector.
 *
 * Chrome starts this process when the extension calls `connectNative("dev.navi.bridge")` and passes the
 * caller origin as its first argument. The connector validates that origin against the pinned extension ID,
 * attaches to the already-running NAVI CLI socket and relays frames between the two byte streams.
 *
 * The relay decodes and re-frames every message with the shared strict framing, keeps no session state and
 * never starts another agent. Frames that arrive from the browser before the CLI connection exists are
 * buffered up to `MAX_QUEUE_FRAMES` and then replayed in order; nothing is silently dropped.
 * Protocol bytes go to stdout only, diagnostics to stderr.
 */
import { realpathSync } from "node:fs";
import { Duplex, type Readable, type Writable } from "node:stream";
import { fileURLToPath } from "node:url";
import { MAX_QUEUE_FRAMES } from "../protocol/limits.ts";
import { createFrameChannel, type FrameChannel } from "./channel.ts";
import { TransportError } from "./framing.ts";
import { type BridgeConnection, connectBridgeSocket } from "./socket.ts";

export const EXIT_OK = 0;
export const EXIT_USAGE = 2;
export const EXIT_NO_CLI = 3;
export const EXIT_TRANSPORT = 4;

const CHROME_ORIGIN_PREFIX = "chrome-extension://";

export type ConnectorArgs = {
	socketDir: string;
	extensionId: string;
	callerOrigin: string;
};

export type ConnectorStreams = {
	/** Protocol stdin/stdout of the native host, paired as one duplex. */
	stdio: Duplex;
	/** Diagnostics. Never carries protocol bytes. */
	stderr: { write(chunk: string): unknown };
};

/** Chrome extension IDs are 32 characters in the a-p alphabet. */
function isExtensionId(value: string): boolean {
	return /^[a-p]{32}$/.test(value);
}

/**
 * Validates `chrome-extension://<id>/`, trailing slash optional. This checks the origin Chrome reported for
 * the caller. It is not proof of the caller's identity: a same-account process can pass any argument.
 */
export function isAllowedCallerOrigin(origin: string, extensionId: string): boolean {
	const normalized = origin.trim().replace(/\/+$/, "");
	return normalized.toLowerCase() === `${CHROME_ORIGIN_PREFIX}${extensionId}`.toLowerCase();
}

/** Parses connector arguments plus the caller origin and `--parent-window` argument Chrome adds. */
export function parseConnectorArgs(argv: string[]): ConnectorArgs | { error: string } {
	let socketDir: string | undefined;
	let extensionId: string | undefined;
	let callerOrigin: string | undefined;

	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		if (arg === "--socket-dir" || arg === "--extension-id") {
			const value = argv[index + 1];
			if (value === undefined || value.startsWith("--")) return { error: `${arg} needs a value` };
			if (arg === "--socket-dir") socketDir = value;
			else extensionId = value;
			index += 1;
			continue;
		}
		if (arg.startsWith("--socket-dir=")) {
			socketDir = arg.slice("--socket-dir=".length);
			continue;
		}
		if (arg.startsWith("--extension-id=")) {
			extensionId = arg.slice("--extension-id=".length);
			continue;
		}
		if (arg.startsWith("--parent-window=")) continue;
		if (arg.startsWith(CHROME_ORIGIN_PREFIX)) {
			if (callerOrigin !== undefined) return { error: "Multiple caller origins were passed" };
			callerOrigin = arg;
			continue;
		}
		return { error: `Unexpected argument: ${arg}` };
	}

	if (!socketDir) return { error: "--socket-dir is required" };
	if (!extensionId) return { error: "--extension-id is required" };
	if (!isExtensionId(extensionId)) return { error: `--extension-id must be 32 characters in a-p: ${extensionId}` };
	if (!callerOrigin) return { error: "Chrome caller origin argument is missing" };
	return { socketDir, extensionId, callerOrigin };
}

/**
 * Pairs the host's stdin and stdout into one duplex of protocol bytes.
 *
 * Input reading starts only when the consumer pulls, pauses while the readable side is full (bounded native
 * stdin) and resumes on demand. Closing the stream detaches every listener and pauses stdin, so the host
 * exits instead of holding the pipe open.
 */
export function createStdioStream(input: Readable = process.stdin, output: Writable = process.stdout): Duplex {
	input.pause();
	const stream = new Duplex({
		read(): void {
			attach();
			input.resume();
		},
		write(chunk: Uint8Array, _encoding: string, callback: (error?: Error | null) => void): void {
			output.write(chunk, (error) => callback(error ?? null));
		},
		final(callback: (error?: Error | null) => void): void {
			callback();
		},
	});
	let attached = false;
	const onData = (chunk: Buffer): void => {
		if (!stream.push(chunk)) input.pause();
	};
	const onEnd = (): void => {
		stream.push(null);
	};
	const onInputError = (error: Error): void => {
		stream.destroy(error);
	};
	const attach = (): void => {
		if (attached) return;
		attached = true;
		input.on("data", onData);
		input.on("end", onEnd);
		input.on("error", onInputError);
	};
	const detach = (): void => {
		input.off("data", onData);
		input.off("end", onEnd);
		input.off("error", onInputError);
		input.pause();
	};
	stream.on("close", detach);
	return stream;
}

/**
 * Runs the relay and resolves with the process exit code.
 *
 * `0` clean disconnect (either side ended), `2` usage or origin failure, `3` no CLI listening,
 * `4` transport failure after the connection was established.
 */
export async function runConnector(argv: string[], streams: ConnectorStreams): Promise<number> {
	const parsed = parseConnectorArgs(argv);
	if ("error" in parsed) {
		streams.stderr.write(`navi-connector: ${parsed.error}\n`);
		return EXIT_USAGE;
	}
	if (!isAllowedCallerOrigin(parsed.callerOrigin, parsed.extensionId)) {
		streams.stderr.write(
			`navi-connector: caller origin ${parsed.callerOrigin} is not chrome-extension://${parsed.extensionId}/\n`,
		);
		return EXIT_USAGE;
	}

	const holder: { cli?: BridgeConnection } = {};
	const pending: unknown[] = [];
	let finished = false;
	let resolveDone: (code: number) => void = () => {};
	const done = new Promise<number>((resolve) => {
		resolveDone = resolve;
	});

	const settle = (code: number, error: TransportError | null): void => {
		if (finished) return;
		finished = true;
		if (error) streams.stderr.write(`navi-connector: relay closed: ${error.code} ${error.message}\n`);
		stdio.close(error ?? undefined);
		holder.cli?.close(error ?? undefined);
		resolveDone(code);
	};

	const asTransportError = (error: unknown): TransportError | null => (error instanceof TransportError ? error : null);

	/** Sends through a channel, converting a bounded-queue or encode failure into a terminal close. */
	const forward = (channel: FrameChannel, frame: unknown): void => {
		try {
			channel.send(frame);
		} catch (error) {
			settle(EXIT_TRANSPORT, asTransportError(error));
		}
	};

	const stdio: FrameChannel = createFrameChannel(streams.stdio, {
		onFrame: (frame) => {
			const cli = holder.cli;
			if (cli) {
				forward(cli.channel, frame);
				return;
			}
			// Chrome may send `hello` immediately; keep it bounded and replay it in order after connecting.
			if (pending.length >= MAX_QUEUE_FRAMES) {
				settle(
					EXIT_TRANSPORT,
					new TransportError(
						"BACKPRESSURE",
						`CLI connection is not ready after ${MAX_QUEUE_FRAMES} buffered frames`,
					),
				);
				return;
			}
			pending.push(frame);
		},
		onClose: (error) => settle(error ? EXIT_TRANSPORT : EXIT_OK, error),
	});

	let cli: BridgeConnection;
	try {
		cli = await connectBridgeSocket({
			socketDir: parsed.socketDir,
			onFrame: (frame) => forward(stdio, frame),
			onClose: (_connection, error) => settle(error ? EXIT_TRANSPORT : EXIT_OK, error),
			onError: (error) => streams.stderr.write(`navi-connector: socket error: ${error.message}\n`),
		});
	} catch (error) {
		stdio.close();
		streams.stderr.write(
			`navi-connector: ${error instanceof Error ? error.message : String(error)}. ` +
				"Start the NAVI CLI (navi serve) and attach again.\n",
		);
		return EXIT_NO_CLI;
	}
	holder.cli = cli;
	for (const frame of pending.splice(0)) {
		if (finished) break;
		forward(cli.channel, frame);
	}
	return done;
}

function isMainModule(): boolean {
	const entry = process.argv[1];
	if (!entry) return false;
	try {
		return realpathSync(entry) === realpathSync(fileURLToPath(import.meta.url));
	} catch {
		return false;
	}
}

if (isMainModule()) {
	process.exitCode = await runConnector(process.argv.slice(2), {
		stdio: createStdioStream(),
		stderr: process.stderr,
	});
}
