/**
 * Attach flow for one user-chosen tab: resolve the tab, refuse unsupported documents, inject the content
 * script and capture the browser-supplied document identity.
 *
 * Document identity comes from the actual `executeScript` result, never from the page, a message payload or
 * model input. The content port is matched against that identity by the worker; this module only produces
 * the identity and the port wait.
 */
import { MAX_ID_CHARS } from "../../src/protocol/limits.ts";
import type { WireErrorCode } from "../../src/protocol/types.ts";
import type { InjectionResult, MessageSender, Platform, RuntimePortApi, TabInfo } from "../platform/types.ts";
import { CONTENT_PORT_NAME, CONTENT_SCRIPT_FILE } from "./local-channel.ts";

/** Failure with a protocol error code, so callers never invent their own vocabulary. */
export class AttachError extends Error {
	readonly code: WireErrorCode;

	constructor(code: WireErrorCode, message: string) {
		super(message);
		this.name = "AttachError";
		this.code = code;
	}
}

/** Only ordinary web documents are supported in the foundation. */
const SUPPORTED_PROTOCOLS = ["http:", "https:"];

/** The active tab of the last-focused window. The popup never passes a tab id of its own. */
export async function resolveActiveTab(platform: Platform): Promise<TabInfo> {
	let tabs: TabInfo[];
	try {
		tabs = await platform.tabs.query({ active: true, lastFocusedWindow: true });
	} catch {
		throw new AttachError("NOT_ATTACHED", "could not read the active tab");
	}
	const tab = tabs[0];
	if (!tab || typeof tab.id !== "number") {
		throw new AttachError("NOT_ATTACHED", "no active tab to attach");
	}
	if (tab.url === undefined) {
		throw new AttachError("CAPTURE_FAILED", "reopen the NAVI popup on the tab you want to attach");
	}
	return tab;
}

/** Rejects documents the extension must not or cannot read. */
export function assertSupportedTab(tab: TabInfo): void {
	let parsed: URL;
	try {
		parsed = new URL(tab.url ?? "");
	} catch {
		throw new AttachError("CAPTURE_FAILED", "the tab has no readable web address");
	}
	if (!SUPPORTED_PROTOCOLS.includes(parsed.protocol)) {
		throw new AttachError("CAPTURE_FAILED", `unsupported page type: ${parsed.protocol}`);
	}
}

/** Display URL for the trusted popup only: no credentials, query or fragment. */
export function displayUrl(raw: string | undefined): string | undefined {
	if (raw === undefined) return undefined;
	try {
		const parsed = new URL(raw);
		if (!SUPPORTED_PROTOCOLS.includes(parsed.protocol)) return undefined;
		return `${parsed.origin}${parsed.pathname}`.slice(0, 512);
	} catch {
		return undefined;
	}
}

/**
 * Injects the packaged content script into the top frame and returns the browser-supplied document id.
 * Fails closed when the injection result does not identify exactly one top-frame document.
 */
export async function injectContentScript(platform: Platform, tabId: number): Promise<string> {
	let results: InjectionResult[];
	try {
		results = await platform.scripting.executeScript({
			target: { tabId, frameIds: [0] },
			files: [CONTENT_SCRIPT_FILE],
			injectImmediately: true,
		});
	} catch {
		throw new AttachError("CAPTURE_FAILED", "the page blocked injection; reopen the popup on that tab and retry");
	}
	const result = results.length === 1 ? results[0] : undefined;
	if (!result || result.frameId !== 0) {
		throw new AttachError("CAPTURE_FAILED", "injection did not report exactly one top-frame document");
	}
	const documentId = result.documentId;
	if (typeof documentId !== "string" || documentId.length === 0 || documentId.length > MAX_ID_CHARS) {
		throw new AttachError("CAPTURE_FAILED", "the browser did not report a usable document id");
	}
	return documentId;
}

/** Browser-supplied sender check for a content port. Pages cannot reach this channel. */
export function contentPortMatches(
	platform: Platform,
	sender: MessageSender | undefined,
	binding: { tabId: number; documentId: string },
): boolean {
	if (!sender) return false;
	if (sender.id !== platform.runtime.id) return false;
	if (sender.frameId !== 0) return false;
	if (sender.tab?.id !== binding.tabId) return false;
	// A content port is authenticated by identity, not by the injection result alone: the sender's own
	// browser-supplied document id must be present and equal. Missing identity fails closed.
	return sender.documentId === binding.documentId;
}

/** Newest ports kept while an attach is still resolving. Anything older is dropped, not queued. */
const MAX_BUFFERED_CONTENT_PORTS = 4;

/** Buffers content ports until the attach flow knows the injected document id. */
export class ContentPortBuffer {
	private readonly ports: RuntimePortApi[] = [];

	add(port: RuntimePortApi): void {
		this.ports.push(port);
		while (this.ports.length > MAX_BUFFERED_CONTENT_PORTS) {
			const dropped = this.ports.shift();
			try {
				dropped?.disconnect();
			} catch {
				// Already gone.
			}
		}
	}

	/** Removes and returns the newest port that matches, if any. */
	take(platform: Platform, binding: { tabId: number; documentId: string }): RuntimePortApi | null {
		for (let index = this.ports.length - 1; index >= 0; index--) {
			const port = this.ports[index];
			if (!port) continue;
			if (port.name !== CONTENT_PORT_NAME) {
				this.ports.splice(index, 1);
				continue;
			}
			if (!contentPortMatches(platform, port.sender, binding)) continue;
			this.ports.splice(index, 1);
			return port;
		}
		return null;
	}

	remove(port: RuntimePortApi): void {
		const index = this.ports.indexOf(port);
		if (index >= 0) this.ports.splice(index, 1);
	}

	/** Drops every buffered port. Used when an attach ends: an unadopted port is never reused. */
	clear(): void {
		for (const port of this.ports.splice(0, this.ports.length)) {
			try {
				port.disconnect();
			} catch {
				// Already gone.
			}
		}
	}
}
