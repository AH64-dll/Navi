import { MAX_SCROLL_DELTA } from "../../src/protocol/limits.ts";
import type { EffectOutcome, NaviErrorCode, ScrollAlign } from "../../src/protocol/types.ts";

export class ActionError extends Error {
	readonly code: NaviErrorCode;
	constructor(code: NaviErrorCode, message: string) {
		super(message);
		this.name = "ActionError";
		this.code = code;
	}
}

const EXCLUDED_TAGS = new Set([
	"INPUT",
	"TEXTAREA",
	"SELECT",
	"OPTION",
	"SCRIPT",
	"STYLE",
	"IFRAME",
	"OBJECT",
	"EMBED",
]);
export { MAX_SCROLL_DELTA };

function activeDocument(doc: Document, signal?: AbortSignal): void {
	if (signal?.aborted) throw new ActionError("CANCELLED", "Action cancelled before dispatch");
	if (doc.visibilityState !== "visible" || !doc.hasFocus()) {
		throw new ActionError("TAB_NOT_ACTIVE", "The attached document is not focused and visible");
	}
}

/** Recheck the retained element, not a selector. Page mutations can invalidate an observed target. */
function safeElement(element: Element): void {
	if (!element.isConnected || element.getRootNode() !== element.ownerDocument) {
		throw new ActionError("UNKNOWN_REF", "The observed node is no longer in the document");
	}
	let ancestor: Element | null = element;
	for (let depth = 0; ancestor !== null; depth += 1) {
		if (depth >= 64) throw new ActionError("UNKNOWN_REF", "Target ancestry exceeds the action limit");
		const autocomplete = ancestor.getAttribute("autocomplete") ?? "";
		const editable = ancestor.getAttribute("contenteditable");
		if (
			EXCLUDED_TAGS.has(ancestor.tagName.toUpperCase()) ||
			ancestor.hasAttribute("hidden") ||
			ancestor.hasAttribute("inert") ||
			ancestor.hasAttribute("disabled") ||
			ancestor.getAttribute("aria-hidden") === "true" ||
			ancestor.getAttribute("aria-disabled") === "true" ||
			(editable !== null && editable !== "false") ||
			/(?:^|\s)(?:one-time-code|current-password|new-password|cc-[a-z-]+)(?:\s|$)/i.test(autocomplete)
		)
			throw new ActionError("UNKNOWN_REF", "Target is excluded from actions");
		const view = ancestor.ownerDocument.defaultView;
		if (!view) throw new ActionError("UNKNOWN_REF", "Target has no live browser view");
		const style = view.getComputedStyle(ancestor);
		if (
			style.display === "none" ||
			style.visibility === "hidden" ||
			style.visibility === "collapse" ||
			style.opacity === "0"
		) {
			throw new ActionError("UNKNOWN_REF", "Target is hidden");
		}
		ancestor = ancestor.parentElement;
	}
}

/** Focus can trigger site handlers. A dispatched effect is never retried or claimed to be undone. */
export function focusElement(element: Element, signal?: AbortSignal): EffectOutcome {
	const doc = element.ownerDocument;
	activeDocument(doc, signal);
	safeElement(element);
	const focusable = element as Element & { focus?: (options: FocusOptions) => void };
	const tag = element.tagName.toUpperCase();
	const tabIndex = element.getAttribute("tabindex");
	const eligible =
		tag === "BUTTON" ||
		tag === "SUMMARY" ||
		((tag === "A" || tag === "AREA") && element.hasAttribute("href")) ||
		(tabIndex !== null && /^-?\d+$/.test(tabIndex));
	if (!eligible || typeof focusable.focus !== "function") {
		throw new ActionError("UNKNOWN_REF", "Target is not an eligible focus target");
	}
	activeDocument(doc, signal);
	try {
		focusable.focus({ preventScroll: true });
		return signal?.aborted || doc.activeElement !== element ? "unknown" : "applied";
	} catch {
		return "unknown";
	}
}

export type ScrollPageInput = {
	element?: Element;
	align?: ScrollAlign;
	deltaX?: number;
	deltaY?: number;
	/** Test seam; the content entry uses its own document, never a wire-supplied object. */
	document?: Document;
};

export function scrollPage(
	input: ScrollPageInput,
	signal?: AbortSignal,
): {
	outcome: EffectOutcome;
	scrollX?: number;
	scrollY?: number;
} {
	const doc = input.document ?? input.element?.ownerDocument ?? globalThis.document;
	activeDocument(doc, signal);
	const view = doc.defaultView;
	if (!view) throw new ActionError("UNKNOWN_REF", "Document has no live browser view");
	const hasDelta = input.deltaX !== undefined || input.deltaY !== undefined;
	if (input.element) {
		if (input.element.ownerDocument !== doc || hasDelta) {
			throw new ActionError("BAD_ARGUMENT", "Reference scroll cannot include a delta or another document");
		}
		safeElement(input.element);
	} else if (!hasDelta || input.align !== undefined) {
		throw new ActionError("BAD_ARGUMENT", "Page scroll requires a bounded delta");
	}
	const x = input.deltaX ?? 0;
	const y = input.deltaY ?? 0;
	if (![x, y].every((value) => Number.isFinite(value) && Math.abs(value) <= MAX_SCROLL_DELTA)) {
		throw new ActionError("BAD_ARGUMENT", "Scroll delta exceeds the action limit");
	}
	activeDocument(doc, signal);
	try {
		if (input.element)
			input.element.scrollIntoView({ behavior: "instant", block: input.align ?? "center", inline: "nearest" });
		else view.scrollBy({ left: x, top: y, behavior: "instant" });
		if (signal?.aborted) return { outcome: "unknown" };
		return { outcome: "applied", scrollX: view.scrollX, scrollY: view.scrollY };
	} catch {
		return { outcome: "unknown" };
	}
}
