/**
 * Injected content-script entry (top frame, isolated world).
 *
 * Wiring only. It owns the document-local reference table lifetime, the local message loop with the
 * worker and the mapping from a validated local request to the collector or the action module. It never
 * creates authority: the worker supplies the binding, the observation id and the capability decision.
 *
 * Local failures send a closed code plus one generic sentence. DOM exceptions, element names, page ids
 * and page text never appear in an error string.
 */
import { MAX_ID_CHARS } from "../../src/protocol/limits.ts";
import type { Binding, EffectOutcome, NaviErrorCode } from "../../src/protocol/types.ts";
import { chromeRuntimePort } from "../platform/chrome.ts";
import type { RuntimePortApi } from "../platform/types.ts";
import {
	CONTENT_PORT_NAME,
	type ContentFailureCode,
	type ContentInbound,
	type ContentOutbound,
	parseContentInbound,
} from "../worker/local-channel.ts";
import { ActionError, focusElement, scrollPage } from "./actions.ts";
import { collectObservation } from "./collector.ts";
import { createRefStore } from "./refs.ts";

/** Codes the worker accepts from a content script. Anything else is reported as INTERNAL. */
const REPORTABLE_CODES: readonly ContentFailureCode[] = [
	"CAPTURE_FAILED",
	"TOO_LARGE",
	"UNKNOWN_REF",
	"BAD_ARGUMENT",
	"TAB_NOT_ACTIVE",
	"CANCELLED",
	"OUTCOME_UNKNOWN",
	"INTERNAL",
];

const GENERIC_MESSAGES: Record<ContentFailureCode, string> = {
	CAPTURE_FAILED: "the page could not be read",
	TOO_LARGE: "the page result exceeded the size budget",
	UNKNOWN_REF: "the observed node is no longer usable",
	BAD_ARGUMENT: "the requested action is not allowed",
	TAB_NOT_ACTIVE: "the page is not focused and visible",
	CANCELLED: "the request was cancelled",
	OUTCOME_UNKNOWN: "the outcome is unknown",
	INTERNAL: "the page operation failed",
};

const refs = createRefStore();
let channel: RuntimePortApi | null = null;

function reportableCode(code: NaviErrorCode): ContentFailureCode {
	return REPORTABLE_CODES.includes(code as ContentFailureCode) ? (code as ContentFailureCode) : "INTERNAL";
}

function fail(requestId: string, code: ContentFailureCode): void {
	send({ t: "content.failure", requestId, code, message: GENERIC_MESSAGES[code] });
}

function send(message: ContentOutbound): void {
	try {
		channel?.postMessage(message);
	} catch {
		// The worker is gone; authority ends on its side.
	}
}

function handleObserve(request: { requestId: string; observationId: string; binding: Binding }): void {
	if (request.observationId.length === 0 || request.observationId.length > MAX_ID_CHARS) {
		fail(request.requestId, "BAD_ARGUMENT");
		return;
	}
	const result = collectObservation({ observationId: request.observationId, binding: request.binding });
	if (!result.ok) {
		fail(request.requestId, reportableCode(result.code));
		return;
	}
	// The worker re-stamps its own binding, so only page-derived data crosses this channel.
	const { binding: _binding, ...draft } = result.observation;
	send({ t: "content.observation", requestId: request.requestId, draft });
}

type ActionOutcome = { outcome: EffectOutcome; scrollX?: number; scrollY?: number };

function handleAction(requestId: string, run: () => ActionOutcome): void {
	let result: ActionOutcome;
	try {
		result = run();
	} catch (error) {
		fail(requestId, reportableCode(error instanceof ActionError ? error.code : "INTERNAL"));
		return;
	}
	const message: ContentOutbound = { t: "content.action", requestId, outcome: result.outcome };
	if (result.scrollX !== undefined) message.scrollX = result.scrollX;
	if (result.scrollY !== undefined) message.scrollY = result.scrollY;
	send(message);
}

function handleScroll(request: Extract<ContentInbound, { t: "content.scroll" }>): void {
	handleAction(request.requestId, () => {
		const hasDelta = request.deltaX !== undefined || request.deltaY !== undefined;
		if (request.ref !== undefined) {
			if (hasDelta) throw new ActionError("BAD_ARGUMENT", "A reference scroll cannot include a delta");
			const element = refs.get(request.ref);
			if (!element) throw new ActionError("UNKNOWN_REF", "The observed node is no longer usable");
			return scrollPage({ element, align: request.align });
		}
		return scrollPage({ deltaX: request.deltaX, deltaY: request.deltaY });
	});
}

function handleMessage(value: unknown): void {
	let request: ContentInbound;
	try {
		request = parseContentInbound(value);
	} catch {
		// An unrecognized local message is ignored: it cannot change page state.
		return;
	}
	switch (request.t) {
		case "content.observe":
			handleObserve(request);
			return;
		case "content.focus":
			handleAction(request.requestId, () => {
				const element = refs.get(request.ref);
				if (!element) throw new ActionError("UNKNOWN_REF", "The observed node is no longer usable");
				return { outcome: focusElement(element) };
			});
			return;
		case "content.scroll":
			handleScroll(request);
			return;
		case "content.cancel":
			// Focus and scroll are synchronous, so a cancel always arrives after the answer.
			return;
		case "content.detach":
			refs.revoke();
			channel?.disconnect();
			return;
	}
}

function main(): void {
	const global = globalThis as { __naviContentPort?: RuntimePortApi };
	// Re-attaching in the same document must not leave a second port alive.
	global.__naviContentPort?.disconnect();
	channel = chromeRuntimePort(CONTENT_PORT_NAME);
	global.__naviContentPort = channel;
	channel.onMessage.addListener((value) => handleMessage(value));
	channel.onDisconnect.addListener(() => {
		// Worker restart, navigation or detach: every reference is invalid.
		refs.revoke();
		channel = null;
	});
	send({ t: "content.hello" });
	// A restored document keeps the port but invalidates every reference.
	globalThis.addEventListener("pageshow", (event) => {
		if (event.persisted === true) send({ t: "content.navigated" });
	});
	globalThis.addEventListener("pagehide", () => {
		refs.revoke();
	});
}

main();
