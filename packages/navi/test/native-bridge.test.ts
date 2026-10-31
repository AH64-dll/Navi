/**
 * End-to-end check of the real native hop: a Chrome-like peer writes native-framed messages to the
 * connector process, the connector relays them over the private socket to a running CLI, and the CLI
 * answers. Nothing here is a mock of the transport: the connector is a real child process and the socket
 * is a real POSIX socket.
 */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PassThrough } from "node:stream";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { serve } from "../src/cli/serve.ts";
import { encodeFrame, FrameDecoder } from "../src/transport/framing.ts";

const repoRoot = fileURLToPath(new URL("../../../", import.meta.url));
const connectorEntry = fileURLToPath(new URL("../src/transport/connector.ts", import.meta.url));
const EXTENSION_ID = "a".repeat(32);

test(
	"the connector relays a real handshake and a rejected request between Chrome and the CLI",
	{ timeout: 15000 },
	async () => {
		const directory = await mkdtemp(join(tmpdir(), "navi-native-"));
		const input = new PassThrough();
		const cliOutput = new PassThrough();
		const serving = serve({ socketDir: directory, input, output: cliOutput });
		const child = spawn(
			process.execPath,
			[
				connectorEntry,
				"--socket-dir",
				directory,
				"--extension-id",
				EXTENSION_ID,
				`chrome-extension://${EXTENSION_ID}/`,
			],
			{ cwd: repoRoot, stdio: ["pipe", "pipe", "pipe"] },
		);
		const frames: unknown[] = [];
		const decoder = new FrameDecoder();
		child.stdout.on("data", (chunk: Buffer) => {
			frames.push(...decoder.push(chunk));
		});
		let diagnostics = "";
		child.stderr.on("data", (chunk: Buffer) => {
			diagnostics += chunk.toString("utf8");
		});
		const waitFor = async (
			predicate: (frame: Record<string, unknown>) => boolean,
		): Promise<Record<string, unknown>> => {
			const deadline = Date.now() + 8000;
			for (;;) {
				const match = frames.find((frame) => predicate(frame as Record<string, unknown>));
				if (match) return match as Record<string, unknown>;
				if (Date.now() > deadline)
					throw new Error(`no matching frame; saw ${JSON.stringify(frames)}; stderr ${diagnostics}`);
				await new Promise((resolve) => setTimeout(resolve, 20));
			}
		};
		const send = (frame: unknown): void => {
			child.stdin.write(encodeFrame(frame));
		};
		// Wait for the CLI to be listening before the connector attaches.
		await new Promise((resolve) => setTimeout(resolve, 300));
		try {
			send({
				v: 1,
				seq: 1,
				type: "hello",
				peer: { kind: "extension", version: "0.0.1" },
				capabilities: ["observe"],
			});
			const ack = await waitFor((frame) => frame.type === "hello.ack");
			assert.equal(typeof ack.connectionId, "string");
			assert.deepEqual(ack.peer, { kind: "cli", version: "0.0.1" });
			// The CLI owns the harness, not browser work: an inbound browser request is refused honestly
			// rather than served with authority the CLI does not have.
			send({
				v: 1,
				seq: 2,
				type: "request",
				connectionId: ack.connectionId,
				requestId: "req-1",
				payload: { op: "observe", binding: { grantId: "g1", tabId: 1, documentId: "d1", pageEpoch: 1 } },
			});
			const result = await waitFor((frame) => frame.type === "result" && frame.requestId === "req-1");
			assert.equal(result.ok, false);
			assert.equal((result.error as { code: string }).code, "UNSUPPORTED_OP");
			// An unknown frame cannot be smuggled through the relay into the session.
			send({
				v: 1,
				seq: 3,
				type: "request",
				connectionId: ack.connectionId,
				requestId: "req-2",
				payload: { op: "click" },
			});
			await new Promise((resolve) => setTimeout(resolve, 200));
			assert.ok(!frames.some((frame) => (frame as { requestId?: string }).requestId === "req-2"));
		} finally {
			child.kill();
			input.end();
			await serving;
			await rm(directory, { recursive: true, force: true });
		}
	},
);

test(
	"the connector refuses an unpinned caller origin and reports a missing CLI separately",
	{ timeout: 15000 },
	async () => {
		const directory = await mkdtemp(join(tmpdir(), "navi-native-"));
		try {
			const wrongOrigin = spawn(
				process.execPath,
				[
					connectorEntry,
					"--socket-dir",
					directory,
					"--extension-id",
					EXTENSION_ID,
					"chrome-extension://bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/",
				],
				{ cwd: repoRoot, stdio: ["pipe", "pipe", "pipe"] },
			);
			let wrongOriginError = "";
			wrongOrigin.stderr.on("data", (chunk: Buffer) => {
				wrongOriginError += chunk.toString("utf8");
			});
			const wrongOriginCode = await new Promise<number | null>((resolve) => wrongOrigin.on("exit", resolve));
			assert.equal(wrongOriginCode, 2);
			assert.match(wrongOriginError, /caller origin/);

			// No CLI listening: the connector reports it instead of starting a second runtime.
			const noCli = spawn(
				process.execPath,
				[
					connectorEntry,
					"--socket-dir",
					directory,
					"--extension-id",
					EXTENSION_ID,
					`chrome-extension://${EXTENSION_ID}/`,
				],
				{ cwd: repoRoot, stdio: ["pipe", "pipe", "pipe"] },
			);
			let noCliError = "";
			noCli.stderr.on("data", (chunk: Buffer) => {
				noCliError += chunk.toString("utf8");
			});
			const noCliCode = await new Promise<number | null>((resolve) => noCli.on("exit", resolve));
			assert.equal(noCliCode, 3);
			assert.match(noCliError, /NAVI CLI/);
		} finally {
			await rm(directory, { recursive: true, force: true });
		}
	},
);
