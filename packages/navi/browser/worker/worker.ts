/**
 * Browser worker: grant authority, request routing and revocation.
 *
 * The worker is the only privileged component. It decides whether a CLI request may touch a document,
 * routes allowed work to the bound content port, and ends the grant conservatively.
 *
 * Handshake, sequence and request correlation come from `src/core/connection.ts`; grant state comes from
 * `src/core/grants.ts`. This module adds the browser-specific authority: authentic sender checks, the
 * tab/document binding, the active-tab guard and revocation triggers. It never reads page text, never
 * talks to a model and never invents browser authority.
 */
import type { ReadyInfo, RequestHandler } from "../../src/core/connection.ts";
import { naviError } from "../../src/core/errors.ts";
import { capabilityGranted, GrantManager } from "../../src/core/grants.ts";
import type { Clock } from "../../src/core/ports.ts";
import {
	INTERACTION_TIMEOUT_MS,
	MAX_OBSERVATION_BYTES,
	MAX_SCROLL_DELTA,
	OBSERVE_TIMEOUT_MS,
} from "../../src/protocol/limits.ts";
import type {
	ByeReason,
	Capability,
	EffectOutcome,
	EventFrame,
	Grant,
	GrantChangeReason,
	NaviError,
	Observation,
	ResultPayload,
} from "../../src/protocol/types.ts";
import { utf8ByteLength } from "../../src/protocol/validate.ts";
import type { MessageSender, Platform, RuntimePortApi, TabChangeInfo } from "../platform/types.ts";
import {
	AttachError,
	assertSupportedTab,
	ContentPortBuffer,
	contentPortMatches,
	displayUrl,
	injectContentScript,
	resolveActiveTab,
} from "./attach.ts";
import { createGrant, GRANT_IDLE_MS, selectCapabilities } from "./grant.ts";
import {
	CONTENT_PORT_NAME,
	type ContentInbound,
	LocalMessageError,
	type ObservationDraft,
	POPUP_PATH,
	type PopupResponse,
	type PopupState,
	parseContentOutbound,
	parsePopupRequest,
} from "./local-channel.ts";
import { type NativeSession, openNativeSession } from "./native-session.ts";

/** How long the attach flow waits for the injected content script to open its port. */
export const CONTENT_BIND_TIMEOUT_MS = 2_000;

export type WorkerDeps = {
	platform: Platform;
	clock: Clock;
	/** Unique id source. Must return protocol-safe ids (<= 64 characters). */
	newId: () => string;
};

export type Worker = {
	handlePopupMessage(message: unknown, sender: MessageSender): Promise<PopupResponse>;
	handleContentPort(port: RuntimePortApi): void;
	handleTabRemoved(tabId: number): void;
	handleTabReplaced(removedTabId: number): void;
	handleTabUpdated(tabId: number, changeInfo: TabChangeInfo): void;
	state(): PopupState;
	dispose(): void;
};

/** One content-script call. Only one may be outstanding: references stay unambiguous. */
type ContentCall = {
	requestId: string;
	kind: "observe" | "focus" | "scroll";
	finish(outcome: ContentOutcome): void;
};

type ContentOutcome =
	| { ok: true; kind: "observation"; draft: ObservationDraft }
	| { ok: true; kind: "action"; outcome: EffectOutcome; scrollX?: number; scrollY?: number }
	| { ok: false; error: NaviError };

type Waiter = {
	binding: { tabId: number; documentId: string };
	resolve(port: RuntimePortApi): void;
	reject(error: AttachError): void;
	cancelTimer(): void;
};

export function createWorker(deps: WorkerDeps): Worker {
	const { platform, clock, newId } = deps;
	const grants = new GrantManager({ clock, expiryMs: GRANT_IDLE_MS });

	let phase: PopupState["phase"] = "detached";
	let requested: Capability[] = ["observe"];
	let attachedTab: PopupState["tab"] | null = null;
	let lastError: NaviError | null = null;
	let session: NativeSession | null = null;
	let contentPort: RuntimePortApi | null = null;
	let contentCall: ContentCall | null = null;
	let expiryTimer: (() => void) | null = null;
	let waiter: Waiter | null = null;
	let ready: { resolve(info: ReadyInfo): void; reject(error: NaviError): void } | null = null;
	let epoch = 0;
	/** Tab of the attach in progress: only that tab may have a port buffered before binding. */
	let attachingTabId: number | null = null;
	/** Bumped on every attach attempt and revoke so late async work cannot adopt stale state. */
	let attempt = 0;
	const portBuffer = new ContentPortBuffer();

	function clearExpiry(): void {
		expiryTimer?.();
		expiryTimer = null;
	}

	function armExpiry(): void {
		clearExpiry();
		expiryTimer = clock.schedule(() => {
			expiryTimer = null;
			revoke("expired");
		}, GRANT_IDLE_MS);
	}

	function safeDisconnect(port: RuntimePortApi): void {
		try {
			port.disconnect();
		} catch {
			// Port already gone.
		}
	}

	function settleContentCall(outcome: ContentOutcome): void {
		const call = contentCall;
		if (!call) return;
		contentCall = null;
		call.finish(outcome);
	}

	/** Ends the grant, drops references, cancels in-flight work and closes the native session. */
	function revoke(reason: GrantChangeReason, byeReason?: ByeReason): void {
		attempt++;
		clearExpiry();
		attachingTabId = null;
		portBuffer.clear();
		settleContentCall({ ok: false, error: naviError("CANCELLED", "the grant ended before the operation finished") });
		const port = contentPort;
		contentPort = null;
		if (port) {
			try {
				port.postMessage({ t: "content.detach" } satisfies ContentInbound);
			} catch {
				// The page channel is gone; the revocation still stands.
			}
			safeDisconnect(port);
		}
		const currentWaiter = waiter;
		waiter = null;
		currentWaiter?.cancelTimer();
		currentWaiter?.reject(new AttachError("CANCELLED", "the grant ended during attach"));
		const hadGrant = grants.grant !== null;
		grants.invalidate();
		attachedTab = null;
		requested = ["observe"];
		phase = "detached";
		ready = null;
		const active = session;
		session = null;
		if (active) {
			// `sendEvent` is a no-op once the connection is closed, so a dead CLI port stays silent.
			if (hadGrant) active.connection.sendEvent({ grant: null, reason });
			active.close(
				byeReason ?? (reason === "disconnected" ? "disconnected" : reason === "detached" ? "detached" : "closed"),
			);
		}
	}

	/** Bounded, already-wire-shaped NaviError values pass through unchanged. */
	function isNaviError(value: unknown): value is NaviError {
		if (typeof value !== "object" || value === null) return false;
		const candidate = value as { code?: unknown; message?: unknown };
		return typeof candidate.code === "string" && typeof candidate.message === "string";
	}

	function errorFrom(error: unknown): NaviError {
		if (isNaviError(error)) return naviError(error.code, error.message);
		if (error instanceof AttachError) return naviError(error.code, error.message);
		if (error instanceof LocalMessageError) return naviError("BAD_ENVELOPE", error.message);
		return naviError("INTERNAL", error instanceof Error ? error.message : "unexpected failure");
	}

	/** Starts the native session and resolves when the CLI has answered the handshake. */
	async function openSession(capabilities: Capability[]): Promise<ReadyInfo> {
		return await new Promise<ReadyInfo>((resolve, reject) => {
			ready = { resolve, reject };
			const opened = openNativeSession({
				platform,
				clock,
				capabilities,
				newRequestId: () => newId(),
				events: {
					onReady(info) {
						const current = ready;
						ready = null;
						current?.resolve(info);
					},
					onStateChange(state, error) {
						if (state !== "closed") return;
						const current = ready;
						ready = null;
						const failure =
							error ??
							naviError(
								"NOT_ATTACHED",
								"NAVI connector unavailable: start the CLI and register the native host",
							);
						current?.reject(failure);
						if (state === "closed" && session !== null && grants.grant !== null) revoke("disconnected");
					},
					onEvent(event) {
						onCliEvent(event);
					},
					handleRequest: handleRequest,
				},
			});
			if ("code" in opened) {
				ready = null;
				reject(opened);
				return;
			}
			session = opened;
		});
	}

	function waitForContentPort(binding: { tabId: number; documentId: string }): Promise<RuntimePortApi> {
		const buffered = portBuffer.take(platform, binding);
		if (buffered) return Promise.resolve(buffered);
		return new Promise<RuntimePortApi>((resolve, reject) => {
			const pending: Waiter = {
				binding,
				cancelTimer: () => undefined,
				resolve(port) {
					pending.cancelTimer();
					waiter = null;
					resolve(port);
				},
				reject(error) {
					pending.cancelTimer();
					waiter = null;
					reject(error);
				},
			};
			pending.cancelTimer = clock.schedule(() => {
				// A port that never matched this document is never adopted later.
				portBuffer.clear();
				pending.reject(new AttachError("CAPTURE_FAILED", "the page did not accept the NAVI content script"));
			}, CONTENT_BIND_TIMEOUT_MS);
			waiter = pending;
		});
	}

	function bindContentPort(port: RuntimePortApi): void {
		const previous = contentPort;
		contentPort = port;
		if (previous && previous !== port) safeDisconnect(previous);
	}

	async function attach(capabilities: Capability[]): Promise<void> {
		if (phase !== "detached") {
			lastError = naviError("BUSY", "detach before attaching again");
			return;
		}
		const currentAttempt = ++attempt;
		phase = "attaching";
		lastError = null;
		requested = selectCapabilities(capabilities);
		try {
			const tab = await resolveActiveTab(platform);
			assertSupportedTab(tab);
			const tabId = tab.id as number;
			attachingTabId = tabId;
			const info = await openSession(requested);
			if (currentAttempt !== attempt) return;
			if (!info.capabilities.includes("observe")) {
				throw new AttachError("BAD_ARGUMENT", "the CLI did not confirm the observe capability");
			}
			const granted = requested.filter((capability) => info.capabilities.includes(capability));
			const documentId = await injectContentScript(platform, tabId);
			if (currentAttempt !== attempt) return;
			const port = await waitForContentPort({ tabId, documentId });
			if (currentAttempt !== attempt) {
				safeDisconnect(port);
				return;
			}
			epoch++;
			const grant: Grant = createGrant({
				grantId: newId(),
				tabId,
				documentId,
				pageEpoch: epoch,
				capabilities: granted,
			});
			grants.attach(grant);
			attachedTab = {
				tabId,
				title: typeof tab.title === "string" ? tab.title.slice(0, 200) : undefined,
				url: displayUrl(tab.url),
			};
			bindContentPort(port);
			attachingTabId = null;
			phase = "attached";
			armExpiry();
			session?.connection.sendEvent({ grant, reason: "attached" });
		} catch (error) {
			if (currentAttempt !== attempt) return;
			lastError = errorFrom(error);
			revoke("detached");
			lastError = errorFrom(error);
		}
	}

	async function isTargetActive(tabId: number): Promise<boolean> {
		try {
			const tabs = await platform.tabs.query({ active: true, lastFocusedWindow: true });
			const active = tabs[0];
			if (!active || active.id !== tabId || active.active !== true) return false;
			const focused = await platform.windows.getLastFocused({ populate: false });
			return focused.focused === true;
		} catch {
			return false;
		}
	}

	function callContent(message: ContentInbound, signal: AbortSignal, timeoutMs: number): Promise<ContentOutcome> {
		const port = contentPort;
		if (!port) {
			return Promise.resolve({ ok: false, error: naviError("NOT_ATTACHED", "the page channel is not open") });
		}
		if (contentCall) {
			return Promise.resolve({ ok: false, error: naviError("BUSY", "one browser operation is already in flight") });
		}
		const kind = message.t === "content.observe" ? "observe" : message.t === "content.focus" ? "focus" : "scroll";
		const requestId = newId();
		return new Promise<ContentOutcome>((resolve) => {
			let finished = false;
			const finish = (outcome: ContentOutcome): void => {
				if (finished) return;
				finished = true;
				if (contentCall === call) contentCall = null;
				cancelTimer();
				signal.removeEventListener("abort", onAbort);
				resolve(outcome);
			};
			const stop = (error: NaviError): void => {
				if (finished) return;
				try {
					port.postMessage({ t: "content.cancel", requestId } satisfies ContentInbound);
				} catch {
					// The page channel is gone; the outcome below is still reported.
				}
				finish({ ok: false, error });
			};
			const onAbort = (): void => {
				stop(
					naviError(
						kind === "observe" ? "CANCELLED" : "OUTCOME_UNKNOWN",
						kind === "observe"
							? "the request was cancelled"
							: "cancelled after dispatch; the effect may have been applied",
					),
				);
			};
			const cancelTimer = clock.schedule(() => {
				stop(
					naviError(
						kind === "observe" ? "DEADLINE_EXCEEDED" : "OUTCOME_UNKNOWN",
						kind === "observe" ? "the page did not answer in time" : "the page did not confirm the outcome",
					),
				);
			}, timeoutMs);
			const call: ContentCall = { requestId, kind, finish };
			contentCall = call;
			signal.addEventListener("abort", onAbort, { once: true });
			try {
				port.postMessage({ ...message, requestId });
			} catch {
				finish({ ok: false, error: naviError("INTERNAL", "the page channel rejected the request") });
			}
		});
	}

	function scrollArgument(payload: {
		ref?: string;
		align?: "start" | "center" | "end";
		deltaX?: number;
		deltaY?: number;
	}): NaviError | null {
		const hasDelta = payload.deltaX !== undefined || payload.deltaY !== undefined;
		if (payload.ref !== undefined && hasDelta) {
			return naviError("BAD_ARGUMENT", "a reference scroll cannot include a delta");
		}
		if (payload.ref === undefined && !hasDelta) {
			return naviError("BAD_ARGUMENT", "a page scroll requires a bounded delta");
		}
		const deltas = [payload.deltaX, payload.deltaY].filter((value): value is number => value !== undefined);
		if (deltas.some((value) => Math.abs(value) > MAX_SCROLL_DELTA)) {
			return naviError("BAD_ARGUMENT", `scroll delta exceeds ${MAX_SCROLL_DELTA}`);
		}
		return null;
	}

	const handleRequest: RequestHandler = async (frame, signal) => {
		const payload = frame.payload;
		if (payload.op === "cancel") {
			// The connection owns cancel routing; a handler never receives one.
			return naviError("UNSUPPORTED_OP", "cancel is handled by the connection");
		}
		const stale = grants.checkBinding(payload.binding);
		if (stale) return stale;
		const active = grants.grant;
		if (!active) return naviError("NOT_ATTACHED", "no document is attached");
		if (payload.op === "observe") {
			if (!capabilityGranted(active, "observe")) return naviError("UNSUPPORTED_OP", "observe was not granted");
			const observationId = newId();
			const outcome = await callContent(
				{ t: "content.observe", requestId: "", observationId, binding: active.binding },
				signal,
				OBSERVE_TIMEOUT_MS,
			);
			if (!outcome.ok) return outcome.error;
			if (outcome.kind !== "observation") return naviError("INTERNAL", "the page answered the wrong kind of result");
			const observation: Observation = { ...outcome.draft, binding: active.binding };
			if (utf8ByteLength(observation) > MAX_OBSERVATION_BYTES) {
				// The receiver would treat an over-budget observation as fatal; refuse it here instead.
				return naviError("TOO_LARGE", "the page observation exceeds the byte budget");
			}
			const noted = grants.noteObservation(observation);
			if (noted) return noted;
			const result: ResultPayload = { op: "observe", observation };
			return result;
		}
		if (!capabilityGranted(active, payload.op)) {
			return naviError("UNSUPPORTED_OP", `${payload.op} was not granted`);
		}
		if (payload.op === "scroll") {
			const argumentError = scrollArgument(payload);
			if (argumentError) return argumentError;
		}
		const staleObservation = grants.checkObservation(payload.binding, payload.observationId);
		if (staleObservation) return staleObservation;
		if (!(await isTargetActive(active.binding.tabId))) {
			return naviError("TAB_NOT_ACTIVE", "the attached tab is not the active tab in the focused window");
		}
		if (grants.grant !== active || contentPort === null) {
			return naviError("NOT_ATTACHED", "the grant ended while the request was being checked");
		}
		if (payload.op === "focus") {
			const outcome = await callContent(
				{ t: "content.focus", requestId: "", ref: payload.ref },
				signal,
				INTERACTION_TIMEOUT_MS,
			);
			if (!outcome.ok) return outcome.error;
			if (outcome.kind !== "action") return naviError("INTERNAL", "the page answered the wrong kind of result");
			return { op: "focus", outcome: outcome.outcome };
		}
		const message: ContentInbound = { t: "content.scroll", requestId: "" };
		if (payload.ref !== undefined) message.ref = payload.ref;
		if (payload.align !== undefined) message.align = payload.align;
		if (payload.deltaX !== undefined) message.deltaX = payload.deltaX;
		if (payload.deltaY !== undefined) message.deltaY = payload.deltaY;
		const outcome = await callContent(message, signal, INTERACTION_TIMEOUT_MS);
		if (!outcome.ok) return outcome.error;
		if (outcome.kind !== "action") return naviError("INTERNAL", "the page answered the wrong kind of result");
		const result: ResultPayload = { op: "scroll", outcome: outcome.outcome };
		if (outcome.scrollX !== undefined) result.scrollX = outcome.scrollX;
		if (outcome.scrollY !== undefined) result.scrollY = outcome.scrollY;
		return result;
	};

	function onCliEvent(event: EventFrame): void {
		// The extension owns grant state. A CLI frame may carry an invalidation, never a grant.
		if (event.payload.grant !== null) {
			revoke("disconnected", "protocol-error");
			lastError = naviError("BAD_ENVELOPE", "the CLI tried to set a grant");
		}
	}

	function onContentMessage(port: RuntimePortApi, value: unknown): void {
		if (port !== contentPort) return;
		let message: ReturnType<typeof parseContentOutbound>;
		try {
			message = parseContentOutbound(value);
		} catch {
			// A malformed local message cannot change authority or state.
			return;
		}
		switch (message.t) {
			case "content.hello": {
				// Informational only: the bound port, not this message, carries authority.
				return;
			}
			case "content.navigated": {
				revoke("navigated");
				return;
			}
			case "content.observation": {
				const call = contentCall;
				if (!call || call.kind !== "observe" || call.requestId !== message.requestId) return;
				settleContentCall({ ok: true, kind: "observation", draft: message.draft });
				return;
			}
			case "content.action": {
				const call = contentCall;
				if (!call || call.kind === "observe" || call.requestId !== message.requestId) return;
				const outcome: ContentOutcome = { ok: true, kind: "action", outcome: message.outcome };
				if (message.scrollX !== undefined) outcome.scrollX = message.scrollX;
				if (message.scrollY !== undefined) outcome.scrollY = message.scrollY;
				settleContentCall(outcome);
				return;
			}
			case "content.failure": {
				const call = contentCall;
				if (!call || call.requestId !== message.requestId) return;
				settleContentCall({ ok: false, error: naviError(message.code, message.message) });
				return;
			}
		}
	}

	async function revokeAfterPortLoss(): Promise<void> {
		// The tab lookup is asynchronous: a new attach may already have completed. Capturing the attempt
		// keeps this stale continuation from killing a grant it does not own.
		const observedAttempt = attempt;
		const tabId = grants.grant?.binding.tabId;
		if (tabId !== undefined) {
			try {
				await platform.tabs.get(tabId);
			} catch {
				if (attempt === observedAttempt) revoke("closed");
				return;
			}
		}
		if (attempt === observedAttempt) revoke("navigated");
	}

	function onContentPortDisconnect(port: RuntimePortApi): void {
		if (port !== contentPort) return;
		contentPort = null;
		void revokeAfterPortLoss();
	}

	function isPopupSender(sender: MessageSender): boolean {
		return (
			sender.id === platform.runtime.id &&
			sender.tab === undefined &&
			sender.url === platform.runtime.getURL(POPUP_PATH)
		);
	}

	function state(): PopupState {
		const grant = grants.grant;
		const result: PopupState = { phase, requested: [...requested], granted: grant ? [...grant.capabilities] : [] };
		if (attachedTab) result.tab = { ...attachedTab };
		const observationId = grants.snapshot().observationId;
		if (observationId !== undefined) result.observationId = observationId;
		if (lastError) result.error = { ...lastError };
		return result;
	}

	return {
		async handlePopupMessage(message, sender) {
			if (!isPopupSender(sender)) {
				// Answer without any state: an unrecognized sender learns nothing.
				return {
					t: "popup.state",
					state: {
						phase: "detached",
						requested: ["observe"],
						granted: [],
						error: naviError("INTERNAL", "rejected a message from an unrecognized sender"),
					},
				};
			}
			let request: ReturnType<typeof parsePopupRequest>;
			try {
				request = parsePopupRequest(message);
			} catch (error) {
				lastError = errorFrom(error);
				return { t: "popup.state", state: state() };
			}
			if (request.t === "popup.attach") {
				await attach(request.capabilities);
			} else if (request.t === "popup.detach") {
				revoke("detached");
			}
			return { t: "popup.state", state: state() };
		},
		handleContentPort(port) {
			if (port.name !== CONTENT_PORT_NAME) {
				safeDisconnect(port);
				return;
			}
			const sender = port.sender;
			if (
				!sender ||
				sender.id !== platform.runtime.id ||
				sender.frameId !== 0 ||
				typeof sender.tab?.id !== "number"
			) {
				safeDisconnect(port);
				return;
			}
			port.onMessage.addListener((value) => onContentMessage(port, value));
			port.onDisconnect.addListener(() => onContentPortDisconnect(port));
			// Only the tab of an attach in progress may have a port waiting to learn its document id.
			if (phase === "attaching" && attachingTabId === sender.tab.id) {
				const currentWaiter = waiter;
				if (currentWaiter && contentPortMatches(platform, sender, currentWaiter.binding)) {
					currentWaiter.resolve(port);
					return;
				}
				portBuffer.add(port);
				return;
			}
			const grant = grants.grant;
			if (grant && contentPortMatches(platform, sender, grant.binding)) {
				// A second injection into the attached document: adopt it and drop older refs.
				bindContentPort(port);
				grants.attach(grant);
				return;
			}
			// Nothing is attaching for this tab: an unadopted port is closed, never reused later.
			safeDisconnect(port);
		},
		handleTabRemoved(tabId) {
			if (grants.grant?.binding.tabId === tabId) revoke("closed");
		},
		handleTabReplaced(removedTabId) {
			if (grants.grant?.binding.tabId === removedTabId) revoke("navigated");
		},
		handleTabUpdated(tabId, changeInfo) {
			if (grants.grant?.binding.tabId !== tabId) return;
			if (changeInfo.discarded === true) revoke("navigated");
		},
		state,
		dispose() {
			revoke("worker-restart");
		},
	};
}
