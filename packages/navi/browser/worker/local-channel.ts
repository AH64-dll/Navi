/**
 * Local, extension-internal message shapes and their strict validators.
 *
 * These messages never travel on the native wire, never come from a page, and never carry authority: the
 * worker re-checks browser-provided sender identity and its own grant state before acting. Deep page
 * payloads (`ObservationDraft`) are validated once, by the shared protocol validator, after the worker
 * stamps its own binding onto the frame.
 */
import type {
	Binding,
	Capability,
	EffectOutcome,
	NaviError,
	Observation,
	ScrollAlign,
	WireErrorCode,
} from "../../src/protocol/types.ts";

/** Port name used by the injected top-frame content script. */
export const CONTENT_PORT_NAME = "navi.content";

/** Extension-relative popup path; the worker compares it against `runtime.getURL`. */
export const POPUP_PATH = "popup.html";

/** Content-script source injected into the granted top frame. */
export const CONTENT_SCRIPT_FILE = "content.js";

/** Longest diagnostic message accepted on a local channel. */
export const MAX_LOCAL_MESSAGE_CHARS = 200;

/** Observation the content script produces. The worker replaces `binding` with its own grant binding. */
export type ObservationDraft = Omit<Observation, "binding">;

/** Popup to worker. */
export type PopupRequest =
	| { t: "popup.state" }
	| { t: "popup.attach"; capabilities: Capability[] }
	| { t: "popup.detach" };

/** Worker to popup. Bounded status; never containing node text. */
export type PopupState = {
	phase: "detached" | "attaching" | "attached";
	/** Capabilities the user asked for and that would apply to the next attach. */
	requested: Capability[];
	/** Capabilities of the grant in force, empty while detached. */
	granted: Capability[];
	tab?: { tabId: number; title?: string; url?: string };
	observationId?: string;
	error?: NaviError;
};

export type PopupResponse = { t: "popup.state"; state: PopupState };

/** Content script to worker. */
export type ContentOutbound =
	| { t: "content.hello" }
	| { t: "content.observation"; requestId: string; draft: ObservationDraft }
	| { t: "content.action"; requestId: string; outcome: EffectOutcome; scrollX?: number; scrollY?: number }
	| { t: "content.failure"; requestId: string; code: ContentFailureCode; message: string }
	| { t: "content.navigated" };

/** Worker to content script. */
export type ContentInbound =
	/**
	 * The worker chooses the observation id and supplies the authoritative binding. The content script
	 * copies the binding into the collection request; the worker re-stamps its own binding afterwards.
	 */
	| { t: "content.observe"; requestId: string; observationId: string; binding: Binding }
	| { t: "content.focus"; requestId: string; ref: string }
	| { t: "content.scroll"; requestId: string; ref?: string; align?: ScrollAlign; deltaX?: number; deltaY?: number }
	| { t: "content.cancel"; requestId: string }
	| { t: "content.detach" };

/** Failure codes a content script may report: a subset of the wire codes, never a transport code. */
export type ContentFailureCode = Extract<
	WireErrorCode,
	| "CAPTURE_FAILED"
	| "TOO_LARGE"
	| "UNKNOWN_REF"
	| "BAD_ARGUMENT"
	| "TAB_NOT_ACTIVE"
	| "CANCELLED"
	| "OUTCOME_UNKNOWN"
	| "INTERNAL"
>;

const CONTENT_FAILURE_CODES: readonly ContentFailureCode[] = [
	"CAPTURE_FAILED",
	"TOO_LARGE",
	"UNKNOWN_REF",
	"BAD_ARGUMENT",
	"TAB_NOT_ACTIVE",
	"CANCELLED",
	"OUTCOME_UNKNOWN",
	"INTERNAL",
];
const CAPABILITY_VALUES: readonly Capability[] = ["observe", "focus", "scroll"];
const SCROLL_ALIGN_VALUES: readonly ScrollAlign[] = ["start", "center", "end"];
const OUTCOME_VALUES: readonly EffectOutcome[] = ["applied", "unknown"];

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Rejects unknown fields, missing fields and explicit `undefined` values. */
function expectKeys(value: Record<string, unknown>, allowed: readonly string[], required: readonly string[]): void {
	for (const key of Object.keys(value)) {
		if (!allowed.includes(key)) throw new LocalMessageError(`${key}: unknown field`);
		if (value[key] === undefined) throw new LocalMessageError(`${key}: undefined is not a value`);
	}
	for (const key of required) {
		if (!(key in value)) throw new LocalMessageError(`${key}: missing field`);
	}
}

function expectString(value: unknown, key: string, maxChars: number, allowEmpty = false): string {
	if (typeof value !== "string") throw new LocalMessageError(`${key}: expected a string`);
	if (!allowEmpty && value.length === 0) throw new LocalMessageError(`${key}: expected a non-empty string`);
	if (value.length > maxChars) throw new LocalMessageError(`${key}: too long`);
	return value;
}

function expectId(value: unknown, key: string): string {
	return expectString(value, key, 64);
}

function expectInt(value: unknown, key: string, min: number, max: number): number {
	if (typeof value !== "number" || !Number.isSafeInteger(value) || value < min || value > max) {
		throw new LocalMessageError(`${key}: expected an integer between ${min} and ${max}`);
	}
	return value;
}

function expectCapabilities(value: unknown, key: string): Capability[] {
	if (!Array.isArray(value) || value.length === 0) throw new LocalMessageError(`${key}: expected capabilities`);
	const seen = new Set<string>();
	for (const entry of value) {
		if (typeof entry !== "string" || !CAPABILITY_VALUES.includes(entry as Capability)) {
			throw new LocalMessageError(`${key}: unknown capability`);
		}
		if (seen.has(entry)) throw new LocalMessageError(`${key}: duplicate capability`);
		seen.add(entry);
	}
	return value as Capability[];
}

/** Raised for any local message that does not match its exact schema. */
export class LocalMessageError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "LocalMessageError";
	}
}

export function parsePopupRequest(value: unknown): PopupRequest {
	if (!isRecord(value)) throw new LocalMessageError("expected a JSON object");
	const t = value.t;
	if (t === "popup.state") {
		expectKeys(value, ["t"], ["t"]);
		return { t: "popup.state" };
	}
	if (t === "popup.attach") {
		expectKeys(value, ["t", "capabilities"], ["t", "capabilities"]);
		return { t: "popup.attach", capabilities: expectCapabilities(value.capabilities, "capabilities") };
	}
	if (t === "popup.detach") {
		expectKeys(value, ["t"], ["t"]);
		return { t: "popup.detach" };
	}
	throw new LocalMessageError("t: unknown popup message");
}

export function parseContentOutbound(value: unknown): ContentOutbound {
	if (!isRecord(value)) throw new LocalMessageError("expected a JSON object");
	const t = value.t;
	switch (t) {
		case "content.hello": {
			expectKeys(value, ["t"], ["t"]);
			return { t: "content.hello" };
		}
		case "content.observation": {
			expectKeys(value, ["t", "requestId", "draft"], ["t", "requestId", "draft"]);
			if (!isRecord(value.draft)) throw new LocalMessageError("draft: expected an object");
			return {
				t: "content.observation",
				requestId: expectId(value.requestId, "requestId"),
				// The worker stamps the binding and validates the full observation with the shared validator.
				draft: value.draft as unknown as ObservationDraft,
			};
		}
		case "content.action": {
			expectKeys(value, ["t", "requestId", "outcome", "scrollX", "scrollY"], ["t", "requestId", "outcome"]);
			const outcome = value.outcome;
			if (typeof outcome !== "string" || !OUTCOME_VALUES.includes(outcome as EffectOutcome)) {
				throw new LocalMessageError("outcome: expected applied or unknown");
			}
			const action: ContentOutbound = {
				t: "content.action",
				requestId: expectId(value.requestId, "requestId"),
				outcome: outcome as EffectOutcome,
			};
			if (value.scrollX !== undefined)
				action.scrollX = expectInt(value.scrollX, "scrollX", -1_000_000_000, 1_000_000_000);
			if (value.scrollY !== undefined)
				action.scrollY = expectInt(value.scrollY, "scrollY", -1_000_000_000, 1_000_000_000);
			return action;
		}
		case "content.failure": {
			expectKeys(value, ["t", "requestId", "code", "message"], ["t", "requestId", "code", "message"]);
			const code = value.code;
			if (typeof code !== "string" || !CONTENT_FAILURE_CODES.includes(code as ContentFailureCode)) {
				throw new LocalMessageError("code: unknown content failure code");
			}
			return {
				t: "content.failure",
				requestId: expectId(value.requestId, "requestId"),
				code: code as ContentFailureCode,
				message: expectString(value.message, "message", MAX_LOCAL_MESSAGE_CHARS, true),
			};
		}
		case "content.navigated": {
			expectKeys(value, ["t"], ["t"]);
			return { t: "content.navigated" };
		}
		default:
			throw new LocalMessageError("t: unknown content message");
	}
}

/** Validates the document-ish fields the content script is allowed to report. */
export function parseContentInbound(value: unknown): ContentInbound {
	if (!isRecord(value)) throw new LocalMessageError("expected a JSON object");
	const t = value.t;
	switch (t) {
		case "content.observe": {
			expectKeys(
				value,
				["t", "requestId", "observationId", "binding"],
				["t", "requestId", "observationId", "binding"],
			);
			if (!isRecord(value.binding)) throw new LocalMessageError("binding: expected an object");
			return {
				t: "content.observe",
				requestId: expectId(value.requestId, "requestId"),
				observationId: expectId(value.observationId, "observationId"),
				binding: value.binding as unknown as Binding,
			};
		}
		case "content.focus": {
			expectKeys(value, ["t", "requestId", "ref"], ["t", "requestId", "ref"]);
			return {
				t: "content.focus",
				requestId: expectId(value.requestId, "requestId"),
				ref: expectId(value.ref, "ref"),
			};
		}
		case "content.scroll": {
			expectKeys(value, ["t", "requestId", "ref", "align", "deltaX", "deltaY"], ["t", "requestId"]);
			const scroll: ContentInbound = { t: "content.scroll", requestId: expectId(value.requestId, "requestId") };
			if (value.ref !== undefined) scroll.ref = expectId(value.ref, "ref");
			if (value.align !== undefined) {
				if (typeof value.align !== "string" || !SCROLL_ALIGN_VALUES.includes(value.align as ScrollAlign)) {
					throw new LocalMessageError("align: expected start, center or end");
				}
				scroll.align = value.align as ScrollAlign;
			}
			if (value.deltaX !== undefined) scroll.deltaX = expectInt(value.deltaX, "deltaX", -1_000_000, 1_000_000);
			if (value.deltaY !== undefined) scroll.deltaY = expectInt(value.deltaY, "deltaY", -1_000_000, 1_000_000);
			if (
				scroll.ref === undefined &&
				scroll.align === undefined &&
				scroll.deltaX === undefined &&
				scroll.deltaY === undefined
			) {
				throw new LocalMessageError("scroll needs a ref, an align value or a delta");
			}
			return scroll;
		}
		case "content.cancel": {
			expectKeys(value, ["t", "requestId"], ["t", "requestId"]);
			return { t: "content.cancel", requestId: expectId(value.requestId, "requestId") };
		}
		case "content.detach": {
			expectKeys(value, ["t"], ["t"]);
			return { t: "content.detach" };
		}
		default:
			throw new LocalMessageError("t: unknown worker message");
	}
}

/** Accepts a worker popup state only if it matches the exact documented shape. */
export function parsePopupState(value: unknown): PopupState {
	if (!isRecord(value)) throw new LocalMessageError("expected a JSON object");
	expectKeys(value, ["t", "state"], ["t", "state"]);
	if (value.t !== "popup.state") throw new LocalMessageError("t: expected popup.state");
	const state = value.state;
	if (!isRecord(state)) throw new LocalMessageError("state: expected an object");
	expectKeys(
		state,
		["phase", "requested", "granted", "tab", "observationId", "error"],
		["phase", "requested", "granted"],
	);
	const phase = state.phase;
	if (phase !== "detached" && phase !== "attaching" && phase !== "attached") {
		throw new LocalMessageError("phase: unknown phase");
	}
	const parsed: PopupState = {
		phase,
		requested: expectCapabilities(state.requested, "requested"),
		granted: Array.isArray(state.granted) ? expectCapabilitiesOrEmpty(state.granted, "granted") : [],
	};
	if (state.tab !== undefined) {
		if (!isRecord(state.tab)) throw new LocalMessageError("tab: expected an object");
		expectKeys(state.tab, ["tabId", "title", "url"], ["tabId"]);
		const tab: { tabId: number; title?: string; url?: string } = {
			tabId: expectInt(state.tab.tabId, "tab.tabId", 0, Number.MAX_SAFE_INTEGER),
		};
		if (state.tab.title !== undefined) tab.title = expectString(state.tab.title, "tab.title", 200, true);
		if (state.tab.url !== undefined) tab.url = expectString(state.tab.url, "tab.url", 512, true);
		parsed.tab = tab;
	}
	if (state.observationId !== undefined) parsed.observationId = expectId(state.observationId, "observationId");
	if (state.error !== undefined) {
		if (!isRecord(state.error)) throw new LocalMessageError("error: expected an object");
		expectKeys(state.error, ["code", "message", "retryable"], ["code", "message", "retryable"]);
		parsed.error = {
			code: expectString(state.error.code, "error.code", 64) as WireErrorCode,
			message: expectString(state.error.message, "error.message", MAX_LOCAL_MESSAGE_CHARS, true),
			retryable: state.error.retryable === true,
		};
	}
	return parsed;
}

function expectCapabilitiesOrEmpty(value: unknown, key: string): Capability[] {
	if (!Array.isArray(value)) throw new LocalMessageError(`${key}: expected capabilities`);
	if (value.length === 0) return [];
	return expectCapabilities(value, key);
}
