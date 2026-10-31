/**
 * CLI-side session over one frame channel. Implements `BrowserPort` for the CLI harness and keeps a
 * read-only projection of the grant that the worker owns.
 *
 * The client never originates authority: it sends the binding from `binding.changed` events, refuses
 * operations whose capability the user did not grant, and refuses to work without a grant.
 *
 * Race rules:
 * - A result is accepted only if the grant generation and binding are still current. An observation
 *   captured under a replaced or revoked grant is refused, not stored.
 * - A binding change while an operation is pending cancels that operation; a possible page effect is
 *   reported as `OUTCOME_UNKNOWN`.
 * - Closing the link drops the grant projection and the current observation reference.
 */
import { MAX_ID_CHARS } from "../protocol/limits.ts";
import type {
	Binding,
	CancelResult,
	Capability,
	EffectOutcome,
	EventFrame,
	Grant,
	Observation,
	ScrollAlign,
} from "../protocol/types.ts";
import type { ConnectionLimits, ConnectionRole, ConnectionState, ReadyInfo } from "./connection.ts";
import { Connection, DEFAULT_CONNECTION_LIMITS } from "./connection.ts";
import { naviError } from "./errors.ts";
import { bindingMatches, capabilityGranted } from "./grants.ts";
import type { BrowserPort, BrowserPortStatus, Clock, FrameChannel, ScrollArgument } from "./ports.ts";
import { NaviRequestError } from "./ports.ts";

export type BrowserClientOptions = {
	channel: FrameChannel;
	clock: Clock;
	/** Our peer descriptor. Defaults to a CLI peer on the shared protocol version. */
	peerVersion?: string;
	capabilities?: Capability[];
	limits?: Partial<ConnectionLimits>;
	newConnectionId?: () => string;
	/**
	 * The worker opens the link, so the CLI is the responder that mints the connection id.
	 * Override only for a test double or a future CLI-initiated link.
	 */
	role?: ConnectionRole;
};

export class BrowserClient implements BrowserPort {
	private readonly connection: Connection;
	private grantValue: Grant | null = null;
	/** Increases on every grant change, so a result from a replaced grant can be detected. */
	private grantGeneration = 0;
	private observationIdValue: string | undefined;
	private readonly listeners = new Set<(status: BrowserPortStatus) => void>();

	constructor(options: BrowserClientOptions) {
		const capabilities = options.capabilities ?? ["observe", "focus", "scroll"];
		this.connection = new Connection({
			role: options.role ?? "responder",
			channel: options.channel,
			clock: options.clock,
			peer: { kind: "cli", version: options.peerVersion ?? "0.0.1" },
			capabilities,
			...(options.limits === undefined ? {} : { limits: options.limits }),
			...(options.newConnectionId === undefined ? {} : { newConnectionId: options.newConnectionId }),
			onReady: (_info: ReadyInfo) => this.emitStatus(),
			onEvent: (event) => this.onEvent(event),
			onStateChange: (state) => this.onStateChange(state),
			// The CLI has no browser work to offer, so an inbound request is not supported.
			handleRequest: async () => naviError("UNSUPPORTED_OP", "The CLI does not serve browser operations"),
		});
	}

	/** Opens the handshake. The worker's `hello` is answered with `hello.ack`. */
	open(): void {
		this.connection.open();
	}

	status(): BrowserPortStatus {
		const lastError = this.connection.lastError;
		const inFlight = this.connection.inFlightRequestId;
		return {
			link: this.connection.state,
			...(this.connection.connectionId === undefined ? {} : { connectionId: this.connection.connectionId }),
			...(this.connection.peer === undefined ? {} : { peer: this.connection.peer }),
			peerCapabilities: [...this.connection.peerCapabilities],
			grant: this.grantValue,
			...(this.observationIdValue === undefined ? {} : { observationId: this.observationIdValue }),
			...(inFlight === undefined ? {} : { inFlightRequestId: inFlight }),
			...(lastError === undefined ? {} : { lastError }),
		};
	}

	onStatus(listener: (status: BrowserPortStatus) => void): () => void {
		this.listeners.add(listener);
		return () => {
			this.listeners.delete(listener);
		};
	}

	async observe(options: { signal?: AbortSignal } = {}): Promise<Observation> {
		const grant = this.requireGrant();
		this.requireCapability("observe");
		const generation = this.grantGeneration;
		const payload = await this.connection.request({ op: "observe", binding: grant.binding }, options);
		if (payload.op !== "observe") {
			throw new NaviRequestError(naviError("INTERNAL", "Peer answered observe with another payload"));
		}
		if (!this.isCurrentGrant(grant.binding, generation)) {
			throw new NaviRequestError(
				naviError("BINDING_STALE", "Grant or document changed while the observation was in flight"),
			);
		}
		const observation = payload.observation;
		if (!bindingMatches(grant.binding, observation.binding)) {
			throw new NaviRequestError(naviError("BINDING_STALE", "Observation belongs to a different grant or document"));
		}
		this.observationIdValue = observation.observationId;
		this.emitStatus();
		return observation;
	}

	async focus(ref: string, options: { signal?: AbortSignal } = {}): Promise<EffectOutcome> {
		const grant = this.requireGrant();
		this.requireCapability("focus");
		const observationId = this.requireObservation("focus");
		if (ref.length === 0 || ref.length > MAX_ID_CHARS) {
			throw new NaviRequestError(naviError("BAD_ARGUMENT", "Node reference is missing or too long"));
		}
		const generation = this.grantGeneration;
		const payload = await this.connection.request(
			{ op: "focus", binding: grant.binding, observationId, ref },
			options,
		);
		if (payload.op !== "focus") {
			throw new NaviRequestError(naviError("INTERNAL", "Peer answered focus with another payload"));
		}
		if (!this.isCurrentGrant(grant.binding, generation)) {
			throw new NaviRequestError(
				naviError("OUTCOME_UNKNOWN", "Grant changed while focus was in flight; the page may have changed"),
			);
		}
		return payload.outcome;
	}

	async scroll(
		argument: ScrollArgument,
		options: { signal?: AbortSignal } = {},
	): Promise<{ outcome: EffectOutcome; scrollX?: number; scrollY?: number }> {
		const grant = this.requireGrant();
		this.requireCapability("scroll");
		const observationId = this.requireObservation("scroll");
		if (
			argument.ref === undefined &&
			argument.align === undefined &&
			argument.deltaY === undefined &&
			argument.deltaX === undefined
		) {
			throw new NaviRequestError(naviError("BAD_ARGUMENT", "Scroll needs a ref, an alignment or a delta"));
		}
		const generation = this.grantGeneration;
		const payload = await this.connection.request(
			{
				op: "scroll",
				binding: grant.binding,
				observationId,
				...(argument.ref === undefined ? {} : { ref: argument.ref }),
				...(argument.align === undefined ? {} : { align: argument.align as ScrollAlign }),
				...(argument.deltaY === undefined ? {} : { deltaY: argument.deltaY }),
				...(argument.deltaX === undefined ? {} : { deltaX: argument.deltaX }),
			},
			options,
		);
		if (payload.op !== "scroll") {
			throw new NaviRequestError(naviError("INTERNAL", "Peer answered scroll with another payload"));
		}
		if (!this.isCurrentGrant(grant.binding, generation)) {
			throw new NaviRequestError(
				naviError("OUTCOME_UNKNOWN", "Grant changed while scroll was in flight; the page may have moved"),
			);
		}
		return {
			outcome: payload.outcome,
			...(payload.scrollX === undefined ? {} : { scrollX: payload.scrollX }),
			...(payload.scrollY === undefined ? {} : { scrollY: payload.scrollY }),
		};
	}

	async cancel(options: { signal?: AbortSignal } = {}): Promise<CancelResult> {
		return await this.connection.cancelInFlight(options);
	}

	async disconnect(reason: "closed" | "detached" = "detached"): Promise<void> {
		this.connection.close(reason);
	}

	private onEvent(event: EventFrame): void {
		if (event.kind !== "binding.changed") return;
		if (this.connection.inFlightRequestId !== undefined) {
			// The pending operation belongs to the previous grant and document. It is settled as unknown
			// or cancelled by the connection; it is never retried against the new binding.
			void this.connection.cancelInFlight().catch(() => undefined);
		}
		this.grantValue = event.payload.grant;
		this.grantGeneration += 1;
		this.observationIdValue = undefined;
		this.emitStatus();
	}

	private onStateChange(state: ConnectionState): void {
		if (state === "closed") {
			// The worker revokes on port loss, so the CLI projection must not outlive the link.
			this.grantValue = null;
			this.observationIdValue = undefined;
		}
		this.emitStatus();
	}

	private isCurrentGrant(binding: Binding, generation: number): boolean {
		const grant = this.grantValue;
		return grant !== null && generation === this.grantGeneration && bindingMatches(grant.binding, binding);
	}

	private requireGrant(): Grant {
		const grant = this.grantValue;
		if (grant === null) {
			throw new NaviRequestError(
				naviError("NOT_ATTACHED", "No browser target is attached; attach from the extension popup"),
			);
		}
		return grant;
	}

	private requireCapability(capability: Capability): void {
		const grant = this.grantValue;
		if (grant === null) {
			throw new NaviRequestError(
				naviError("NOT_ATTACHED", "No browser target is attached; attach from the extension popup"),
			);
		}
		if (!capabilityGranted(grant, capability)) {
			throw new NaviRequestError(naviError("UNSUPPORTED_OP", `The user did not grant the ${capability} capability`));
		}
	}

	private requireObservation(op: string): string {
		if (this.observationIdValue === undefined) {
			throw new NaviRequestError(naviError("STALE_OBSERVATION", `${op} needs a current observation; observe again`));
		}
		return this.observationIdValue;
	}

	private emitStatus(): void {
		const status = this.status();
		for (const listener of this.listeners) listener(status);
	}
}

/** Re-exported for callers that build a client with the default limits. */
export const CLIENT_DEFAULT_LIMITS = DEFAULT_CONNECTION_LIMITS;

export type { ConnectionLimits, ConnectionState };
