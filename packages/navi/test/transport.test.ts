/**
 * Transport tests: framing, bounded channels, the private socket, and the native connector relay.
 *
 * These use real streams, real sockets and a real spawned connector process. They do not exercise Chrome,
 * model calls or the protocol session layer.
 */
import assert from "node:assert/strict";
import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";
import { chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Duplex, PassThrough, type Readable } from "node:stream";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { MAX_ENVELOPE_BYTES } from "../src/protocol/limits.ts";
import { createFrameChannel, type FrameChannel, type FrameChannelOptions } from "../src/transport/channel.ts";
import {
	createStdioStream,
	EXIT_NO_CLI,
	EXIT_OK,
	EXIT_TRANSPORT,
	EXIT_USAGE,
	parseConnectorArgs,
	runConnector,
} from "../src/transport/connector.ts";
import { encodeFrame, FrameDecoder, nativeByteOrder, TransportError } from "../src/transport/framing.ts";
import {
	assertPrivateSocketDir,
	type BridgeConnection,
	bridgeSocketPath,
	connectBridgeSocket,
	createBridgeServer,
} from "../src/transport/socket.ts";

const CONNECTOR = fileURLToPath(new URL("../src/transport/connector.ts", import.meta.url));
const EXTENSION_ID = "abcdefghijklmnopabcdefghijklmnop";
const ORIGIN = `chrome-extension://${EXTENSION_ID}/`;

function makeSocketDir(): string {
	const root = mkdtempSync(join(tmpdir(), "navi-transport-"));
	const dir = join(root, "run");
	mkdirSync(dir, { mode: 0o700 });
	return dir;
}

function failureCode(error: unknown): string {
	assert.ok(error instanceof TransportError, `expected a TransportError, got ${String(error)}`);
	return error.code;
}

function delay(ms: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Bounded wait for an asynchronous condition. Fails instead of hanging the test runner. */
async function waitFor(condition: () => boolean, label: string, timeoutMs = 3_000): Promise<void> {
	const deadline = Date.now() + timeoutMs;
	while (Date.now() < deadline) {
		if (condition()) return;
		await delay(5);
	}
	assert.fail(`Timed out waiting for ${label}`);
}

function frameWithRawBody(body: string | Buffer): Buffer {
	const payload = typeof body === "string" ? Buffer.from(body, "utf8") : body;
	const frame = Buffer.alloc(4 + payload.byteLength);
	if (nativeByteOrder() === "LE") frame.writeUInt32LE(payload.byteLength, 0);
	else frame.writeUInt32BE(payload.byteLength, 0);
	payload.copy(frame, 4);
	return frame;
}

function duplexPair(): [Duplex, Duplex] {
	let left: Duplex;
	let right: Duplex;
	const options = (target: () => Duplex) => ({
		read(): void {},
		write(chunk: Uint8Array, _encoding: string, callback: (error?: Error | null) => void): void {
			target().push(chunk);
			callback();
		},
		final(callback: (error?: Error | null) => void): void {
			target().push(null);
			callback();
		},
	});
	left = new Duplex(options(() => right));
	right = new Duplex(options(() => left));
	return [left, right];
}

type Recorder = {
	channel: FrameChannel;
	frames: unknown[];
	closed: Promise<TransportError | null>;
};

function recordChannel(stream: Duplex, options: FrameChannelOptions = {}): Recorder {
	const frames: unknown[] = [];
	let resolveClose: (error: TransportError | null) => void = () => {};
	const closed = new Promise<TransportError | null>((resolve) => {
		resolveClose = resolve;
	});
	const channel = createFrameChannel(
		stream,
		{ onFrame: (frame) => frames.push(frame), onClose: (error) => resolveClose(error) },
		{ drainGraceMs: 150, ...options },
	);
	return { channel, frames, closed };
}

function readFrames(stream: Readable, count: number, timeoutMs = 5_000): Promise<unknown[]> {
	return new Promise((resolve, reject) => {
		const decoder = new FrameDecoder();
		const frames: unknown[] = [];
		const timer = setTimeout(() => {
			finish();
			reject(new Error(`Timed out waiting for ${count} frame(s)`));
		}, timeoutMs);
		const onData = (chunk: Buffer): void => {
			try {
				frames.push(...decoder.push(chunk));
			} catch (error) {
				finish();
				reject(error);
				return;
			}
			if (frames.length >= count) {
				finish();
				resolve(frames);
			}
		};
		const finish = (): void => {
			clearTimeout(timer);
			stream.off("data", onData);
		};
		stream.on("data", onData);
	});
}

type ConnectorChild = {
	child: ChildProcessWithoutNullStreams;
	exited: Promise<number | null>;
	stderr: () => string;
	stdout: () => string;
};

function spawnConnector(socketDir: string, origin = ORIGIN, extraArgs: string[] = []): ConnectorChild {
	const child = spawn(
		process.execPath,
		[CONNECTOR, "--socket-dir", socketDir, "--extension-id", EXTENSION_ID, ...extraArgs, origin],
		{ stdio: ["pipe", "pipe", "pipe"] },
	);
	let stderrText = "";
	let stdoutText = "";
	child.stderr.on("data", (chunk: Buffer) => {
		stderrText += chunk.toString("utf8");
	});
	child.stdout.on("data", (chunk: Buffer) => {
		stdoutText += chunk.toString("utf8");
	});
	const exited = new Promise<number | null>((resolve) => {
		child.once("exit", (code) => resolve(code));
	});
	return { child, exited, stderr: () => stderrText, stdout: () => stdoutText };
}

async function staleSocketPath(path: string): Promise<void> {
	const script =
		"const net = require('node:net'); const server = net.createServer();" +
		"server.listen(process.argv[1], () => { console.log('ready'); });";
	const child = spawn(process.execPath, ["-e", script, path], { stdio: ["ignore", "pipe", "inherit"] });
	await new Promise<void>((resolve, reject) => {
		const timer = setTimeout(() => reject(new Error("fixture socket never bound")), 5_000);
		child.stdout.on("data", (chunk: Buffer) => {
			if (!chunk.toString("utf8").includes("ready")) return;
			clearTimeout(timer);
			resolve();
		});
		child.once("exit", () => {
			clearTimeout(timer);
			reject(new Error("fixture socket process exited before binding"));
		});
	});
	child.kill("SIGKILL");
	await new Promise<void>((resolve) => child.once("exit", () => resolve()));
}

test("framing round-trips Unicode and counts UTF-8 bytes", () => {
	const value = { text: "NAVI\nمرحبا\r\n🌐\u2028\u2029", nested: { ok: true } };
	const frame = Buffer.from(encodeFrame(value));
	const expectedBody = Buffer.byteLength(JSON.stringify(value), "utf8");
	assert.equal(nativeByteOrder() === "LE" ? frame.readUInt32LE(0) : frame.readUInt32BE(0), expectedBody);
	assert.notEqual(expectedBody, JSON.stringify(value).length);
	const decoder = new FrameDecoder();
	assert.deepEqual(decoder.push(frame), [value]);
	decoder.finish();
});

test("framing survives every split, coalescing and one-byte chunks", () => {
	const values = [{ seq: 1 }, { text: "🌐🌐", seq: 2 }];
	const bytes = Buffer.concat(values.map((value) => Buffer.from(encodeFrame(value))));
	for (let split = 0; split <= bytes.byteLength; split += 1) {
		const decoder = new FrameDecoder();
		const frames = [...decoder.push(bytes.subarray(0, split)), ...decoder.push(bytes.subarray(split))];
		decoder.finish();
		assert.deepEqual(frames, values, `split at ${split}`);
	}
	const decoder = new FrameDecoder();
	const frames: unknown[] = [];
	for (const byte of bytes) frames.push(...decoder.push(Buffer.from([byte])));
	decoder.finish();
	assert.deepEqual(frames, values);
});

test("oversized declared length is rejected before allocating a body and never resyncs", () => {
	for (const length of [0, MAX_ENVELOPE_BYTES + 1, 0xffffffff]) {
		const header = Buffer.alloc(4);
		if (nativeByteOrder() === "LE") header.writeUInt32LE(length, 0);
		else header.writeUInt32BE(length, 0);
		const decoder = new FrameDecoder();
		let code = "";
		try {
			decoder.push(header);
		} catch (error) {
			code = failureCode(error);
		}
		assert.equal(code, length === 0 ? "BAD_FRAME_LENGTH" : "FRAME_TOO_LARGE");
		assert.equal(decoder.pendingBytes, 0);
		assert.throws(() => decoder.push(encodeFrame({ ok: true })));
	}
	assert.equal(
		failureCode(captureThrow(() => encodeFrame({ text: "x".repeat(MAX_ENVELOPE_BYTES) }))),
		"ENVELOPE_TOO_LARGE",
	);
	assert.equal(failureCode(captureThrow(() => encodeFrame([1, 2, 3]))), "NOT_JSON_OBJECT");
});

test("the exact cap is accepted and truncated input at EOF is an error", () => {
	const value = { text: "x".repeat(MAX_ENVELOPE_BYTES - 20) };
	const frame = Buffer.from(encodeFrame(value));
	assert.ok(frame.byteLength > MAX_ENVELOPE_BYTES - 20);
	const decoder = new FrameDecoder();
	assert.deepEqual(decoder.push(frame), [value]);
	decoder.finish();

	const complete = Buffer.from(encodeFrame({ ok: true }));
	for (const length of [1, 2, 3, 4, complete.byteLength - 1]) {
		const partial = new FrameDecoder();
		partial.push(complete.subarray(0, length));
		assert.equal(failureCode(captureThrow(() => partial.finish())), "TRUNCATED_FRAME");
	}
});

test("malformed UTF-8, malformed JSON and non-object frames fail closed", () => {
	const cases: Array<[Buffer, string]> = [
		[frameWithRawBody(""), "BAD_FRAME_LENGTH"],
		[frameWithRawBody("{"), "BAD_JSON"],
		[frameWithRawBody("null"), "NOT_JSON_OBJECT"],
		[frameWithRawBody("[]"), "NOT_JSON_OBJECT"],
	];
	for (const [frame, expected] of cases) {
		const decoder = new FrameDecoder();
		let code = "";
		try {
			decoder.push(frame);
		} catch (error) {
			code = failureCode(error);
		}
		assert.equal(code, expected);
	}
	const decoder = new FrameDecoder();
	assert.equal(
		failureCode(captureThrow(() => decoder.push(frameWithRawBody(Buffer.from([0x34, 0xff, 0xfe, 0x34]))))),
		"BAD_UTF8",
	);
});

test("channel decodes fragmented input and reports one close", async () => {
	const [left, right] = duplexPair();
	const recorder = recordChannel(left);
	// Writes on the peer duplex arrive on `left`'s readable side, as with a socket or a pipe.
	right.write(Buffer.from(encodeFrame({ seq: 1 })));
	right.write(Buffer.from(encodeFrame({ seq: 2 })));
	await new Promise((resolve) => setTimeout(resolve, 20));
	assert.deepEqual(recorder.frames, [{ seq: 1 }, { seq: 2 }]);
	right.end();
	assert.equal(await recorder.closed, null);
	assert.equal(recorder.channel.closed, true);
	assert.equal(recorder.channel.pendingFrames, 0);
});

test("channel fails on a decode error instead of resynchronising", async () => {
	const [left, right] = duplexPair();
	const recorder = recordChannel(left);
	right.write(frameWithRawBody("[1]"));
	const error = await recorder.closed;
	assert.equal(error?.code, "NOT_JSON_OBJECT");
	await assert.rejects(async () => recorder.channel.send({ late: true }));
});

test("channel bounds the outbound queue and closes on overflow", async () => {
	const blocked = new Duplex({
		highWaterMark: 1,
		read(): void {},
		write(_chunk: Uint8Array, _encoding: string, _callback: (error?: Error | null) => void): void {},
	});
	blocked.on("error", () => {});
	const recorder = recordChannel(blocked, { maxQueueFrames: 1, maxQueueBytes: MAX_ENVELOPE_BYTES * 4 });
	recorder.channel.send({ seq: 1 });
	recorder.channel.send({ seq: 2 });
	let code = "";
	try {
		recorder.channel.send({ seq: 3 });
	} catch (error) {
		code = failureCode(error);
	}
	assert.equal(code, "BACKPRESSURE");
	assert.equal((await recorder.closed)?.code, "BACKPRESSURE");
	assert.equal(recorder.channel.closed, true);
});

test("private socket directory rules are enforced", () => {
	const dir = makeSocketDir();
	assertPrivateSocketDir(dir);
	const root = mkdtempSync(join(tmpdir(), "navi-transport-"));
	chmodSync(dir, 0o755);
	assert.throws(() => assertPrivateSocketDir(dir), /mode 0700/);
	chmodSync(dir, 0o700);
	const linked = join(root, "linked");
	symlinkSync(dir, linked);
	assert.throws(() => assertPrivateSocketDir(linked), /symlink/);
	assert.throws(() => assertPrivateSocketDir("relative/run"), /absolute/);
	assert.throws(() => assertPrivateSocketDir(join(root, "missing")), /does not exist/);
	rmSync(root, { recursive: true, force: true });
});

test("the CLI server binds a private socket and refuses existing paths", async () => {
	const dir = makeSocketDir();
	const server = await createBridgeServer({ socketDir: dir, onFrame: () => {} });
	const stats = lstatSync(bridgeSocketPath(dir));
	assert.ok(stats.isSocket());
	assert.equal(stats.mode & 0o777, 0o600);
	await server.close();
	assert.equal(existsSync(bridgeSocketPath(dir)), false);

	const occupied = makeSocketDir();
	writeFileSync(bridgeSocketPath(occupied), "not a socket");
	await assert.rejects(createBridgeServer({ socketDir: occupied, onFrame: () => {} }), /Refusing to bind/);
	assert.equal(existsSync(bridgeSocketPath(occupied)), true);

	const stale = makeSocketDir();
	await staleSocketPath(bridgeSocketPath(stale));
	assert.ok(lstatSync(bridgeSocketPath(stale)).isSocket());
	await assert.rejects(createBridgeServer({ socketDir: stale, onFrame: () => {} }), /Refusing to bind/);
	assert.ok(lstatSync(bridgeSocketPath(stale)).isSocket());
});

test("connecting refuses a symlinked socket path and a missing CLI", async () => {
	const dir = makeSocketDir();
	const server = await createBridgeServer({ socketDir: dir, onFrame: () => {} });
	const other = makeSocketDir();
	try {
		symlinkSync(bridgeSocketPath(dir), bridgeSocketPath(other));
		await assert.rejects(connectBridgeSocket({ socketDir: other, onFrame: () => {} }), /symlink/);
	} finally {
		await server.close();
	}
	await assert.rejects(connectBridgeSocket({ socketDir: makeSocketDir(), onFrame: () => {} }), /not listening/);
});

test("socket hop delivers frames in both directions", async () => {
	const dir = makeSocketDir();
	const serverFrames: unknown[] = [];
	let serverConnection: BridgeConnection | undefined;
	const server = await createBridgeServer({
		socketDir: dir,
		onFrame: (frame) => serverFrames.push(frame),
		onOpen: (connection) => {
			serverConnection = connection;
		},
	});
	const clientFrames: unknown[] = [];
	let resolveClosed: (error: TransportError | null) => void = () => {};
	const clientClosed = new Promise<TransportError | null>((resolve) => {
		resolveClosed = resolve;
	});
	const client = await connectBridgeSocket({
		socketDir: dir,
		onFrame: (frame) => clientFrames.push(frame),
		onClose: (_connection, error) => resolveClosed(error),
	});
	client.channel.send({ type: "request", op: "observe" });
	serverConnection?.channel.send({ type: "result", ok: true });
	await waitFor(() => serverFrames.length === 1 && clientFrames.length === 1, "both hops to deliver a frame");
	assert.deepEqual(serverFrames, [{ type: "request", op: "observe" }]);
	assert.deepEqual(clientFrames, [{ type: "result", ok: true }]);
	await server.close();
	assert.equal(await Promise.race([clientClosed, delay(2_000).then(() => "timeout")]), null);
});

test("connector relays frames between browser stdio and the CLI socket", async () => {
	const dir = makeSocketDir();
	const received: unknown[] = [];
	const server = await createBridgeServer({
		socketDir: dir,
		onFrame: (frame, connection) => {
			received.push(frame);
			connection.channel.send({ type: "result", ok: true });
		},
	});
	const connector = spawnConnector(dir);
	try {
		connector.child.stdin.write(Buffer.from(encodeFrame({ type: "request", requestId: "r1" })));
		assert.deepEqual(await readFrames(connector.child.stdout, 1), [{ type: "result", ok: true }]);
		assert.deepEqual(received, [{ type: "request", requestId: "r1" }]);
		connector.child.stdin.end();
		assert.equal(await connector.exited, EXIT_OK);
	} finally {
		connector.child.kill("SIGKILL");
		await server.close();
	}
});

test("connector rejects a different caller origin without writing protocol bytes", async () => {
	const dir = makeSocketDir();
	const wrongId = "pppppppppppppppppppppppppppppppp";
	const connector = spawnConnector(dir, `chrome-extension://${wrongId}/`);
	assert.equal(await connector.exited, EXIT_USAGE);
	assert.equal(connector.stdout(), "");
	assert.match(connector.stderr(), /caller origin/);
});

test("connector reports a missing CLI as a setup error", async () => {
	const connector = spawnConnector(makeSocketDir());
	assert.equal(await connector.exited, EXIT_NO_CLI);
	assert.equal(connector.stdout(), "");
	assert.match(connector.stderr(), /not listening|Start the NAVI CLI/);
});

test("connector closes on a non-object frame from the browser", async () => {
	const dir = makeSocketDir();
	const server = await createBridgeServer({ socketDir: dir, onFrame: () => {} });
	const connector = spawnConnector(dir);
	try {
		connector.child.stdin.write(frameWithRawBody("[1]"));
		assert.equal(await connector.exited, EXIT_TRANSPORT);
		assert.match(connector.stderr(), /NOT_JSON_OBJECT/);
	} finally {
		connector.child.kill("SIGKILL");
		await server.close();
	}
});

test("connector argument parsing pins the socket directory and extension id", () => {
	assert.deepEqual(parseConnectorArgs(["--socket-dir", "/tmp/navi-1000", "--extension-id", EXTENSION_ID, ORIGIN]), {
		socketDir: "/tmp/navi-1000",
		extensionId: EXTENSION_ID,
		callerOrigin: ORIGIN,
	});
	assert.deepEqual(
		parseConnectorArgs([
			`--socket-dir=/tmp/navi-1000`,
			`--extension-id=${EXTENSION_ID}`,
			"--parent-window=0",
			ORIGIN,
		]),
		{ socketDir: "/tmp/navi-1000", extensionId: EXTENSION_ID, callerOrigin: ORIGIN },
	);
	for (const argv of [
		[ORIGIN, "--extension-id", EXTENSION_ID],
		["--socket-dir", "/tmp/navi-1000", ORIGIN],
		["--socket-dir", "/tmp/navi-1000", "--extension-id", "short", ORIGIN],
		["--socket-dir", "/tmp/navi-1000", "--extension-id", EXTENSION_ID, "--bogus"],
		["--socket-dir", "/tmp/navi-1000", "--extension-id", EXTENSION_ID],
		["--socket-dir", "/tmp/navi-1000", "--extension-id", EXTENSION_ID, ORIGIN, ORIGIN],
	]) {
		assert.ok("error" in parseConnectorArgs(argv), `expected a usage error for ${argv.join(" ")}`);
	}
});

test("runConnector refuses a foreign origin before it touches the socket", async () => {
	const seen: string[] = [];
	const [stdio, browser] = duplexPair();
	const stderr = {
		write(chunk: string): unknown {
			seen.push(chunk);
			return true;
		},
	};
	const wrongId = "pppppppppppppppppppppppppppppppp";
	const code = await runConnector(
		["--socket-dir", "/nonexistent-navi", "--extension-id", EXTENSION_ID, `chrome-extension://${wrongId}/`],
		{ stdio, stderr },
	);
	const stderrText = seen.join("");
	assert.equal(code, EXIT_USAGE);
	assert.match(stderrText, /caller origin/);

	// A non-Chrome origin argument is a usage error too, and still never opens a socket.
	assert.equal(
		await runConnector(
			["--socket-dir", "/nonexistent-navi", "--extension-id", EXTENSION_ID, "https://evil.example/"],
			{
				stdio,
				stderr,
			},
		),
		EXIT_USAGE,
	);
	assert.match(seen.join(""), /Unexpected argument/);
	browser.destroy();
});

test("connector buffers a hello that arrives before the CLI connection is ready", async () => {
	const dir = makeSocketDir();
	const received: unknown[] = [];
	const hello = { type: "hello", seq: 1, peer: { kind: "extension", version: "0.0.1" } };
	const server = await createBridgeServer({
		socketDir: dir,
		onFrame: (frame) => received.push(frame),
	});
	const [stdio, browser] = duplexPair();
	// The hello is already queued on the host's stdin side before the relay starts and before the socket connects.
	browser.write(Buffer.from(encodeFrame(hello)));
	let stderrText = "";
	const code = runConnector(["--socket-dir", dir, "--extension-id", EXTENSION_ID, ORIGIN], {
		stdio,
		stderr: {
			write(chunk: string): unknown {
				stderrText += chunk;
				return true;
			},
		},
	});
	await waitFor(() => received.length === 1, "the CLI to receive the buffered hello");
	assert.deepEqual(received, [hello]);
	assert.equal(stderrText, "");
	browser.end();
	assert.equal(await code, EXIT_OK);
	await server.close();
});

test("the CLI server accepts one attachment at a time", async () => {
	const dir = makeSocketDir();
	const server = await createBridgeServer({ socketDir: dir, onFrame: () => {} });
	const first = await connectBridgeSocket({ socketDir: dir, onFrame: () => {} });
	let refused = false;
	await connectBridgeSocket({
		socketDir: dir,
		onFrame: () => {},
		onClose: () => {
			refused = true;
		},
	});
	await waitFor(() => refused, "the extra connection to be refused");
	first.channel.close();
	await server.close();
});

test("stdio stream delays reading, pauses under backpressure and detaches on close", async () => {
	const input = new PassThrough();
	const output = new PassThrough();
	const stdio = createStdioStream(input, output);
	assert.equal(input.isPaused(), true);
	const recorder = recordChannel(stdio);
	await waitFor(() => !input.isPaused(), "stdin to resume for a reader");
	input.write(Buffer.from(encodeFrame({ seq: 1 })));
	await waitFor(() => recorder.frames.length === 1, "the inbound frame");
	assert.deepEqual(recorder.frames, [{ seq: 1 }]);

	const outChunks: Buffer[] = [];
	output.on("data", (chunk: Buffer) => outChunks.push(chunk));
	recorder.channel.send({ ok: true });
	await waitFor(() => outChunks.length > 0, "the outbound frame");
	assert.deepEqual(new FrameDecoder().push(Buffer.concat(outChunks)), [{ ok: true }]);

	// A stalled consumer must stop pulling: pushing past the readable high-water mark pauses the input.
	const slowInput = new PassThrough();
	const slowStdio = createStdioStream(slowInput, new PassThrough());
	slowStdio.resume();
	await waitFor(() => !slowInput.isPaused(), "slow stdin to start flowing");
	slowStdio.pause();
	slowInput.write(Buffer.alloc(256 * 1024, 0x20));
	await waitFor(() => slowInput.isPaused(), "slow stdin to pause under backpressure");
	slowStdio.destroy();
	await waitFor(() => slowInput.listenerCount("data") === 0, "input listeners to detach");
	assert.equal(slowInput.isPaused(), true);
});

test("socket path ancestors must not be symlinks or world writable", () => {
	const root = mkdtempSync(join(tmpdir(), "navi-ancestor-"));
	const real = join(root, "real");
	mkdirSync(real, { mode: 0o700 });
	symlinkSync(real, join(root, "linked"));
	assert.throws(() => assertPrivateSocketDir(join(root, "linked", "run")), /symlink/);

	const open = join(root, "open");
	mkdirSync(open, { mode: 0o700 });
	mkdirSync(join(open, "run"), { mode: 0o700 });
	chmodSync(open, 0o777);
	assert.throws(() => assertPrivateSocketDir(join(open, "run")), /writable/);
	rmSync(root, { recursive: true, force: true });
});

function captureThrow(run: () => unknown): unknown {
	try {
		run();
	} catch (error) {
		return error;
	}
	throw new Error("expected the call to throw");
}
