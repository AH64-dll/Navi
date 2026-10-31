import { randomUUID } from "node:crypto";
import { mkdir } from "node:fs/promises";
import { createInterface } from "node:readline";
import type { Readable, Writable } from "node:stream";
import { BrowserClient } from "../core/client.ts";
import { type ChannelCloseReason, type FrameChannel, NaviRequestError, systemClock } from "../core/ports.ts";
import { type BridgeConnection, createBridgeServer } from "../transport/socket.ts";
import { DIAGNOSTIC_COMMANDS, runDiagnostic } from "./diagnostics.ts";
import { formatOutput } from "./output.ts";

type Session = {
	client: BrowserClient;
	receive(raw: unknown): void;
	closed(reason: ChannelCloseReason): void;
};

export type ServeOptions = { socketDir: string; input?: Readable; output?: Writable };

/** One user-started diagnostic runtime and one browser connection. No model/provider calls. */
export async function serve(options: ServeOptions): Promise<void> {
	const input = options.input ?? process.stdin;
	const output = options.output ?? process.stdout;
	const emit = (value: unknown): void => {
		output.write(`${formatOutput(value)}\n`);
	};
	await mkdir(options.socketDir, { recursive: true, mode: 0o700 });
	let current: Session | undefined;
	const sessions = new Map<BridgeConnection, Session>();
	const server = await createBridgeServer({
		socketDir: options.socketDir,
		maxConnections: 1,
		onOpen: (connection) => {
			let receive: (raw: unknown) => void = () => {};
			let closed: (reason: ChannelCloseReason) => void = () => {};
			const channel: FrameChannel = {
				send: (frame) => connection.channel.send(frame),
				onMessage: (handler) => {
					receive = handler;
				},
				onClose: (handler) => {
					closed = handler;
				},
				close: () => connection.close(),
			};
			const client = new BrowserClient({ channel, clock: systemClock, newConnectionId: randomUUID });
			const session: Session = { client, receive: (raw) => receive(raw), closed: (reason) => closed(reason) };
			sessions.set(connection, session);
			current = session;
			client.onStatus((state) => emit({ type: "status", ...state }));
			client.open();
		},
		onFrame: (frame, connection) => sessions.get(connection)?.receive(frame),
		onClose: (connection, error) => {
			const session = sessions.get(connection);
			session?.closed(error ? "error" : "peer-closed");
			if (current === session) current = undefined;
			sessions.delete(connection);
		},
		onError: () => emit({ type: "error", message: "Local browser connection failed" }),
	});
	const terminal = createInterface({
		input,
		output,
		terminal: Boolean(input === process.stdin && process.stdin.isTTY),
	});
	const stop = (): void => terminal.close();
	process.once("SIGINT", stop);
	process.once("SIGTERM", stop);
	emit({ type: "ready", socket: server.path, mode: "diagnostic-scaffold", commands: DIAGNOSTIC_COMMANDS });
	try {
		await new Promise<void>((resolve) => {
			terminal.once("close", resolve);
			terminal.on("line", (line: string) => {
				if (line.trim() === "quit") {
					terminal.close();
					return;
				}
				if (!line.trim()) return;
				if (!current) {
					emit({ type: "status", link: "disconnected", message: "Attach from the extension popup" });
					return;
				}
				// Do not serialize the input loop: cancel must work while observe/action is pending.
				void runDiagnostic(current.client, line).then(
					(result) => emit({ type: "result", result }),
					(error: unknown) =>
						emit({
							type: "error",
							code: error instanceof NaviRequestError ? error.code : "BAD_ARGUMENT",
							message: error instanceof Error ? error.message : "Diagnostic command failed",
						}),
				);
			});
		});
	} finally {
		process.removeListener("SIGINT", stop);
		process.removeListener("SIGTERM", stop);
		await current?.client.disconnect("closed");
		await server.close();
	}
}
