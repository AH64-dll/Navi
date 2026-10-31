/**
 * Collector and reference-table tests.
 *
 * These use a narrow fake DOM: only the properties the collector is allowed to touch exist, and
 * `textContent`, `innerText`, `value`, `outerHTML` and `querySelector` throw when read. A regression
 * that reaches for one of them fails here instead of passing silently.
 *
 * This is not live-browser verification. Real Chrome behaviour (injection, sender identity, real layout
 * and CSS) remains a manual gate.
 */
import assert from "node:assert/strict";
import test from "node:test";
import { MAX_OBSERVATION_BYTES } from "../../src/protocol/limits.ts";
import type { Binding, Observation } from "../../src/protocol/types.ts";
import { parseFrame } from "../../src/protocol/validate.ts";
import { type CollectRequest, type CollectResult, collectObservation, sanitizeUrl } from "../content/collector.ts";
import { createRefStore } from "../content/refs.ts";

type FakeTextNode = { nodeType: 3; data: string };
type FakeElementNode = {
	nodeType: 1;
	localName: string;
	childNodes: Array<FakeTextNode | FakeElementNode>;
	children: FakeElementNode[];
	isConnected: boolean;
	shadowRoot: { mode: string } | null;
	parentElement: FakeElementNode | null;
	labels?: FakeElementNode[] | null;
	ownerDocument?: unknown;
	getAttribute(name: string): string | null;
	hasAttribute(name: string): boolean;
	closest(selector: string): FakeElementNode | null;
	getBoundingClientRect(): { left: number; top: number; width: number; height: number };
	checkVisibility?(): boolean;
};
type FakeNode = FakeTextNode | FakeElementNode;

type ElementOptions = {
	attrs?: Record<string, string>;
	children?: FakeNode[];
	connected?: boolean;
	openShadow?: boolean;
	hideByStyle?: boolean;
	rect?: { left: number; top: number; width: number; height: number };
	throwOnRect?: boolean;
};

function text(data: string): FakeTextNode {
	return { nodeType: 3, data };
}

function element(localName: string, options: ElementOptions = {}): FakeElementNode {
	const attrs = options.attrs ?? {};
	const childNodes = options.children ?? [];
	const node: FakeElementNode = {
		nodeType: 1,
		localName,
		childNodes,
		children: childNodes.filter((child): child is FakeElementNode => child.nodeType === 1),
		isConnected: options.connected !== false,
		shadowRoot: options.openShadow === true ? { mode: "open" } : null,
		parentElement: null,
		getAttribute: (name) => (Object.hasOwn(attrs, name) ? (attrs[name] ?? "") : null),
		hasAttribute: (name) => Object.hasOwn(attrs, name),
		closest: (selector) => (selector === "label" && localName === "label" ? node : null),
		getBoundingClientRect: () => {
			if (options.throwOnRect === true) throw new Error("geometry denied");
			return options.rect ?? { left: 0, top: 0, width: 10, height: 10 };
		},
	};
	if (options.hideByStyle !== undefined) node.checkVisibility = () => options.hideByStyle !== true;
	for (const forbidden of ["textContent", "innerText", "value", "outerHTML", "querySelector", "querySelectorAll"]) {
		Object.defineProperty(node, forbidden, {
			get() {
				throw new Error(`${forbidden} must not be read`);
			},
		});
	}
	return node;
}

type DocumentOptions = {
	body: FakeElementNode;
	url?: string;
	title?: string;
	viewport?: { w: number; h: number } | null;
	computedStyles?: (element: FakeElementNode) => { display: string; visibility: string };
};

function findById(root: FakeElementNode, id: string): FakeElementNode | null {
	if (root.getAttribute("id") === id) return root;
	for (const child of root.children) {
		const found = findById(child, id);
		if (found) return found;
	}
	return null;
}

function attachOwnerDocument(node: FakeElementNode, doc: unknown): void {
	node.ownerDocument = doc;
	for (const child of node.children) {
		child.parentElement = node;
		attachOwnerDocument(child, doc);
	}
}

function fakeDocument(options: DocumentOptions): Document {
	const url = options.url ?? "https://example.com/page";
	const view =
		options.viewport === null
			? null
			: {
					innerWidth: options.viewport?.w ?? 800,
					innerHeight: options.viewport?.h ?? 600,
					getComputedStyle: (node: FakeElementNode) =>
						options.computedStyles?.(node) ?? { display: "block", visibility: "visible" },
				};
	const doc = {
		title: options.title ?? "",
		location: { href: url },
		URL: url,
		body: options.body,
		documentElement: options.body,
		defaultView: view,
		getElementById: (id: string) => findById(options.body, id),
	};
	attachOwnerDocument(options.body, doc);
	return doc as unknown as Document;
}

const binding: Binding = { grantId: "grant-1", tabId: 7, documentId: "document-1", pageEpoch: 3 };

/** Wraps an observation in the result frame the worker sends, for the real receiver gate. */
function observeFrame(observation: Observation): unknown {
	return {
		v: 1,
		seq: 1,
		type: "result",
		connectionId: "conn-1",
		requestId: "req-1",
		ok: true,
		payload: { op: "observe", observation },
	};
}

/** Runs the frozen receiver validator and returns the observation it accepted. */
function parseObservation(observation: Observation): Observation {
	const frame = parseFrame(observeFrame(observation));
	assert.equal(frame.type, "result");
	if (frame.type !== "result" || !frame.ok) throw new Error("expected an ok result frame");
	assert.equal(frame.payload.op, "observe");
	if (frame.payload.op !== "observe") throw new Error("expected an observe payload");
	return frame.payload.observation;
}

const FORBIDDEN_IN_OUTPUT = /[\u0000-\u001F\u007F-\u009F\u202A-\u202E\u2066-\u2069]/;

function collect(doc: Document, overrides: Partial<CollectRequest> = {}): Extract<CollectResult, { ok: true }> {
	const result = collectObservation({ observationId: "obs-1", binding, document: doc, ...overrides });
	if (!result.ok) throw new Error(`${result.code}: ${result.message}`);
	return result;
}

test("URL sanitization removes credentials, query and fragment", () => {
	assert.equal(sanitizeUrl("https://user:secret@example.com/a/b?token=abc#frag"), "https://example.com/a/b");
	assert.equal(sanitizeUrl("https://example.com/search?q=private"), "https://example.com/search");
	assert.equal(sanitizeUrl("https://example.com"), "https://example.com/");
	assert.equal(sanitizeUrl("javascript:alert(1)"), "");
	assert.equal(sanitizeUrl("not a url"), "");
});

test("observation returns focus targets, headings and text in document order", () => {
	const doc = fakeDocument({
		title: "Docs",
		body: element("body", {
			children: [
				element("h1", { children: [text("Install NAVI")] }),
				element("p", { children: [text("Bounded observation only.")] }),
				element("button", { children: [text("Attach")] }),
			],
		}),
	});
	const { observation } = collect(doc);
	assert.equal(observation.observationId, "obs-1");
	assert.deepEqual(observation.binding, binding);
	assert.equal(observation.url, "https://example.com/page");
	assert.equal(observation.title, "Docs");
	assert.deepEqual(
		observation.nodes.map((node) => [node.tag, node.role, node.name, node.text, node.depth]),
		[
			["h1", "heading", "Install NAVI", "Install NAVI", 1],
			["p", undefined, undefined, "Bounded observation only.", 1],
			["button", "button", "Attach", "Attach", 1],
		],
	);
	assert.equal(observation.visited, 3);
	assert.equal(observation.truncated, false);
});

test("hidden and aria-hidden ancestry is skipped before descendant text is read", () => {
	const doc = fakeDocument({
		body: element("body", {
			children: [
				element("div", { attrs: { hidden: "" }, children: [element("p", { children: [text("HIDDEN-SECRET")] })] }),
				element("div", { attrs: { "aria-hidden": "true" }, children: [text("ARIA-SECRET")] }),
				element("p", { children: [text("Visible text")] }),
			],
		}),
	});
	const { observation } = collect(doc);
	const serialized = JSON.stringify(observation);
	assert.ok(!serialized.includes("HIDDEN-SECRET"));
	assert.ok(!serialized.includes("ARIA-SECRET"));
	assert.deepEqual(
		observation.nodes.map((node) => node.text),
		["Visible text"],
	);
});

test("style-hidden subtrees are excluded through checkVisibility or computed style", () => {
	const checkable = fakeDocument({
		body: element("body", {
			children: [
				element("div", { hideByStyle: true, children: [element("span", { children: [text("CHECK-SECRET")] })] }),
			],
		}),
	});
	assert.ok(!JSON.stringify(collect(checkable).observation).includes("CHECK-SECRET"));

	const computed = fakeDocument({
		body: element("body", {
			children: [element("div", { children: [element("span", { children: [text("CSS-SECRET")] })] })],
		}),
		computedStyles: (node) =>
			node.localName === "div"
				? { display: "none", visibility: "visible" }
				: { display: "block", visibility: "visible" },
	});
	assert.ok(!JSON.stringify(collect(computed).observation).includes("CSS-SECRET"));
});

test("password, OTP, payment and hidden fields are never read or emitted", () => {
	const doc = fakeDocument({
		body: element("body", {
			children: [
				element("input", { attrs: { type: "password", value: "PASSWORDVALUE" } }),
				element("input", { attrs: { type: "text", autocomplete: "one-time-code" } }),
				element("input", { attrs: { type: "text", autocomplete: "cc-number cc-exp" } }),
				element("input", { attrs: { type: "hidden", value: "HIDDENVALUE" } }),
				element("input", { attrs: { type: "search", placeholder: "Search docs" } }),
			],
		}),
	});
	const { observation } = collect(doc);
	assert.equal(observation.nodes.length, 1);
	const node = observation.nodes[0]!;
	assert.equal(node.tag, "input");
	assert.equal(node.role, "searchbox");
	assert.equal(node.name, "Search docs");
	assert.equal(node.editable, true);
	assert.equal(node.interactive, true);
	assert.equal(node.text, undefined);
	const serialized = JSON.stringify(observation);
	assert.ok(!serialized.includes("PASSWORDVALUE"));
	assert.ok(!serialized.includes("HIDDENVALUE"));
});

test("ancestor text sampling never leaks an editable or secret descendant", () => {
	const doc = fakeDocument({
		body: element("body", {
			children: [
				element("p", {
					children: [
						text("Before "),
						element("input", { attrs: { type: "password" } }),
						element("textarea", { children: [text("DRAFTTEXT")] }),
						text(" after"),
					],
				}),
			],
		}),
	});
	const { observation } = collect(doc);
	const paragraph = observation.nodes.find((node) => node.tag === "p");
	assert.equal(paragraph?.text, "Before after");
	const editor = observation.nodes.find((node) => node.tag === "textarea");
	assert.equal(editor?.editable, true);
	assert.equal(editor?.text, undefined);
	assert.ok(!JSON.stringify(observation).includes("DRAFTTEXT"));
});

test("names come from aria-labelledby, aria-label and labels without selectors", () => {
	const labelled = element("input", { attrs: { type: "text", id: "q" } });
	labelled.labels = [element("label", { attrs: { for: "q" }, children: [text("Query")] })];
	const doc = fakeDocument({
		body: element("body", {
			children: [
				element("span", { attrs: { id: "lbl" }, children: [text("Search docs")] }),
				element("input", { attrs: { type: "search", "aria-labelledby": "lbl" } }),
				element("button", { attrs: { "aria-label": "Close dialog" } }),
				labelled,
			],
		}),
	});
	const { observation } = collect(doc);
	assert.deepEqual(
		observation.nodes.map((node) => node.name).filter((name) => name !== undefined),
		["Search docs", "Close dialog", "Query"],
	);
});

test("text is whitespace-collapsed and capped at the shared character limit", () => {
	const doc = fakeDocument({
		body: element("body", { children: [element("p", { children: [text(`   ${"word ".repeat(100)}   `)] })] }),
	});
	const { observation } = collect(doc);
	const node = observation.nodes[0]!;
	assert.ok(node.text !== undefined);
	assert.ok(node.text.length <= 200);
	assert.ok(!node.text.includes("  "));
});

test("the visit budget stops the walk and reports truncation", () => {
	const children = Array.from({ length: 1600 }, (_unused, index) =>
		element("div", { attrs: { "data-i": String(index) }, children: [text(`row ${index}`)] }),
	);
	const { observation } = collect(fakeDocument({ body: element("body", { children }) }));
	assert.equal(observation.visited, 1500);
	assert.equal(observation.truncated, true);
	assert.ok(observation.omitted.includes("node-limit"));
	assert.ok(observation.nodes.length <= 100);
});

test("the observation byte budget trims nodes and reports the byte cause", () => {
	const children = Array.from({ length: 100 }, () => element("p", { children: [text("x".repeat(400))] }));
	const { observation, bytes } = collect(fakeDocument({ body: element("body", { children }) }));
	assert.ok(bytes <= MAX_OBSERVATION_BYTES - 512);
	assert.equal(observation.truncated, true);
	assert.ok(observation.omitted.includes("byte-limit"));
	assert.ok(observation.nodes.length > 0 && observation.nodes.length < 100);
});

test("subframes, canvas and open shadow roots are reported as omitted coverage", () => {
	const doc = fakeDocument({
		body: element("body", {
			children: [
				element("iframe", { attrs: { src: "https://other.example/frame" } }),
				element("canvas"),
				element("div", { openShadow: true, children: [text("light text")] }),
			],
		}),
	});
	const { observation } = collect(doc);
	assert.deepEqual([...observation.omitted].sort(), ["canvas", "shadow", "subframes"]);
	assert.ok(!JSON.stringify(observation).includes("other.example"));
});

test("focus targets win the node budget while output stays in document order", () => {
	const paragraphs = Array.from({ length: 120 }, (_unused, index) =>
		element("p", { children: [text(`paragraph ${index}`)] }),
	);
	const doc = fakeDocument({
		body: element("body", { children: [...paragraphs, element("button", { children: [text("Attach")] })] }),
	});
	const { observation } = collect(doc);
	assert.equal(observation.truncated, true);
	assert.ok(observation.nodes.some((node) => node.tag === "button" && node.name === "Attach"));
	assert.equal(observation.nodes[0]?.tag, "p");
});

test("refs use element identity and stop resolving after revoke, replacement or disconnect", () => {
	const button = element("button", { children: [text("Attach")] });
	const doc = fakeDocument({ body: element("body", { children: [button] }) });
	const { observation, refs } = collect(doc);
	const ref = observation.nodes[0]!.ref;
	assert.equal(ref, "n1");
	assert.equal(refs.get(ref), button as unknown as Element);
	assert.equal(refs.get("page-id"), undefined);
	assert.equal(refs.get("n0"), undefined);

	const store = createRefStore();
	const table = store.begin("obs-1");
	const handle = table.put(button as unknown as Element);
	assert.equal(handle, "n1");
	assert.equal(table.put(button as unknown as Element), handle);
	assert.equal(store.get(handle), button);
	button.isConnected = false;
	assert.equal(store.get(handle), undefined);
	button.isConnected = true;

	const replacement = store.begin("obs-2");
	assert.equal(replacement.observationId, "obs-2");
	assert.equal(store.begin("obs-2").get(handle), undefined);
	table.revoke();
	assert.equal(table.size, 0);
	assert.equal(table.get(handle), undefined);
});

test("identical input produces identical output", () => {
	const build = () =>
		fakeDocument({
			body: element("body", {
				children: [
					element("h2", { children: [text("Section")] }),
					element("input", { attrs: { type: "search", placeholder: "Find" } }),
					element("p", { children: [text("Body text")] }),
				],
			}),
		});
	const first = collect(build(), { now: () => 1000 });
	const second = collect(build(), { now: () => 1000 });
	assert.equal(JSON.stringify(first.observation), JSON.stringify(second.observation));
	assert.equal(JSON.stringify(first.observation.nodes.map((node) => node.ref)), '["n1","n2","n3"]');
});

test("the deadline valve stops a walk and reports truncation", () => {
	const children = Array.from({ length: 60 }, (_unused, index) => element("p", { children: [text(`row ${index}`)] }));
	const doc = fakeDocument({ body: element("body", { children }) });
	let calls = 0;
	const expired = collect(doc, { now: () => (calls++ === 0 ? 0 : 10_000) });
	assert.equal(expired.observation.truncated, true);
	assert.ok(expired.observation.visited < 60);
	const full = collect(doc);
	assert.equal(full.observation.truncated, false);
	assert.equal(full.observation.visited, 60);
});

test("geometry is read per surviving node and a failing element does not abort the capture", () => {
	const doc = fakeDocument({
		body: element("body", {
			children: [
				element("p", { rect: { left: 0, top: 5000, width: 100, height: 20 }, children: [text("Below the fold")] }),
				element("p", { throwOnRect: true, children: [text("No geometry")] }),
			],
		}),
		viewport: { w: 800, h: 600 },
	});
	const { observation } = collect(doc);
	assert.equal(observation.nodes[0]?.offscreen, true);
	assert.deepEqual(observation.nodes[0]?.rect, { x: 0, y: 5000, w: 100, h: 20 });
	assert.equal(observation.nodes[1]?.offscreen, false);
	assert.equal(observation.nodes[1]?.rect, undefined);
});

test("the returned observation is frozen", () => {
	const doc = fakeDocument({ body: element("body", { children: [element("button", { children: [text("Go")] })] }) });
	const { observation } = collect(doc);
	assert.ok(Object.isFrozen(observation));
	assert.ok(Object.isFrozen(observation.nodes));
	assert.ok(Object.isFrozen(observation.nodes[0]));
});

test("a document without a body fails instead of inventing an observation", () => {
	const doc = { title: "", URL: "https://example.com/" } as unknown as Document;
	const result = collectObservation({ observationId: "obs-1", binding, document: doc });
	assert.equal(result.ok, false);
	if (result.ok) return;
	assert.equal(result.code, "CAPTURE_FAILED");
});

test("an excluded root stops the capture instead of walking excluded children", () => {
	const hiddenRoot = fakeDocument({
		body: element("body", {
			attrs: { hidden: "" },
			children: [element("p", { children: [text("ROOT-SECRET")] })],
		}),
	});
	const hidden = collect(hiddenRoot);
	assert.equal(hidden.observation.nodes.length, 0);
	assert.equal(hidden.observation.visited, 0);
	assert.equal(hidden.observation.truncated, true);
	assert.ok(!JSON.stringify(hidden.observation).includes("ROOT-SECRET"));

	const editorRoot = fakeDocument({
		body: element("body", {
			attrs: { contenteditable: "" },
			children: [element("p", { children: [text("EDITOR-ROOT-SECRET")] })],
		}),
	});
	const editor = collect(editorRoot);
	assert.equal(editor.observation.nodes.length, 0);
	assert.equal(editor.observation.visited, 0);
	assert.ok(!JSON.stringify(editor.observation).includes("EDITOR-ROOT-SECRET"));
});

test("hidden or editor label targets are never used as names", () => {
	const labelled = element("input", { attrs: { type: "text", id: "q" } });
	labelled.labels = [
		element("label", { attrs: { hidden: "" }, children: [text("HIDDEN-FOR-LABEL")] }),
		element("label", { attrs: { for: "q" }, children: [text("Visible label")] }),
	];
	const doc = fakeDocument({
		body: element("body", {
			children: [
				element("span", { attrs: { id: "hidden-lbl", hidden: "" }, children: [text("HIDDEN-LABEL")] }),
				element("span", { attrs: { id: "editor-lbl", contenteditable: "" }, children: [text("EDITOR-LABEL")] }),
				element("span", { attrs: { id: "safe-lbl" }, children: [text("Safe label")] }),
				element("input", { attrs: { type: "search", "aria-labelledby": "hidden-lbl editor-lbl" } }),
				element("input", { attrs: { type: "search", "aria-labelledby": "safe-lbl" } }),
				labelled,
			],
		}),
	});
	const { observation } = collect(doc);
	const names = observation.nodes.map((node) => node.name);
	assert.ok(!names.includes("HIDDEN-LABEL"));
	assert.ok(!names.includes("EDITOR-LABEL"));
	assert.ok(!names.includes("HIDDEN-FOR-LABEL"));
	assert.ok(names.includes("Safe label"));
	assert.equal(names.at(-1), "Visible label");
	const serialized = JSON.stringify(observation);
	assert.ok(!serialized.includes("HIDDEN-LABEL"));
	assert.ok(!serialized.includes("EDITOR-LABEL"));
	assert.ok(!serialized.includes("HIDDEN-FOR-LABEL"));
});

test("text clamping counts UTF-16 units and never splits a surrogate pair", () => {
	const doc = fakeDocument({
		body: element("body", {
			children: [
				element("p", { children: [text("😀".repeat(300))] }),
				element("p", { children: [text(`a${"😀".repeat(300)}`)] }),
			],
		}),
	});
	const { observation } = collect(doc);
	for (const node of observation.nodes) {
		assert.ok(node.text !== undefined);
		assert.ok(node.text.length <= 200);
		assert.ok(!/[\uD800-\uDBFF]$/.test(node.text));
	}
});

test("the reference table matches the returned nodes after byte trimming", () => {
	const children = Array.from({ length: 100 }, () => element("p", { children: [text("x".repeat(400))] }));
	const { observation, refs } = collect(fakeDocument({ body: element("body", { children }) }));
	assert.equal(refs.size, observation.nodes.length);
	assert.deepEqual(
		observation.nodes.map((node) => node.ref),
		observation.nodes.map((_unused, index) => `n${index + 1}`),
	);
	for (const node of observation.nodes) assert.notEqual(refs.get(node.ref), undefined);
	assert.equal(refs.get("n9999"), undefined);
});

test("titles drop control, DEL, C1 and bidi characters before the receiver sees them", () => {
	const doc = fakeDocument({
		title: "Docs\u0000\u0007\u001B\u007F\u009F\u202E\u2066\u2069 Title",
		body: element("body", { children: [element("p", { children: [text("Body")] })] }),
	});
	const { observation } = collect(doc);
	assert.equal(observation.title, "Docs Title");
	assert.ok((observation.title?.length ?? 0) <= 200);
	assert.ok(!FORBIDDEN_IN_OUTPUT.test(observation.title ?? ""));
	assert.doesNotThrow(() => parseFrame(observeFrame(observation)));
	assert.equal(parseObservation(observation).title, "Docs Title");
});

test("the receiver rejects an unsanitized title or non-canonical url, so the boundary is load-bearing", () => {
	const doc = fakeDocument({
		title: "Plain",
		body: element("body", { children: [element("p", { children: [text("Body")] })] }),
	});
	const { observation } = collect(doc);
	assert.throws(() => parseFrame(observeFrame({ ...observation, title: "bad\u0007title" })), /control characters/);
	assert.throws(() => parseFrame(observeFrame({ ...observation, title: "bidi\u202Eoverride" })), /control characters/);
	assert.throws(
		() => parseFrame(observeFrame({ ...observation, url: "https://example.com/x?q=1" })),
		/query or fragment/,
	);
	assert.throws(
		() => parseFrame(observeFrame({ ...observation, url: "https://user:pass@example.com/x" })),
		/credentials/,
	);
	assert.throws(() => parseFrame(observeFrame({ ...observation, url: "" })), /non-empty/);
});

test("the display url stays canonical, credential-free and free of forbidden characters", () => {
	const doc = fakeDocument({
		url: "https://user:pass@example.com/docs/a\u202Eb?token=SECRET#frag",
		body: element("body", { children: [element("p", { children: [text("Body")] })] }),
	});
	const { observation } = collect(doc);
	assert.ok(observation.url.startsWith("https://example.com/docs/a"));
	assert.ok(!observation.url.includes("SECRET"));
	assert.ok(!observation.url.includes("user"));
	assert.ok(!observation.url.includes("?"));
	assert.ok(!observation.url.includes("#"));
	assert.ok(!FORBIDDEN_IN_OUTPUT.test(observation.url));
	assert.ok(observation.url.length <= 512);
	assert.equal(parseObservation(observation).url, observation.url);
});

test("unsupported documents are refused instead of returning an empty url", () => {
	for (const url of ["chrome://settings/", "about:blank", "file:///tmp/page.html", "data:text/html,x", ""]) {
		const doc = fakeDocument({
			url,
			body: element("body", { children: [element("p", { children: [text("Body")] })] }),
		});
		const result = collectObservation({ observationId: "obs-1", binding, document: doc });
		assert.equal(result.ok, false, url);
		if (result.ok) continue;
		assert.equal(result.code, "CAPTURE_FAILED");
		assert.equal(result.message, "unsupported document url");
	}
});

test("declared roles use the first safe token and otherwise fall back to tag semantics", () => {
	const doc = fakeDocument({
		body: element("body", {
			children: [
				element("div", { attrs: { role: "button link" }, children: [text("Multi token")] }),
				element("input", { attrs: { type: "search", role: "1bad !!" } }),
				element("span", { attrs: { role: "BUTTON" }, children: [text("Cased")] }),
				element("div", { attrs: { role: "listbox option" }, children: [text("Combined")] }),
				element("div", {
					attrs: { role: "x".repeat(80) },
					children: [text("Over-long token")],
				}),
			],
		}),
	});
	const { observation } = collect(doc);
	assert.deepEqual(
		observation.nodes.map((node) => [node.tag, node.role]),
		[
			["div", "button"],
			["input", "searchbox"],
			["span", "button"],
			["div", "listbox"],
			["div", undefined],
		],
	);
	for (const node of observation.nodes) {
		assert.ok(node.tag === undefined || /^[a-z][a-z0-9-]*$/.test(node.tag));
		if (node.role !== undefined) assert.ok(/^[a-z][a-z-]*$/.test(node.role), node.role);
	}
	assert.doesNotThrow(() => parseFrame(observeFrame(observation)));
});

test("depth and rect stay inside the protocol limits", () => {
	let deepest = element("button", { children: [text("Deep")] });
	for (let index = 0; index < 150; index += 1) deepest = element("div", { children: [deepest] });
	const doc = fakeDocument({
		body: element("body", {
			children: [
				deepest,
				element("p", {
					rect: { left: 5_000_000, top: -5_000_000, width: 9_000_000, height: 2 },
					children: [text("Huge")],
				}),
			],
		}),
	});
	const { observation } = collect(doc);
	assert.equal(observation.nodes.find((node) => node.tag === "button")?.depth, 100);
	assert.deepEqual(observation.nodes.find((node) => node.tag === "p")?.rect, {
		x: 1_000_000,
		y: -1_000_000,
		w: 1_000_000,
		h: 2,
	});
	assert.equal(parseObservation(observation).nodes.length, observation.nodes.length);
});

test("a full observation with hostile page content passes the protocol receiver gate", () => {
	let nested = element("button", { attrs: { role: "button link" }, children: [text("😀 Attach now")] });
	for (let index = 0; index < 20; index += 1) nested = element("div", { children: [nested] });
	const doc = fakeDocument({
		title: "Docs \u202E\u2066 \u0007 页面 \uD83D\uDE00",
		url: "https://user:pass@example.com/docs/a\u202Eb?token=SECRET#frag",
		viewport: { w: 800, h: 600 },
		body: element("body", {
			children: [
				element("h1", { children: [text("Übersicht \u0000 Hinweise")] }),
				nested,
				element("input", { attrs: { type: "text", "aria-label": "Name \u202E\u2069" } }),
				element("input", { attrs: { type: "password", "aria-labelledby": "pw-label" } }),
				element("iframe", { attrs: { src: "https://other.example/frame" } }),
				element("canvas"),
				element("p", { children: [text("Tail text 🎉")] }),
			],
		}),
	});
	const { observation } = collect(doc);
	const parsed = parseObservation(observation);
	assert.equal(parsed.url, "https://example.com/docs/a%E2%80%AEb");
	assert.ok(!FORBIDDEN_IN_OUTPUT.test(parsed.title ?? ""));
	assert.ok(!JSON.stringify(parsed).includes("SECRET"));
	assert.ok(parsed.nodes.length > 0 && parsed.nodes.length <= 100);
	assert.ok(parsed.nodes.some((node) => node.text?.includes("🎉")));
	assert.deepEqual([...parsed.omitted].sort(), ["canvas", "subframes"]);
});
