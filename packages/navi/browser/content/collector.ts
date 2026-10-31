/**
 * Bounded DOM/ARIA observation collector for the NAVI content script.
 *
 * What this is: a requested, bounded, single-pass projection of the granted top-frame document, derived
 * from DOM structure, attributes and a reduced name computation. It is NOT Chrome's computed
 * accessibility tree, and `chrome.debugger` is not used.
 *
 * What it is not: no mutation observer, no polling, no screenshots, no input values, no selection, no
 * cookies or storage, no child-frame or shadow-root capture, no page-supplied identifiers as handles.
 *
 * Exclusions are decided before any descendant text is read, and text is gathered by walking child nodes
 * while skipping excluded subtrees. `textContent` / `innerText` / `value` are never read, because they
 * would pull text out of a hidden or value-bearing descendant.
 *
 * Everything returned is untrusted page data and must stay labelled as such on the wire.
 *
 * DOM access only. No `chrome` API, no Node API, no dynamic import, no module side effects, so the
 * extension packager can bundle this module into the single injected content script.
 */
import {
	MAX_NODES,
	MAX_OBSERVATION_BYTES,
	MAX_TEXT_CHARS,
	MAX_TITLE_CHARS,
	MAX_URL_CHARS,
	MAX_VISITED_ELEMENTS,
} from "../../src/protocol/limits.ts";
import type { Binding, DomNode, DomRect, Observation, OmittedRegion } from "../../src/protocol/types.ts";
import { createRefTable, type RefTable } from "./refs.ts";

/** Wall-clock safety valve for one capture, in milliseconds. The visit budget is the primary bound. */
export const COLLECT_TIME_BUDGET_MS = 250;

/** Reserved space for the fields the worker adds around this payload inside the observation budget. */
const OBSERVATION_HEADROOM_BYTES = 512;

/**
 * C0 and C1 controls, DEL, and bidi overrides/isolates. Mirrors the unreferenced pattern in
 * `src/protocol/validate.ts`, which rejects these characters in ids, URLs, `title` and error messages.
 * Page `name`/`text` are exempt there, so they are not filtered here either.
 */
const FORBIDDEN_TEXT_CHARACTERS = /[\u0000-\u001F\u007F-\u009F\u202A-\u202E\u2066-\u2069]/g;

/** Mirrors `TAG_PATTERN` and `ROLE_PATTERN` in `src/protocol/validate.ts`, including the 64-char cap. */
const TAG_PATTERN = /^[a-z][a-z0-9-]*$/;
const ROLE_PATTERN = /^[a-z][a-z-]*$/;
const MAX_TOKEN_CHARS = 64;

/** Largest absolute rect offset and size accepted by the protocol validator. */
const MAX_RECT_EXTENT = 1_000_000;

/** Bounds for one capture. Element-policy evaluations and text-node reads are globally capped. */
const MAX_TEXT_NODE_READS = 4000;
const MAX_ELEMENT_CHECKS = 6000;
const MAX_LABELLEDBY_TARGETS = 3;
const MAX_LABEL_TARGETS = 2;
const MAX_ANCESTRY_DEPTH = 16;
const TEXT_SAMPLE_DEPTH = 8;
const TIME_CHECK_INTERVAL = 64;

/** Tags that never contribute text or a node. */
const SKIPPED_TAGS = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "HEAD", "META", "LINK", "BASE", "TITLE"]);

/** Tags that are skipped as nodes but reported as omitted coverage. */
const COVERAGE_TAGS: Record<string, OmittedRegion> = {
	IFRAME: "subframes",
	FRAME: "subframes",
	FRAMESET: "subframes",
	CANVAS: "canvas",
};

/** Form content that is skipped entirely rather than reflected as a node. */
const FORM_CONTENT_TAGS = new Set(["OPTION", "OPTGROUP", "DATALIST", "OUTPUT"]);

/** Autocomplete tokens that mark a field as secret. Matching fields are never emitted or read. */
const SECRET_AUTOCOMPLETE_TOKENS = new Set([
	"one-time-code",
	"current-password",
	"new-password",
	"cc-number",
	"cc-name",
	"cc-given-name",
	"cc-additional-name",
	"cc-family-name",
	"cc-exp",
	"cc-exp-month",
	"cc-exp-year",
	"cc-csc",
	"cc-type",
]);

/** Tag-derived roles. A page-authored `role` attribute wins when present. */
const TAG_ROLES: Record<string, string> = {
	A: "link",
	BUTTON: "button",
	SELECT: "combobox",
	TEXTAREA: "textbox",
	SUMMARY: "button",
	IMG: "img",
	NAV: "navigation",
	MAIN: "main",
	HEADER: "banner",
	FOOTER: "contentinfo",
	ASIDE: "complementary",
	FORM: "form",
	TABLE: "table",
	UL: "list",
	OL: "list",
	LI: "listitem",
	DIALOG: "dialog",
};

const INPUT_TYPE_ROLES: Record<string, string> = {
	button: "button",
	submit: "button",
	reset: "button",
	checkbox: "checkbox",
	radio: "radio",
	search: "searchbox",
	range: "slider",
	number: "spinbutton",
	email: "textbox",
	tel: "textbox",
	text: "textbox",
	url: "textbox",
};

const INTERACTIVE_ROLES = new Set([
	"button",
	"link",
	"textbox",
	"searchbox",
	"checkbox",
	"radio",
	"combobox",
	"listbox",
	"menuitem",
	"menuitemcheckbox",
	"menuitemradio",
	"option",
	"slider",
	"spinbutton",
	"switch",
	"tab",
]);

type ElementPolicy =
	| { kind: "skip"; omitted?: OmittedRegion }
	| { kind: "emit"; skipSubtree: boolean; textAllowed: boolean; editable: boolean; omitted?: OmittedRegion };

const SKIP_POLICY: ElementPolicy = { kind: "skip" };

type EmittedPolicy = Extract<ElementPolicy, { kind: "emit" }>;

type CollectorState = {
	visited: number;
	elementChecks: number;
	textNodeReads: number;
	exhausted: boolean;
};

export type CollectRequest = {
	/** Worker-supplied observation id. Used only to scope the returned reference table. */
	observationId: string;
	/** Browser-supplied identity. Copied through; this module never derives or validates authority. */
	binding: Binding;
	/** Injected content scripts default to the real document; tests pass a fixture. */
	document?: Document;
	/** Injectable clock for the deadline valve. */
	now?: () => number;
};

export type CollectResult =
	| { ok: true; observation: Observation; refs: RefTable; bytes: number }
	| { ok: false; code: "CAPTURE_FAILED" | "TOO_LARGE"; message: string };

type Candidate = { node: DomNode; element: Element; tier: number; order: number };

const textEncoder = typeof TextEncoder === "function" ? new TextEncoder() : undefined;

/** UTF-8 byte length without depending on Buffer or Node APIs. */
function byteLength(value: string): number {
	if (textEncoder) return textEncoder.encode(value).length;
	let bytes = 0;
	for (const character of value) {
		const codePoint = character.codePointAt(0) ?? 0;
		bytes += codePoint < 0x80 ? 1 : codePoint < 0x800 ? 2 : codePoint < 0x10000 ? 3 : 4;
	}
	return bytes;
}

function collapse(value: string): string {
	return value.replace(/\s+/g, " ").trim();
}

/**
 * Cut to a UTF-16 length limit without splitting a surrogate pair. The protocol compares
 * `string.length`, so the limit is counted in UTF-16 units, not code points.
 */
function cutAt(value: string, limit: number): string {
	let end = value.length <= limit ? value.length : limit;
	if (end > 0) {
		const last = value.charCodeAt(end - 1);
		if (last >= 0xd800 && last <= 0xdbff) end -= 1;
	}
	return value.slice(0, end);
}

/** Collapse whitespace and cut to the shared character limit. */
function clampText(value: string, limit = MAX_TEXT_CHARS): string {
	return cutAt(collapse(value), limit);
}

/** Runtime freeze that keeps the declared type. All returned observation parts are frozen. */
function deepFreeze<T>(value: T): T {
	if (value !== null && typeof value === "object") Object.freeze(value);
	return value;
}

/**
 * Strip credentials, query and fragment. The collector returns origin plus path only; full navigation
 * identity stays in worker state. Non-HTTP(S) documents return an empty string.
 */
export function sanitizeUrl(raw: string): string {
	const trimmed = raw.trim();
	if (trimmed.length === 0) return "";
	let parsed: URL;
	try {
		parsed = new URL(trimmed);
	} catch {
		return "";
	}
	if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return "";
	const path = parsed.pathname.length > 0 ? parsed.pathname : "/";
	// Canonical origin plus path, with control and bidi characters removed so the receiver accepts it.
	const sanitized = stripForbiddenCharacters(`${parsed.origin}${path}`);
	return sanitized.length > MAX_URL_CHARS ? sanitized.slice(0, MAX_URL_CHARS) : sanitized;
}

function attribute(element: Element, name: string): string | null {
	try {
		return element.getAttribute(name);
	} catch {
		return null;
	}
}

function hasAttribute(element: Element, name: string): boolean {
	try {
		return element.hasAttribute(name);
	} catch {
		return false;
	}
}

function stripForbiddenCharacters(value: string): string {
	return value.replace(FORBIDDEN_TEXT_CHARACTERS, "");
}

/** Lowercase element name that satisfies the protocol tag pattern; otherwise a bounded placeholder. */
function safeTag(element: Element): string {
	const tag = element.localName.toLowerCase();
	return tag.length <= MAX_TOKEN_CHARS && TAG_PATTERN.test(tag) ? tag : "unknown";
}

function spendCheck(state: CollectorState): boolean {
	state.elementChecks += 1;
	if (state.elementChecks > MAX_ELEMENT_CHECKS) {
		state.exhausted = true;
		return false;
	}
	return true;
}

/** Hidden elements and their subtrees are dropped before anything inside them is inspected. */
function isHidden(element: Element): boolean {
	if (hasAttribute(element, "hidden")) return true;
	if (attribute(element, "aria-hidden") === "true") return true;
	const checkable = element as Element & { checkVisibility?: (options?: { checkVisibilityCSS?: boolean }) => boolean };
	if (typeof checkable.checkVisibility === "function") {
		try {
			return checkable.checkVisibility({ checkVisibilityCSS: true }) === false;
		} catch {
			// A page can make this throw; fall through to the computed-style decision below.
		}
	}
	const view = element.ownerDocument?.defaultView ?? null;
	if (!view || typeof view.getComputedStyle !== "function") return false;
	try {
		const style = view.getComputedStyle(element);
		return style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse";
	} catch {
		return false;
	}
}

function isSecretAutocomplete(value: string | null): boolean {
	if (!value) return false;
	return value
		.toLowerCase()
		.split(/\s+/)
		.some((token) => SECRET_AUTOCOMPLETE_TOKENS.has(token));
}

function isContentEditable(element: Element): boolean {
	const value = attribute(element, "contenteditable");
	if (value !== null && value.toLowerCase() !== "false") return true;
	return (element as Element & { isContentEditable?: boolean }).isContentEditable === true;
}

function elementPolicy(element: Element, state: CollectorState): ElementPolicy {
	if (!spendCheck(state)) return SKIP_POLICY;
	const tag = element.localName.toUpperCase();
	const coverage = COVERAGE_TAGS[tag];
	if (coverage !== undefined) return { kind: "skip", omitted: coverage };
	if (SKIPPED_TAGS.has(tag) || FORM_CONTENT_TAGS.has(tag)) return SKIP_POLICY;
	if (isHidden(element)) return SKIP_POLICY;
	if (tag === "INPUT") {
		const type = (attribute(element, "type") ?? "text").toLowerCase();
		if (type === "hidden" || type === "password" || type === "file" || type === "image") return SKIP_POLICY;
		if (isSecretAutocomplete(attribute(element, "autocomplete"))) return SKIP_POLICY;
		return { kind: "emit", skipSubtree: true, textAllowed: false, editable: true };
	}
	if (tag === "TEXTAREA" || tag === "SELECT") {
		return { kind: "emit", skipSubtree: true, textAllowed: false, editable: true };
	}
	if (isContentEditable(element) || hasAttribute(element, "contenteditable")) {
		// The editor content is user text: report the node, never read inside it.
		return { kind: "emit", skipSubtree: true, textAllowed: false, editable: true };
	}
	const shadow = element.shadowRoot;
	if (shadow !== null && shadow !== undefined) {
		// The open shadow root is not traversed; closed roots are undetectable and stay unreported.
		return { kind: "emit", skipSubtree: false, textAllowed: true, editable: false, omitted: "shadow" };
	}
	return { kind: "emit", skipSubtree: false, textAllowed: true, editable: false };
}

/** True when text must not be read from this element. Fail-closed. */
function excludesText(element: Element, state: CollectorState): boolean {
	const policy = elementPolicy(element, state);
	return policy.kind === "skip" || !policy.textAllowed;
}

/** True when this element and every ancestor may contribute text. Used before reading a label target. */
function ancestryAllowsText(element: Element, state: CollectorState): boolean {
	let current: Element | null = element;
	for (let depth = 0; current && depth < MAX_ANCESTRY_DEPTH; depth += 1) {
		const policy = elementPolicy(current, state);
		if (policy.kind === "skip" || !policy.textAllowed) return false;
		current = current.parentElement;
	}
	return true;
}

/**
 * Bounded descendant text sample. Walks child nodes and skips excluded subtrees, so a hidden or
 * value-bearing descendant cannot leak through an ancestor's text. Each text-node read is sliced to the
 * remaining character budget, so one huge text node cannot force a large allocation.
 */
function sampleText(root: Element, depthLimit: number, state: CollectorState): string {
	const parts: string[] = [];
	let length = 0;
	const stack: Array<{ element: Element; depth: number; index: number }> = [{ element: root, depth: 0, index: 0 }];
	while (stack.length > 0) {
		const frame = stack[stack.length - 1]!;
		const childNodes = frame.element.childNodes;
		if (frame.index >= childNodes.length) {
			stack.pop();
			continue;
		}
		const node = childNodes[frame.index]!;
		frame.index += 1;
		if (node.nodeType === 3) {
			state.textNodeReads += 1;
			const data = typeof (node as CharacterData).data === "string" ? (node as CharacterData).data : "";
			const remaining = MAX_TEXT_CHARS - length;
			if (data.length > 0 && remaining > 0) {
				const piece = cutAt(data, remaining);
				if (piece.length > 0) {
					parts.push(piece);
					length += piece.length;
				}
			}
			if (length >= MAX_TEXT_CHARS || state.textNodeReads >= MAX_TEXT_NODE_READS) break;
			continue;
		}
		if (node.nodeType !== 1 || frame.depth >= depthLimit) continue;
		const child = node as Element;
		if (excludesText(child, state)) continue;
		stack.push({ element: child, depth: frame.depth + 1, index: 0 });
	}
	return collapse(parts.join(" "));
}

/**
 * First safe token of the declared role. ARIA allows a space-separated fallback list, and only the first
 * token that satisfies the protocol role pattern is used; otherwise the tag mapping decides.
 */
function deriveRole(element: Element): string | undefined {
	const declared = attribute(element, "role");
	if (declared !== null) {
		for (const token of declared.toLowerCase().split(/\s+/)) {
			if (token.length <= MAX_TOKEN_CHARS && ROLE_PATTERN.test(token)) return token;
		}
	}
	const tag = element.localName.toUpperCase();
	if (tag === "INPUT") return INPUT_TYPE_ROLES[(attribute(element, "type") ?? "text").toLowerCase()];
	if (/^H[1-6]$/.test(tag)) return "heading";
	return TAG_ROLES[tag];
}

function isHeading(element: Element): boolean {
	if (/^H[1-6]$/.test(element.localName)) return true;
	return attribute(element, "role") === "heading";
}

/**
 * Reduced name computation: page-authored labels only, never values and never the content of a secret or
 * editable element. Every label target is tested for hidden or excluded ancestry before it is read.
 */
function deriveName(element: Element, policy: EmittedPolicy, state: CollectorState): string | undefined {
	const doc = element.ownerDocument;
	const labelledBy = attribute(element, "aria-labelledby");
	if (labelledBy !== null && doc) {
		const parts: string[] = [];
		const ids = labelledBy
			.split(/\s+/)
			.filter((entry) => entry.length > 0)
			.slice(0, MAX_LABELLEDBY_TARGETS);
		for (const id of ids) {
			let target: Element | null = null;
			try {
				target = doc.getElementById(id);
			} catch {
				target = null;
			}
			if (!target || target === element) continue;
			if (!ancestryAllowsText(target, state)) continue;
			const text = sampleText(target, 2, state);
			if (text.length > 0) parts.push(text);
		}
		const joined = clampText(parts.join(" "));
		if (joined.length > 0) return joined;
	}
	const ariaLabel = attribute(element, "aria-label");
	if (ariaLabel !== null && collapse(ariaLabel).length > 0) return clampText(ariaLabel);
	const labels = (element as Element & { labels?: ArrayLike<Element> | null }).labels;
	if (labels) {
		for (const label of Array.from(labels).slice(0, MAX_LABEL_TARGETS)) {
			if (!ancestryAllowsText(label, state)) continue;
			const text = sampleText(label, 2, state);
			if (text.length > 0) return clampText(text);
		}
	}
	if (typeof element.closest === "function") {
		const wrapper = element.closest("label");
		if (wrapper && ancestryAllowsText(wrapper, state)) {
			const text = sampleText(wrapper, 2, state);
			if (text.length > 0) return clampText(text);
		}
	}
	const title = attribute(element, "title");
	if (title !== null && collapse(title).length > 0) return clampText(title);
	if (element.localName === "img") {
		const alt = attribute(element, "alt");
		if (alt !== null && collapse(alt).length > 0) return clampText(alt);
	}
	if (policy.textAllowed) {
		const text = sampleText(element, 3, state);
		if (text.length > 0) return clampText(text);
	}
	if (element.localName === "input") {
		const type = (attribute(element, "type") ?? "text").toLowerCase();
		if (["search", "text", "email", "url", "tel"].includes(type)) {
			const placeholder = attribute(element, "placeholder");
			if (placeholder !== null && collapse(placeholder).length > 0) return clampText(placeholder);
		}
	}
	return undefined;
}

/** Focusability from DOM and attribute facts only. Disabled controls are not focus targets. */
export function isFocusable(element: Element): boolean {
	if (hasAttribute(element, "disabled")) return false;
	if (hasAttribute(element, "tabindex")) return true;
	const tag = element.localName.toUpperCase();
	if (tag === "A" || tag === "AREA") return hasAttribute(element, "href");
	if (tag === "INPUT" || tag === "BUTTON" || tag === "SELECT" || tag === "TEXTAREA" || tag === "SUMMARY") return true;
	if (hasAttribute(element, "contenteditable")) return true;
	return (element as Element & { isContentEditable?: boolean }).isContentEditable === true;
}

function isInteractive(element: Element): boolean {
	if (isFocusable(element)) return true;
	const role = attribute(element, "role");
	if (role === null) return false;
	return INTERACTIVE_ROLES.has(collapse(role).toLowerCase());
}

function viewportOf(doc: Document): { w: number; h: number } | undefined {
	const view = doc.defaultView;
	if (view && Number.isFinite(view.innerWidth) && Number.isFinite(view.innerHeight)) {
		return { w: view.innerWidth, h: view.innerHeight };
	}
	const root = doc.documentElement;
	if (!root) return undefined;
	const width = root.clientWidth;
	const height = root.clientHeight;
	if (!Number.isFinite(width) || !Number.isFinite(height) || width === 0 || height === 0) return undefined;
	return { w: width, h: height };
}

/** Viewport geometry is read only for surviving nodes, so the layout cost stays bounded. */
function measureGeometry(
	element: Element,
	viewport: { w: number; h: number } | undefined,
): { rect?: DomRect; offscreen: boolean } {
	const getBounds = (element as Element).getBoundingClientRect;
	if (typeof getBounds !== "function") return { offscreen: false };
	let bounds: DOMRect;
	try {
		bounds = getBounds.call(element);
	} catch {
		return { offscreen: false };
	}
	const left = Number(bounds.left);
	const top = Number(bounds.top);
	const width = Number(bounds.width);
	const height = Number(bounds.height);
	if (![left, top, width, height].every((value) => Number.isFinite(value))) return { offscreen: false };
	const offset = (value: number): number => Math.min(MAX_RECT_EXTENT, Math.max(-MAX_RECT_EXTENT, Math.round(value)));
	const extent = (value: number): number => Math.min(MAX_RECT_EXTENT, Math.max(0, Math.round(value)));
	const rect: DomRect = { x: offset(left), y: offset(top), w: extent(width), h: extent(height) };
	if (!viewport) return { rect, offscreen: false };
	const offscreen = top + height <= 0 || left + width <= 0 || top >= viewport.h || left >= viewport.w;
	return { rect, offscreen };
}

function ownDocumentUrl(doc: Document): string {
	const location = doc.location;
	const href = typeof location?.href === "string" ? location.href : "";
	if (href.length > 0) return href;
	return typeof doc.URL === "string" ? doc.URL : "";
}

/**
 * Collect one bounded observation of the granted top-frame document.
 *
 * Priority when more elements qualify than the node budget allows: focus targets first, then
 * headings/landmarks/role-bearing elements, then other text-bearing elements. Output stays in document
 * order, so identical input produces byte-identical output.
 */
export function collectObservation(request: CollectRequest): CollectResult {
	const doc =
		request.document ??
		(typeof globalThis.document === "undefined" ? undefined : (globalThis.document as unknown as Document));
	if (!doc) return { ok: false, code: "CAPTURE_FAILED", message: "no document available" };
	const root = doc.body ?? doc.documentElement;
	if (!root) return { ok: false, code: "CAPTURE_FAILED", message: "no document root" };
	// Only http(s) documents have a canonical display URL. An unsupported document is a refusal, not an
	// observation with an empty url, because the receiver rejects an empty or non-http(s) url.
	const url = sanitizeUrl(ownDocumentUrl(doc));
	if (url.length === 0) return { ok: false, code: "CAPTURE_FAILED", message: "unsupported document url" };

	const now = request.now ?? (() => (typeof performance === "undefined" ? Date.now() : performance.now()));
	const deadline = now() + COLLECT_TIME_BUDGET_MS;
	const state: CollectorState = { visited: 0, elementChecks: 0, textNodeReads: 0, exhausted: false };
	const omitted = new Set<OmittedRegion>();
	const candidates: Candidate[] = [];
	const viewport = viewportOf(doc);

	// An excluded or text-forbidding root stops the whole capture: walking its children would read text
	// out of a subtree that was excluded on purpose.
	const rootPolicy = elementPolicy(root, state);
	if (rootPolicy.omitted !== undefined) omitted.add(rootPolicy.omitted);
	let truncated = rootPolicy.kind === "skip" || rootPolicy.skipSubtree;

	let order = 0;
	const stack: Array<{ element: Element; depth: number; index: number }> = [{ element: root, depth: 0, index: 0 }];
	while (!truncated && stack.length > 0) {
		if (state.visited % TIME_CHECK_INTERVAL === 0 && now() > deadline) {
			// The frozen OmittedRegion set has no time value; `visited` plus `truncated` carries the fact.
			truncated = true;
			break;
		}
		const frame = stack[stack.length - 1]!;
		const children = frame.element.children;
		if (frame.index >= children.length) {
			stack.pop();
			continue;
		}
		const next = children[frame.index]!;
		frame.index += 1;
		if (!next) continue;
		if (state.visited >= MAX_VISITED_ELEMENTS) {
			truncated = true;
			omitted.add("node-limit");
			break;
		}
		state.visited += 1;
		const policy = elementPolicy(next, state);
		if (policy.kind === "skip") {
			if (policy.omitted !== undefined) omitted.add(policy.omitted);
			continue;
		}
		if (policy.omitted !== undefined) omitted.add(policy.omitted);

		const interactive = isInteractive(next);
		const role = deriveRole(next);
		const heading = isHeading(next);
		const text = policy.textAllowed ? clampText(sampleText(next, TEXT_SAMPLE_DEPTH, state)) : "";
		const tier = interactive ? 1 : heading || role !== undefined ? 2 : text.length > 0 ? 3 : 0;
		if (tier > 0) {
			const node: DomNode = {
				ref: "",
				// The protocol bounds depth to MAX_NODES; deeper nodes report the bound.
				depth: Math.min(frame.depth + 1, MAX_NODES),
				tag: safeTag(next),
				interactive,
				editable: policy.editable,
				offscreen: false,
			};
			if (role !== undefined) node.role = role;
			if (interactive || heading || role !== undefined) {
				const name = deriveName(next, policy, state);
				if (name !== undefined) node.name = name;
			}
			if (text.length > 0) node.text = text;
			candidates.push({ node, element: next, tier, order: order++ });
		}

		if (policy.skipSubtree) continue;
		stack.push({ element: next, depth: frame.depth + 1, index: 0 });
	}

	if (state.exhausted) truncated = true;

	const ranked = candidates.slice().sort((left, right) => left.tier - right.tier || left.order - right.order);
	const selected = ranked.slice(0, MAX_NODES);
	if (ranked.length > selected.length) {
		truncated = true;
		omitted.add("node-limit");
	}
	selected.sort((left, right) => left.order - right.order);

	const title = clampText(stripForbiddenCharacters(doc.title ?? ""), MAX_TITLE_CHARS);
	const observation: Observation = {
		observationId: request.observationId,
		binding: request.binding,
		// The protocol stores capturedAt as a safe integer, so a fractional monotonic clock is rounded.
		capturedAt: Math.max(0, Math.round(now())),
		url,
		nodes: selected.map((candidate) => candidate.node),
		visited: state.visited,
		truncated,
		omitted: [],
	};
	if (title.length > 0) observation.title = title;

	// Handle and geometry fields are part of the payload, so they are assigned before the byte budget is
	// measured. Refs run in document order, so trimming the tail leaves exactly n1..nK.
	const draftRefs = createRefTable(request.observationId);
	for (const candidate of selected) {
		const geometry = measureGeometry(candidate.element, viewport);
		candidate.node.offscreen = geometry.offscreen;
		if (geometry.rect !== undefined) candidate.node.rect = geometry.rect;
		candidate.node.ref = draftRefs.put(candidate.element);
	}
	draftRefs.revoke();

	const finalise = (): void => {
		observation.truncated = truncated;
		observation.omitted = Array.from(omitted);
	};
	finalise();
	const budget = MAX_OBSERVATION_BYTES - OBSERVATION_HEADROOM_BYTES;
	while (observation.nodes.length > 0 && byteLength(JSON.stringify(observation)) > budget) {
		observation.nodes.pop();
		truncated = true;
		omitted.add("byte-limit");
		finalise();
	}
	finalise();
	if (observation.nodes.length === 0 && byteLength(JSON.stringify(observation)) > budget) {
		return { ok: false, code: "TOO_LARGE", message: "observation metadata exceeds the byte budget" };
	}

	const liveRefs = createRefTable(request.observationId);
	for (const candidate of selected.slice(0, observation.nodes.length)) liveRefs.put(candidate.element);

	const nodes: DomNode[] = observation.nodes.map((node) => deepFreeze(node));
	const frozen = deepFreeze<Observation>({
		...observation,
		nodes: deepFreeze(nodes),
		omitted: deepFreeze(observation.omitted),
	});
	return { ok: true, observation: frozen, refs: liveRefs, bytes: byteLength(JSON.stringify(frozen)) };
}
