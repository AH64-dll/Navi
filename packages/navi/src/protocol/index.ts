/**
 * NAVI protocol v1: browser-safe frame types, fixed limits, validation and text codec.
 * Transport (length framing, sockets, ports) and the connection state machine live outside this module.
 */

export * from "./codec.ts";
export * from "./limits.ts";
export type * from "./types.ts";
export * from "./validate.ts";
