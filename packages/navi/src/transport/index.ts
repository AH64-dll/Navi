/**
 * NAVI transport layer: native-endian capped framing, bounded frame channels, the private POSIX CLI
 * socket, and the browser-launched native connector relay.
 *
 * Owns no authority and no model semantics. Protocol session state, sequence numbers and grant decisions
 * belong to the CLI core and the extension worker.
 */

export {
	createFrameChannel,
	type FrameChannel,
	type FrameChannelHandlers,
	type FrameChannelOptions,
} from "./channel.ts";
export {
	type ConnectorArgs,
	type ConnectorStreams,
	createStdioStream,
	EXIT_NO_CLI,
	EXIT_OK,
	EXIT_TRANSPORT,
	EXIT_USAGE,
	isAllowedCallerOrigin,
	parseConnectorArgs,
	runConnector,
} from "./connector.ts";
export {
	type ByteOrder,
	encodeFrame,
	FrameDecoder,
	nativeByteOrder,
	TransportError,
} from "./framing.ts";
export {
	assertPrivateSocketDir,
	BRIDGE_SOCKET_NAME,
	type BridgeConnection,
	type BridgeConnectOptions,
	type BridgeHandlers,
	type BridgeServer,
	type BridgeServerOptions,
	bridgeSocketPath,
	connectBridgeSocket,
	createBridgeConnection,
	createBridgeServer,
	DEFAULT_CONNECT_TIMEOUT_MS,
	DEFAULT_MAX_CONNECTIONS,
} from "./socket.ts";
