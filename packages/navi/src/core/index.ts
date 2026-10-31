/**
 * NAVI core: browser-neutral ports, protocol connection state, the CLI client and the grant manager.
 *
 * No Node, Chrome or DOM imports. The Node transport implements `FrameChannel`; the extension worker
 * uses `Connection` (initiator role) plus `GrantManager`; the CLI uses `BrowserClient`.
 */

export type { BrowserClientOptions } from "./client.ts";
export { BrowserClient, CLIENT_DEFAULT_LIMITS } from "./client.ts";
export type {
	ConnectionLimits,
	ConnectionOptions,
	ConnectionRole,
	ConnectionState,
	ReadyInfo,
	RequestHandler,
} from "./connection.ts";
export { Connection, DEFAULT_CONNECTION_LIMITS } from "./connection.ts";
export { isFatalErrorCode, naviError, toWireErrorCode } from "./errors.ts";
export type { GrantSnapshot } from "./grants.ts";
export { bindingMatches, capabilityGranted, GrantManager } from "./grants.ts";
export type {
	BrowserPort,
	BrowserPortStatus,
	ChannelCloseReason,
	Clock,
	FrameChannel,
	LinkState,
	ScrollArgument,
} from "./ports.ts";
export { NaviRequestError, systemClock } from "./ports.ts";
