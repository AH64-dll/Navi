/**
 * Private POSIX pathname socket for the NAVI bridge.
 *
 * The CLI creates one socket in an explicitly configured directory. The directory must be owned by the
 * current user and mode 0700, the socket is mode 0600, and an existing path is never unlinked or reused:
 * a stale socket means "stop and check", not "overwrite". Same-account processes can still open the socket;
 * this is a private-path boundary, not a sandbox, and no peer credential is asserted.
 */
import { chmodSync, lstatSync, type Stats, unlinkSync } from "node:fs";
import { createConnection, createServer, type Server, type Socket } from "node:net";
import { isAbsolute, join, resolve, sep } from "node:path";
import { createFrameChannel, type FrameChannel, type FrameChannelHandlers } from "./channel.ts";
import type { TransportError } from "./framing.ts";

export const BRIDGE_SOCKET_NAME = "bridge.sock";
export const DEFAULT_CONNECT_TIMEOUT_MS = 2_000;
/** One CLI owns one browser attachment at a time; extra connections are refused, not queued. */
export const DEFAULT_MAX_CONNECTIONS = 1;

export type BridgeConnection = {
	readonly channel: FrameChannel;
	/** Peer label for diagnostics. POSIX sockets carry no peer address. */
	readonly remote: string;
	close(error?: TransportError): void;
};

export type BridgeHandlers = {
	onFrame(frame: unknown, connection: BridgeConnection): void;
	onClose?(connection: BridgeConnection, error: TransportError | null): void;
	onError?(error: Error): void;
};

export type BridgeServerOptions = BridgeHandlers & {
	socketDir: string;
	onOpen?(connection: BridgeConnection): void;
	maxConnections?: number;
};

export type BridgeServer = {
	readonly path: string;
	readonly closed: boolean;
	readonly connections: number;
	close(): Promise<void>;
};

export type BridgeConnectOptions = BridgeHandlers & {
	socketDir: string;
	timeoutMs?: number;
};

type FileIdentity = { dev: number; ino: number };

/** Socket path inside the configured directory. The name is fixed; there is no discovery protocol. */
export function bridgeSocketPath(socketDir: string): string {
	if (!isAbsolute(socketDir)) throw new Error(`socket directory must be absolute: ${socketDir}`);
	return join(socketDir, BRIDGE_SOCKET_NAME);
}

/**
 * Checks every ancestor of `socketDir` below the root: a directory, not a symlink, owned by root or this
 * user, and not group or world writable. A sticky, root-owned directory such as `/tmp` is allowed because
 * it stops another user renaming our entry.
 */
function assertSafeAncestors(socketDir: string, uid: number | undefined): void {
	const segments = resolve(socketDir)
		.split(sep)
		.filter((segment) => segment.length > 0);
	let prefix: string = sep;
	for (let index = 0; index < segments.length - 1; index += 1) {
		prefix = join(prefix, segments[index]);
		let stats: Stats;
		try {
			stats = lstatSync(prefix);
		} catch {
			throw new Error(`NAVI bridge socket directory has a missing ancestor: ${prefix}`);
		}
		if (stats.isSymbolicLink()) throw new Error(`NAVI bridge socket path crosses a symlink: ${prefix}`);
		if (!stats.isDirectory()) throw new Error(`NAVI bridge socket path crosses a non-directory: ${prefix}`);
		if (uid !== undefined && stats.uid !== 0 && stats.uid !== uid) {
			throw new Error(`NAVI bridge socket path crosses a directory owned by uid ${stats.uid}: ${prefix}`);
		}
		const stickyRoot = stats.uid === 0 && (stats.mode & 0o1000) !== 0;
		if ((stats.mode & 0o022) !== 0 && !stickyRoot) {
			throw new Error(`NAVI bridge socket path crosses a group or world writable directory: ${prefix}`);
		}
	}
}

/**
 * Rejects a socket directory that another user could replace or read: the whole ancestor chain must be
 * trustworthy and the final directory must be owned by this user with mode 0700.
 */
export function assertPrivateSocketDir(socketDir: string): void {
	if (process.platform === "win32") {
		throw new Error("The NAVI bridge socket is POSIX-only; Windows named pipes are not implemented");
	}
	if (!isAbsolute(socketDir)) throw new Error(`socket directory must be absolute: ${socketDir}`);
	assertSafeAncestors(socketDir, process.getuid?.());
	const path = bridgeSocketPath(socketDir);
	let stats: ReturnType<typeof lstatSync>;
	try {
		stats = lstatSync(socketDir);
	} catch {
		throw new Error(`NAVI bridge socket directory does not exist: ${socketDir}`);
	}
	if (stats.isSymbolicLink()) throw new Error(`NAVI bridge socket directory must not be a symlink: ${socketDir}`);
	if (!stats.isDirectory()) throw new Error(`NAVI bridge socket directory is not a directory: ${socketDir}`);
	const uid = process.getuid?.();
	if (uid !== undefined && stats.uid !== uid) {
		throw new Error(`NAVI bridge socket directory is owned by uid ${stats.uid}, not ${uid}: ${path}`);
	}
	if ((stats.mode & 0o077) !== 0) {
		throw new Error(`NAVI bridge socket directory must be private (mode 0700): ${socketDir}`);
	}
}

/** Verifies that `path` is a socket we own with no group or other access. */
function assertSocketFile(path: string): FileIdentity {
	let stats: ReturnType<typeof lstatSync>;
	try {
		stats = lstatSync(path);
	} catch {
		throw new Error(`NAVI CLI is not listening on ${path}`);
	}
	if (stats.isSymbolicLink()) throw new Error(`NAVI bridge socket path must not be a symlink: ${path}`);
	if (!stats.isSocket()) throw new Error(`NAVI bridge socket path is not a socket: ${path}`);
	const uid = process.getuid?.();
	if (uid !== undefined && stats.uid !== uid) throw new Error(`NAVI bridge socket is owned by another user: ${path}`);
	if ((stats.mode & 0o077) !== 0) throw new Error(`NAVI bridge socket must be private (mode 0600): ${path}`);
	return { dev: stats.dev, ino: stats.ino };
}

function pathExists(path: string): boolean {
	try {
		lstatSync(path);
		return true;
	} catch {
		return false;
	}
}

function remoteLabel(socket: Socket): string {
	return socket.remoteAddress ?? "local";
}

/** Wraps one accepted or connected socket in a bounded frame channel. */
export function createBridgeConnection(socket: Socket, handlers: FrameChannelHandlers): BridgeConnection {
	const channel = createFrameChannel(socket, handlers);
	return {
		channel,
		remote: remoteLabel(socket),
		close(error?: TransportError): void {
			channel.close(error);
		},
	};
}

/** Listens on the fixed private socket path. Refuses to bind over an existing path. */
export async function createBridgeServer(options: BridgeServerOptions): Promise<BridgeServer> {
	assertPrivateSocketDir(options.socketDir);
	const path = bridgeSocketPath(options.socketDir);
	if (pathExists(path)) {
		throw new Error(
			`Refusing to bind ${path}: the path already exists. Stop any running NAVI CLI and remove a stale socket yourself.`,
		);
	}

	const maxConnections = options.maxConnections ?? DEFAULT_MAX_CONNECTIONS;
	const connections = new Set<BridgeConnection>();
	let closed = false;
	const server = createServer();
	server.on("error", (error: Error) => options.onError?.(error));

	try {
		await new Promise<void>((resolve, reject) => {
			const onListenError = (error: Error): void => {
				reject(error);
			};
			server.once("error", onListenError);
			server.listen(path, () => {
				server.off("error", onListenError);
				resolve();
			});
		});
	} catch (error) {
		server.removeAllListeners("error");
		server.on("error", () => {});
		server.close();
		throw error;
	}

	let identity: FileIdentity;
	try {
		chmodSync(path, 0o600);
		identity = assertSocketFile(path);
	} catch (error) {
		await closeServer(server);
		removeBoundSocket(path);
		throw error;
	}

	server.on("connection", (socket: Socket) => {
		if (closed) {
			socket.destroy();
			return;
		}
		if (connections.size >= maxConnections) {
			options.onError?.(new Error(`Refusing connection: ${maxConnections} are already open`));
			socket.destroy();
			return;
		}
		const holder: { connection?: BridgeConnection } = {};
		const connection = createBridgeConnection(socket, {
			onFrame: (frame) => {
				const current = holder.connection;
				if (current) options.onFrame(frame, current);
			},
			onClose: (error) => {
				const current = holder.connection;
				if (!current) return;
				connections.delete(current);
				options.onClose?.(current, error);
			},
		});
		holder.connection = connection;
		connections.add(connection);
		options.onOpen?.(connection);
	});

	return {
		path,
		get closed(): boolean {
			return closed;
		},
		get connections(): number {
			return connections.size;
		},
		async close(): Promise<void> {
			if (closed) return;
			closed = true;
			for (const connection of [...connections]) connection.close();
			connections.clear();
			await closeServer(server);
			removeSocketIfOwned(path, identity);
		},
	};
}

/** Connects to a NAVI CLI that is already listening. Missing or unsafe paths fail instead of retrying. */
export async function connectBridgeSocket(options: BridgeConnectOptions): Promise<BridgeConnection> {
	assertPrivateSocketDir(options.socketDir);
	const path = bridgeSocketPath(options.socketDir);
	assertSocketFile(path);

	const timeoutMs = options.timeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS;
	const socket = createConnection(path);
	try {
		await new Promise<void>((resolve, reject) => {
			const timer = setTimeout(() => {
				reject(new Error(`Timed out connecting to ${path} after ${timeoutMs} ms`));
			}, timeoutMs);
			timer.unref();
			const onError = (error: Error): void => {
				clearTimeout(timer);
				reject(error);
			};
			socket.once("error", onError);
			socket.once("connect", () => {
				clearTimeout(timer);
				socket.off("error", onError);
				resolve();
			});
		});
	} catch (error) {
		socket.destroy();
		throw error;
	}

	const holder: { connection?: BridgeConnection } = {};
	const connection = createBridgeConnection(socket, {
		onFrame: (frame) => {
			const current = holder.connection;
			if (current) options.onFrame(frame, current);
		},
		onClose: (error) => {
			const current = holder.connection;
			if (current) options.onClose?.(current, error);
		},
	});
	holder.connection = connection;
	return connection;
}

function closeServer(server: Server): Promise<void> {
	return new Promise<void>((resolve) => {
		server.close(() => resolve());
	});
}

/** Removes a socket this call just bound, after the bind was verified to have created it. */
function removeBoundSocket(path: string): void {
	try {
		if (lstatSync(path).isSocket()) unlinkSync(path);
	} catch {
		// Already removed.
	}
}

/** Removes only the socket file this server created, identified by device and inode. */
function removeSocketIfOwned(path: string, identity: FileIdentity): void {
	try {
		const stats = lstatSync(path);
		if (stats.isSocket() && stats.dev === identity.dev && stats.ino === identity.ino) unlinkSync(path);
	} catch {
		// Already removed, for example by the socket runtime on close.
	}
}
