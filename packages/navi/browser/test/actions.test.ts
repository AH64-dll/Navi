import assert from "node:assert/strict";
import { test } from "node:test";
import { ActionError, focusElement, scrollPage } from "../content/actions.ts";

function fixture(attrs: Record<string, string> = {}, tag = "BUTTON") {
	const effects: unknown[] = [];
	const style = { display: "block", visibility: "visible", opacity: "1" };
	const view = {
		scrollX: 0,
		scrollY: 0,
		getComputedStyle: () => style,
		scrollBy: (options: ScrollToOptions) => {
			effects.push(options);
			view.scrollX += options.left ?? 0;
			view.scrollY += options.top ?? 0;
		},
	};
	const documentData = {
		visibilityState: "visible",
		hasFocus: () => true,
		activeElement: null as Element | null,
		defaultView: view,
	};
	const document = documentData as unknown as Document;
	const data = {
		ownerDocument: document,
		tagName: tag,
		isConnected: true,
		parentElement: null as Element | null,
		getRootNode: () => document,
		getAttribute: (name: string) => attrs[name] ?? null,
		hasAttribute: (name: string) => Object.hasOwn(attrs, name),
		focus: (options: FocusOptions) => {
			effects.push(options);
			documentData.activeElement = element;
		},
		scrollIntoView: (options: ScrollIntoViewOptions) => {
			effects.push(options);
		},
		get value(): never {
			throw new Error("Actions must never read values");
		},
	};
	const element = data as unknown as Element;
	return { document, documentData, element, data, effects, style, view };
}

function hasCode(code: string) {
	return (error: unknown) => error instanceof ActionError && error.code === code;
}

test("focus uses retained identity and preventScroll without reading values", () => {
	const target = fixture();
	assert.equal(focusElement(target.element), "applied");
	assert.deepEqual(target.effects, [{ preventScroll: true }]);
});

test("detached, hidden, inert and sensitive targets dispatch nothing", () => {
	for (const [attrs, tag] of [
		[{ hidden: "" }, "BUTTON"],
		[{ inert: "" }, "BUTTON"],
		[{ disabled: "" }, "BUTTON"],
		[{ contenteditable: "true" }, "BUTTON"],
		[{ autocomplete: "one-time-code" }, "BUTTON"],
		[{}, "INPUT"],
		[{ "aria-hidden": "true" }, "BUTTON"],
	] as [Record<string, string>, string][]) {
		const target = fixture(attrs, tag);
		assert.throws(() => focusElement(target.element), hasCode("UNKNOWN_REF"));
		assert.equal(target.effects.length, 0);
	}
	const detached = fixture();
	detached.data.isConnected = false;
	assert.throws(() => focusElement(detached.element), hasCode("UNKNOWN_REF"));
});

test("excluded ancestry is rechecked after observation", () => {
	const target = fixture();
	target.data.parentElement = fixture({ hidden: "" }, "DIV").element;
	assert.throws(() => focusElement(target.element), hasCode("UNKNOWN_REF"));
	assert.equal(target.effects.length, 0);
});

test("inactive documents do not focus or scroll", () => {
	const target = fixture();
	target.documentData.hasFocus = () => false;
	assert.throws(() => focusElement(target.element), hasCode("TAB_NOT_ACTIVE"));
	assert.throws(() => scrollPage({ document: target.document, deltaY: 10 }), hasCode("TAB_NOT_ACTIVE"));
	assert.equal(target.effects.length, 0);
});

test("pre-dispatch cancellation differs from unknown outcome after dispatch", () => {
	const target = fixture();
	const controller = new AbortController();
	controller.abort();
	assert.throws(() => focusElement(target.element, controller.signal), hasCode("CANCELLED"));
	assert.equal(target.effects.length, 0);
	const after = new AbortController();
	target.data.focus = () => {
		target.effects.push("focus");
		after.abort();
	};
	assert.equal(focusElement(target.element, after.signal), "unknown");
	assert.deepEqual(target.effects, ["focus"]);
});

test("page scroll is bounded and instant; mixed reference/delta is denied", () => {
	const target = fixture();
	assert.deepEqual(scrollPage({ document: target.document, deltaY: 100 }), {
		outcome: "applied",
		scrollX: 0,
		scrollY: 100,
	});
	assert.deepEqual(target.effects, [{ left: 0, top: 100, behavior: "instant" }]);
	for (const deltaY of [1001, -1001, NaN, Infinity]) {
		assert.throws(() => scrollPage({ document: target.document, deltaY }), hasCode("BAD_ARGUMENT"));
	}
	assert.throws(() => scrollPage({ element: target.element, deltaY: 10 }), hasCode("BAD_ARGUMENT"));
	assert.throws(() => scrollPage({ document: target.document, align: "end" }), hasCode("BAD_ARGUMENT"));
	assert.equal(target.effects.length, 1);
});

test("reference scrolling never looks up a replacement node", () => {
	const target = fixture({}, "DIV");
	assert.equal(scrollPage({ element: target.element, align: "end" }).outcome, "applied");
	assert.deepEqual(target.effects, [{ behavior: "instant", block: "end", inline: "nearest" }]);
	target.data.isConnected = false;
	assert.throws(() => scrollPage({ element: target.element }), hasCode("UNKNOWN_REF"));
	assert.equal(target.effects.length, 1);
});
