/**
 * Node reference table for the NAVI content script.
 *
 * A ref is an opaque handle for one collected element. The table holds the actual `Element` identity
 * behind a `WeakRef`; it never re-resolves a ref from a page id, a CSS selector, a text match or an
 * index. A handle is valid only for its own observation: a new observation replaces the table and every
 * older handle stops resolving.
 *
 * Connectedness is checked on every lookup. A node removed from the document, or a document replaced by
 * navigation, makes its handle unresolvable instead of silently pointing at a similar element.
 *
 * DOM access only. No `chrome` API, no Node API, no module side effects.
 */

/** Handles are `n1`, `n2`, ... bounded to match the maximum node count. */
const REF_PATTERN = /^n[1-9][0-9]{0,4}$/;

export type RefTable = {
	readonly observationId: string;
	readonly size: number;
	/** Store one element and return its handle. Re-storing the same element returns the same handle. */
	put(element: Element): string;
	/** Resolve a handle. Returns undefined when unknown, revoked, or no longer connected. */
	get(ref: string): Element | undefined;
	/** Drop every handle in this table. Later lookups always fail. */
	revoke(): void;
};

export type RefStore = {
	/** Replace the current table. The previous table is revoked, so only the latest observation resolves. */
	begin(observationId: string): RefTable;
	current(): RefTable | undefined;
	get(ref: string): Element | undefined;
	revoke(): void;
};

export function createRefTable(observationId: string): RefTable {
	const byRef = new Map<string, WeakRef<Element>>();
	const byElement = new WeakMap<Element, string>();
	let next = 1;
	let revoked = false;

	return {
		observationId,
		get size() {
			return byRef.size;
		},
		put(element: Element): string {
			if (revoked) throw new Error("ref table revoked");
			const existing = byElement.get(element);
			if (existing !== undefined) return existing;
			const ref = `n${next}`;
			next += 1;
			byElement.set(element, ref);
			byRef.set(ref, new WeakRef(element));
			return ref;
		},
		get(ref: string): Element | undefined {
			if (revoked || !REF_PATTERN.test(ref)) return undefined;
			const entry = byRef.get(ref);
			if (entry === undefined) return undefined;
			const element = entry.deref();
			if (element === undefined || element.isConnected !== true) {
				byRef.delete(ref);
				return undefined;
			}
			return element;
		},
		revoke(): void {
			revoked = true;
			byRef.clear();
		},
	};
}

/** Latest-observation-only holder used by the content-script dispatcher. */
export function createRefStore(): RefStore {
	let table: RefTable | undefined;
	return {
		begin(observationId: string): RefTable {
			table?.revoke();
			table = createRefTable(observationId);
			return table;
		},
		current(): RefTable | undefined {
			return table;
		},
		get(ref: string): Element | undefined {
			return table?.get(ref);
		},
		revoke(): void {
			table?.revoke();
			table = undefined;
		},
	};
}
