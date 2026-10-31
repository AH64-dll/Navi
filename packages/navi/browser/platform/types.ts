/**
 * Narrow, structurally typed browser platform facade.
 *
 * NAVI deliberately avoids a bundled `@types/chrome` dependency: this file declares only the members the
 * foundation actually uses, so the worker logic is testable against a fake and the extension cannot quietly
 * grow a new browser API. Field types follow the documented MV3 shapes; unverified members are marked.
 */

/** Everything the extension is allowed to do to the browser, as one injectable port. */
export type Platform = {
	runtime: RuntimeApi;
	tabs: TabsApi;
	windows: WindowsApi;
	scripting: ScriptingApi;
};

export type RuntimeApi = {
	/** The extension's own ID. Used to authenticate local message senders. */
	id: string;
	/** Absolute `chrome-extension://<id>/` URL for an extension-relative path. */
	getURL(path: string): string;
	connectNative(application: string): NativePortApi;
	onMessage: EventSource<RuntimeMessageListener>;
	onConnect: EventSource<RuntimePortListener>;
	/** Set after a failed call, for example a rejected `connectNative`. */
	lastError?: { message?: string };
};

export type EventSource<T> = {
	addListener(listener: T): void;
};

export type RuntimeMessageListener = (
	message: unknown,
	sender: MessageSender,
	sendResponse: (response: unknown) => void,
) => boolean | undefined;

export type RuntimePortListener = (port: RuntimePortApi) => void;

/** Browser-supplied sender identity. Never accepted from page or message content. */
export type MessageSender = {
	id?: string;
	url?: string;
	tab?: { id?: number; windowId?: number; active?: boolean };
	frameId?: number;
	/** Document the message came from. Present for content-script ports in supported Chrome versions. */
	documentId?: string;
};

/** A port owned by an extension page or content script. Messages are JSON values, not strings. */
export type RuntimePortApi = {
	name: string;
	sender?: MessageSender;
	postMessage(message: unknown): void;
	disconnect(): void;
	onMessage: EventSink<(message: unknown) => void>;
	onDisconnect: EventSink<() => void>;
};

/** The native messaging port. Chrome serializes JSON values on this hop; NAVI never double-encodes. */
export type NativePortApi = {
	postMessage(message: unknown): void;
	disconnect(): void;
	onMessage: EventSink<(message: unknown) => void>;
	onDisconnect: EventSink<() => void>;
};

export type EventSink<T> = {
	addListener(listener: T): void;
};

export type TabInfo = {
	/** Absent when the tab has no permitted identity, for example without `activeTab`. */
	id?: number;
	windowId?: number;
	active?: boolean;
	discarded?: boolean;
	/** Available only with `activeTab`, host permission or the `tabs` permission. */
	url?: string;
	title?: string;
};

export type TabChangeInfo = {
	status?: string;
	url?: string;
	discarded?: boolean;
};

export type TabsApi = {
	get(tabId: number): Promise<TabInfo>;
	query(query: { active?: boolean; lastFocusedWindow?: boolean }): Promise<TabInfo[]>;
	onRemoved: EventSink<(tabId: number) => void>;
	onReplaced: EventSink<(addedTabId: number, removedTabId: number) => void>;
	onUpdated: EventSink<(tabId: number, changeInfo: TabChangeInfo, tab: TabInfo) => void>;
};

export type WindowInfo = {
	id?: number;
	focused?: boolean;
};

export type WindowsApi = {
	getLastFocused(options: { populate: false }): Promise<WindowInfo>;
	onFocusChanged: EventSink<(windowId: number) => void>;
};

export type InjectionTarget = {
	tabId: number;
	frameIds: number[];
	allFrames?: boolean;
};

export type InjectionResult = {
	/** Browser-supplied identity of the document that received the script. */
	documentId?: string;
	frameId: number;
	result?: unknown;
};

export type ScriptingApi = {
	executeScript(injection: {
		target: InjectionTarget;
		files: string[];
		injectImmediately?: boolean;
	}): Promise<InjectionResult[]>;
};
