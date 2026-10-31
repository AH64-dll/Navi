/**
 * Grant construction: the only place that turns a user choice into a grant value.
 *
 * A grant binds one tab, one document and a fresh generation. Every field is either user-chosen
 * (capabilities) or browser-supplied (tab/document); nothing here reads page, model or CLI input.
 * Grant *state* belongs to `src/core/grants.ts`; this module only builds the value.
 */
import type { Binding, Capability, Grant } from "../../src/protocol/types.ts";

/** A grant ends this long after it was created. Reattaching is explicit; nothing is renewed silently. */
export const GRANT_IDLE_MS = 10 * 60 * 1000;

/** Capabilities in canonical order. `observe` is always part of a grant. */
const CANONICAL: readonly Capability[] = ["observe", "focus", "scroll"];

/** Normalizes a user capability choice: always includes `observe`, deduplicates, keeps canonical order. */
export function selectCapabilities(requested: readonly Capability[]): Capability[] {
	const set = new Set<Capability>(requested);
	set.add("observe");
	return CANONICAL.filter((capability) => set.has(capability));
}

export function createGrant(input: {
	grantId: string;
	tabId: number;
	documentId: string;
	pageEpoch: number;
	capabilities: readonly Capability[];
}): Grant {
	const binding: Binding = {
		grantId: input.grantId,
		tabId: input.tabId,
		documentId: input.documentId,
		pageEpoch: input.pageEpoch,
	};
	return { grantId: input.grantId, binding, capabilities: selectCapabilities(input.capabilities) };
}
