import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PassThrough } from "node:stream";
import { test } from "node:test";
import { writeNativeSetup } from "../src/cli/native-setup.ts";
import { formatOutput } from "../src/cli/output.ts";
import { serve } from "../src/cli/serve.ts";
import { Connection } from "../src/core/connection.ts";
import { type FrameChannel, systemClock } from "../src/core/ports.ts";
import type { Binding, Observation } from "../src/protocol/types.ts";
import { connectBridgeSocket } from "../src/transport/socket.ts";

test("terminal output stays one JSON record and escapes terminal controls/bidi", () => {
	const value = { text: "untrusted\n\u001b[2J\u0085\u202eFORGED" };
	const output = formatOutput(value);
	assert.ok(!/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/.test(output));
	assert.deepEqual(JSON.parse(output), value);
});

test("native setup pins one origin and writes only explicit developer artifacts", async () => {
	const directory = await mkdtemp(join(tmpdir(), "navi-setup-"));
	try {
		const outDir = join(directory, "native 'quoted");
		const extensionId = "a".repeat(32);
		const result = await writeNativeSetup({ outDir, socketDir: join(directory, "socket 'quoted"), extensionId });
		const manifest = JSON.parse(await readFile(result.manifest, "utf8")) as {
			path: string;
			allowed_origins: string[];
			type: string;
		};
		assert.deepEqual(manifest.allowed_origins, [`chrome-extension://${extensionId}/`]);
		assert.equal(manifest.path, result.launcher);
		assert.equal(manifest.type, "stdio");
		assert.equal((await stat(result.launcher)).mode & 0o777, 0o700);
		const launcher = await readFile(result.launcher, "utf8");
		assert.ok(launcher.startsWith("#!/bin/sh\nexec "));
		assert.ok(launcher.endsWith(' "$@"\n'));
		assert.ok(launcher.includes("'\\''"));
		await assert.rejects(writeNativeSetup({ outDir, socketDir: directory, extensionId }));
		assert.equal(await readFile(result.launcher, "utf8"), launcher, "existing artifacts are not overwritten");
		await assert.rejects(writeNativeSetup({ outDir, socketDir: directory, extensionId: "*" }));
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test(
	"CLI to real private socket to simulated worker observes and revokes without a model",
	{ timeout: 8000 },
	async () => {
		const directory = await mkdtemp(join(tmpdir(), "navi-cli-"));
		const input = new PassThrough();
		const output = new PassThrough();
		const events: Record<string, unknown>[] = [];
		const watchers = new Set<() => void>();
		let buffer = "";
		output.on("data", (chunk: Buffer) => {
			buffer += chunk.toString("utf8");
			let index = buffer.indexOf("\n");
			while (index !== -1) {
				events.push(JSON.parse(buffer.slice(0, index)) as Record<string, unknown>);
				buffer = buffer.slice(index + 1);
				index = buffer.indexOf("\n");
			}
			for (const notify of watchers) notify();
		});
		const wait = (predicate: (event: Record<string, unknown>) => boolean): Promise<Record<string, unknown>> =>
			new Promise((resolve, reject) => {
				const timer = setTimeout(() => {
					watchers.delete(check);
					reject(new Error("CLI output deadline"));
				}, 3000);
				const check = () => {
					const match = events.find(predicate);
					if (match) {
						clearTimeout(timer);
						watchers.delete(check);
						resolve(match);
					}
				};
				watchers.add(check);
				check();
			});
		const serving = serve({ socketDir: directory, input, output });
		let worker: Connection | undefined;
		try {
			await wait((event) => event.type === "ready");
			let receive: (raw: unknown) => void = () => {};
			let closed: () => void = () => {};
			const link = await connectBridgeSocket({
				socketDir: directory,
				onFrame: (raw) => receive(raw),
				onClose: () => closed(),
			});
			const channel: FrameChannel = {
				send: (frame) => link.channel.send(frame),
				onMessage: (handler) => {
					receive = handler;
				},
				onClose: (handler) => {
					closed = () => handler("peer-closed");
				},
				close: () => link.close(),
			};
			const binding: Binding = { grantId: "g1", tabId: 7, documentId: "doc1", pageEpoch: 1 };
			const observation: Observation = {
				observationId: "obs1",
				binding,
				capturedAt: 1,
				url: "https://fixture.test/",
				nodes: [],
				visited: 0,
				truncated: false,
				omitted: [],
			};
			worker = new Connection({
				role: "initiator",
				channel,
				clock: systemClock,
				peer: { kind: "extension", version: "0.0.1" },
				capabilities: ["observe"],
				onReady: () =>
					worker?.sendEvent({ grant: { grantId: "g1", binding, capabilities: ["observe"] }, reason: "attached" }),
				handleRequest: async () => ({ op: "observe", observation }),
			});
			worker.open();
			await wait((event) => event.type === "status" && event.grant !== null && event.grant !== undefined);
			input.write("observe\n");
			const result = await wait((event) => event.type === "result");
			assert.deepEqual(result.result, { source: "page", trust: "untrusted", observation });
			input.write("detach\n");
			await wait((event) => event.type === "status" && event.link === "closed" && event.grant === null);
		} finally {
			worker?.close("closed");
			input.end();
			await serving;
			await rm(directory, { recursive: true, force: true });
		}
	},
);
