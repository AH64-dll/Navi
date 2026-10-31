/**
 * Trusted popup UI.
 *
 * It renders worker state and sends explicit user intent. Approval never renders in a page, and the popup
 * holds no authority of its own: the worker authenticates the sender and decides what a grant means.
 */
import type { Capability } from "../../src/protocol/types.ts";
import { chromeSendMessage } from "../platform/chrome.ts";
import { type PopupRequest, type PopupState, parsePopupState } from "../worker/local-channel.ts";

const PHASE_TEXT: Record<PopupState["phase"], string> = {
	detached: "Not attached",
	attaching: "Attaching...",
	attached: "Attached to one document",
};

function element<T extends HTMLElement>(id: string): T {
	const found = document.getElementById(id);
	if (!found) throw new Error(`popup is missing #${id}`);
	return found as T;
}

const phaseText = element<HTMLParagraphElement>("phase");
const targetText = element<HTMLParagraphElement>("target");
const errorText = element<HTMLParagraphElement>("error");
const observeBox = element<HTMLInputElement>("cap-observe");
const focusBox = element<HTMLInputElement>("cap-focus");
const scrollBox = element<HTMLInputElement>("cap-scroll");
const attachButton = element<HTMLButtonElement>("attach");
const detachButton = element<HTMLButtonElement>("detach");

function requestedCapabilities(): Capability[] {
	const capabilities: Capability[] = ["observe"];
	if (focusBox.checked) capabilities.push("focus");
	if (scrollBox.checked) capabilities.push("scroll");
	return capabilities;
}

function render(state: PopupState): void {
	phaseText.textContent = PHASE_TEXT[state.phase];
	targetText.textContent = state.tab
		? `${state.tab.title ? `${state.tab.title} - ` : ""}${state.tab.url ?? ""} (tab ${state.tab.tabId})`
		: "Attach the tab you are looking at. NAVI will pin it until you detach.";
	const grantCaps = new Set(state.granted);
	if (state.phase === "attached") {
		focusBox.checked = grantCaps.has("focus");
		scrollBox.checked = grantCaps.has("scroll");
	}
	observeBox.checked = true;
	attachButton.disabled = state.phase !== "detached";
	detachButton.disabled = state.phase === "detached";
	focusBox.disabled = state.phase === "attached";
	scrollBox.disabled = state.phase === "attached";
	if (state.error) {
		errorText.hidden = false;
		errorText.textContent = `${state.error.code}: ${state.error.message}`;
	} else {
		errorText.hidden = true;
		errorText.textContent = "";
	}
}

async function send(request: PopupRequest): Promise<void> {
	let response: unknown;
	try {
		response = await chromeSendMessage(request);
	} catch {
		errorText.hidden = false;
		errorText.textContent = "the NAVI worker is not running";
		return;
	}
	try {
		render(parsePopupState(response));
	} catch {
		// A response this popup cannot read is a wiring bug, not user-recoverable.
		errorText.hidden = false;
		errorText.textContent = "unexpected response from the NAVI worker";
	}
}

attachButton.addEventListener("click", () => {
	void send({ t: "popup.attach", capabilities: requestedCapabilities() });
});

detachButton.addEventListener("click", () => {
	void send({ t: "popup.detach" });
});

// The popup is short-lived: it polls while open and stops when it closes.
void send({ t: "popup.state" });
const poll = setInterval(() => {
	void send({ t: "popup.state" });
}, 1000);
window.addEventListener("unload", () => clearInterval(poll));
