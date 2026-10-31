/**
 * Grant lifecycle for one connection. Pure and browser-neutral: the worker owns the authority and
 * feeds this class values it read from authenticated browser APIs; the CLI only projects
 * `binding.changed` events into `applyChange`.
 *
 * What this class does not do: authenticate a browser target, persist a grant, or reconnect. A grant
 * here is a record of a decision made elsewhere. Nothing is written to storage and nothing survives
 * a worker restart.
 */

import { IDLE_CLOSE_MS } from "../protocol/limits.ts";
import type { Binding, Capability, Grant, GrantChangeReason, NaviError, Observation } from "../protocol/types.ts";
import { naviError } from "./errors.ts";
import type { Clock } from "./ports.ts";

/** Full binding equality: every field is browser-supplied and none may be defaulted. */
export function bindingMatches(current: Binding, candidate: Binding): boolean {
	return (
		current.grantId === candidate.grantId &&
		current.tabId === candidate.tabId &&
		current.documentId === candidate.documentId &&
		current.pageEpoch === candidate.pageEpoch
	);
}

export function capabilityGranted(grant: Grant, capability: Capability): boolean {
	return grant.capabilities.includes(capability);
}

export type GrantSnapshot = {
	grant: Grant | null;
	generation: number;
	observationId?: string;
	expiresAtMs?: number;
};

export class GrantManager {
	private current: Grant | null = null;
	private currentGeneration = 0;
	private latestObservationId: string | undefined;
	private expiresAtMs: number | undefined;

	private readonly clock: Clock;
	/** Grant lifetime in clock milliseconds. The clock is monotonic, so this is a duration, not a date. */
	private readonly expiryMs: number;

	constructor(options: { clock: Clock; expiryMs?: number }) {
		this.clock = options.clock;
		this.expiryMs = options.expiryMs ?? IDLE_CLOSE_MS;
	}

	get grant(): Grant | null {
		return this.current;
	}

	/** Increases on every attach and every invalidation. Observations die with a generation change. */
	get generation(): number {
		return this.currentGeneration;
	}

	get observationId(): string | undefined {
		return this.latestObservationId;
	}

	snapshot(): GrantSnapshot {
		return {
			grant: this.current,
			generation: this.currentGeneration,
			...(this.latestObservationId === undefined ? {} : { observationId: this.latestObservationId }),
			...(this.expiresAtMs === undefined ? {} : { expiresAtMs: this.expiresAtMs }),
		};
	}

	/**
	 * Worker-only path. `grant` must come from the trusted popup flow and browser-provided identity.
	 * Attaching replaces any previous binding and starts a new generation.
	 */
	attach(grant: Grant): void {
		this.current = grant;
		this.currentGeneration += 1;
		this.latestObservationId = undefined;
		this.expiresAtMs = this.clock.now() + this.expiryMs;
	}

	/** CLI path: the same state change, but the value arrived in a `binding.changed` event. */
	applyChange(grant: Grant | null): void {
		if (grant === null) {
			this.invalidate();
			return;
		}
		this.current = grant;
		this.currentGeneration += 1;
		this.latestObservationId = undefined;
		this.expiresAtMs = this.clock.now() + this.expiryMs;
	}

	invalidate(): void {
		if (this.current === null) return;
		this.current = null;
		this.currentGeneration += 1;
		this.latestObservationId = undefined;
		this.expiresAtMs = undefined;
	}

	/** Returns the reason when the grant is past its lifetime; invalidates in that case. */
	expireIfDue(): GrantChangeReason | null {
		if (this.current === null || this.expiresAtMs === undefined) return null;
		if (this.clock.now() < this.expiresAtMs) return null;
		this.invalidate();
		return "expired";
	}

	requireGrant(): Grant {
		if (this.current === null) throw new Error("not attached");
		return this.current;
	}

	requireCapability(capability: Capability): void {
		const grant = this.current;
		if (grant === null) throw new Error("not attached");
		if (!capabilityGranted(grant, capability)) throw new Error(`capability not granted: ${capability}`);
	}

	/** `null` when the request binding is current; otherwise the error to return for that request. */
	checkBinding(binding: Binding): NaviError | null {
		const grant = this.current;
		if (grant === null) return naviError("NOT_ATTACHED", "No grant is attached for this connection");
		if (!bindingMatches(grant.binding, binding)) {
			return naviError("BINDING_STALE", "Request binding does not match the current grant and document");
		}
		return null;
	}

	/** Records the observation that refs may point into. Refuses a binding that is already stale. */
	noteObservation(observation: Observation): NaviError | null {
		const stale = this.checkBinding(observation.binding);
		if (stale !== null) return stale;
		this.latestObservationId = observation.observationId;
		return null;
	}

	/** Refs belong to one observation of one document and are never rebound to a similar node. */
	checkObservation(binding: Binding, observationId: string): NaviError | null {
		const stale = this.checkBinding(binding);
		if (stale !== null) return stale;
		if (this.latestObservationId === undefined || this.latestObservationId !== observationId) {
			return naviError("STALE_OBSERVATION", "Observation is not the current one for this document");
		}
		return null;
	}
}
