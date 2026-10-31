/**
 * Extension service-worker entry.
 *
 * Wiring only: it connects the Chrome event sources to one worker instance. All authority checks live in
 * `worker.ts`, `attach.ts` and `grant.ts`, so they can be tested without a browser.
 */
import { systemClock } from "../../src/core/ports.ts";
import { chromePlatform } from "../platform/chrome.ts";
import { createWorker } from "./worker.ts";

const platform = chromePlatform();

const worker = createWorker({
	platform,
	clock: systemClock,
	newId: () => crypto.randomUUID(),
});

platform.runtime.onMessage.addListener((message, sender, sendResponse) => {
	// The popup expects an asynchronous answer, so the message channel stays open.
	void worker
		.handlePopupMessage(message, sender)
		.then(sendResponse)
		.catch(() => sendResponse(undefined));
	return true;
});

platform.runtime.onConnect.addListener((port) => {
	worker.handleContentPort(port);
});

platform.tabs.onRemoved.addListener((tabId) => {
	worker.handleTabRemoved(tabId);
});

platform.tabs.onReplaced.addListener((_addedTabId, removedTabId) => {
	worker.handleTabReplaced(removedTabId);
});

platform.tabs.onUpdated.addListener((tabId, changeInfo) => {
	worker.handleTabUpdated(tabId, changeInfo);
});
