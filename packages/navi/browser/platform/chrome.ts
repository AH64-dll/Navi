/**
 * Adapter from the real `chrome.*` namespace to the narrow `Platform` facade.
 *
 * NAVI does not ship Chrome type definitions, so the shapes are asserted in this file only and every
 * other module stays structurally typed. Missing or mismatched members fail loudly instead of degrading
 * silently.
 */
import type {
	EventSink,
	InjectionResult,
	NativePortApi,
	Platform,
	RuntimePortApi,
	TabInfo,
	WindowInfo,
} from "./types.ts";

type ChromeLike = {
	runtime?: {
		id?: string;
		getURL?(path: string): string;
		connectNative?(application: string): NativePortApi;
		connect?(options: { name: string }): RuntimePortApi;
		sendMessage?(message: unknown): Promise<unknown>;
		onMessage?: EventSink<unknown>;
		onConnect?: EventSink<unknown>;
		lastError?: { message?: string };
	};
	tabs?: {
		get(tabId: number): Promise<TabInfo>;
		query(query: { active?: boolean; lastFocusedWindow?: boolean }): Promise<TabInfo[]>;
		onRemoved?: EventSink<unknown>;
		onReplaced?: EventSink<unknown>;
		onUpdated?: EventSink<unknown>;
	};
	windows?: {
		getLastFocused(options: { populate: false }): Promise<WindowInfo>;
		onFocusChanged?: EventSink<unknown>;
	};
	scripting?: {
		executeScript(injection: {
			target: { tabId: number; frameIds: number[]; allFrames?: boolean };
			files: string[];
			injectImmediately?: boolean;
		}): Promise<InjectionResult[]>;
	};
};

function chromeApi(): ChromeLike {
	const value = (globalThis as { chrome?: ChromeLike }).chrome;
	if (!value) throw new Error("chrome extension APIs are unavailable");
	return value;
}

function requireRuntime(value: ChromeLike): NonNullable<ChromeLike["runtime"]> {
	const runtime = value.runtime;
	if (!runtime || typeof runtime.id !== "string" || runtime.id.length === 0) {
		throw new Error("chrome.runtime is unavailable");
	}
	return runtime;
}

/** Service-worker and popup platform. Requires the APIs the worker owns. */
export function chromePlatform(): Platform {
	const value = chromeApi();
	const runtime = requireRuntime(value);
	if (typeof runtime.connectNative !== "function" || !runtime.onMessage || !runtime.onConnect) {
		throw new Error("chrome.runtime messaging or nativeMessaging is unavailable");
	}
	if (!value.tabs || !value.windows || !value.scripting) {
		throw new Error("chrome.tabs, chrome.windows or chrome.scripting is unavailable");
	}
	if (typeof runtime.getURL !== "function") throw new Error("chrome.runtime.getURL is unavailable");
	return value as unknown as Platform;
}

/**
 * Content-script side: only `chrome.runtime` is available in an injected isolated world. The returned
 * port carries the browser-supplied sender identity that the worker authenticates.
 */
export function chromeRuntimePort(name: string): RuntimePortApi {
	const runtime = requireRuntime(chromeApi());
	if (typeof runtime.connect !== "function") throw new Error("chrome.runtime.connect is unavailable");
	return runtime.connect({ name });
}

/** Popup side: ask the worker for state. Rejects unknown shapes at the caller, not here. */
export async function chromeSendMessage(message: unknown): Promise<unknown> {
	const runtime = requireRuntime(chromeApi());
	const send = runtime.sendMessage;
	if (typeof send !== "function") throw new Error("chrome.runtime.sendMessage is unavailable");
	return await send(message);
}

export type { NativePortApi, RuntimePortApi };
