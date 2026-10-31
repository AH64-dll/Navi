"use strict";
(() => {
  // packages/navi/src/protocol/limits.ts
  var MAX_ENVELOPE_BYTES = 64 * 1024;
  var MAX_OBSERVATION_BYTES = 16 * 1024;
  var MAX_QUEUE_BYTES = 2 * MAX_ENVELOPE_BYTES;
  var MAX_URL_CHARS = 512;
  var MAX_SEQUENCE = Number.MAX_SAFE_INTEGER;

  // packages/navi/browser/content/collector.ts
  var FORBIDDEN_TEXT_CHARACTERS = /[\u0000-\u001F\u007F-\u009F\u202A-\u202E\u2066-\u2069]/g;
  var textEncoder = typeof TextEncoder === "function" ? new TextEncoder() : void 0;
  function sanitizeUrl(raw) {
    const trimmed = raw.trim();
    if (trimmed.length === 0) return "";
    let parsed;
    try {
      parsed = new URL(trimmed);
    } catch {
      return "";
    }
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return "";
    const path = parsed.pathname.length > 0 ? parsed.pathname : "/";
    const sanitized = stripForbiddenCharacters(`${parsed.origin}${path}`);
    return sanitized.length > MAX_URL_CHARS ? sanitized.slice(0, MAX_URL_CHARS) : sanitized;
  }
  function stripForbiddenCharacters(value) {
    return value.replace(FORBIDDEN_TEXT_CHARACTERS, "");
  }

  // apps/navi/browser/background-entry.js
  var attached = null;
  var attachedOrigin = null;
  var port = null;
  var connected = false;
  var waiting = false;
  var lastSnapshot = 0;
  async function revoke() {
    attached = null;
    const origin = attachedOrigin;
    attachedOrigin = null;
    if (origin) await browser.permissions.remove({ origins: [origin] });
  }
  var host = "dev.navi.station";
  function connect() {
    if (port) return;
    try {
      port = browser.runtime.connectNative(host);
      port.onDisconnect.addListener(() => {
        port = null;
        connected = false;
        waiting = false;
        setTimeout(connect, 2e3);
      });
      port.onMessage.addListener(async (reply) => {
        waiting = false;
        connected = reply.ok;
        if (reply.ok && reply.result) {
          const item = reply.result;
          let result;
          if (item.expires < Date.now()) result = { ok: false, error: "Action expired before delivery" };
          else try {
            result = await execute(item.action);
          } catch (error) {
            result = { ok: false, error: error.message };
          }
          synchronize({ ...result, ticket: item.ticket });
        }
      });
      synchronize();
    } catch {
      port = null;
      setTimeout(connect, 2e3);
    }
  }
  async function snapshot() {
    const tabs = (await browser.tabs.query({})).slice(0, 40).map((tab) => ({ id: tab.id, title: (tab.title || "").slice(0, 120), url: sanitizeUrl(tab.url || ""), active: tab.active }));
    let page = {};
    if (attached !== null) try {
      page = await browser.tabs.sendMessage(attached, { type: "observe" });
    } catch {
      attached = null;
    }
    return { tabs, attached, ...page };
  }
  async function synchronize(result) {
    if (!port || waiting) return;
    waiting = true;
    try {
      const params = {};
      if (result) params.result = result;
      if (Date.now() - lastSnapshot > 1200 || result) {
        params.snapshot = await snapshot();
        lastSnapshot = Date.now();
      }
      port.postMessage({ op: "browser_sync", params });
    } catch {
      waiting = false;
    }
  }
  async function execute(action) {
    if (action.type === "popup") {
      await browser.action.openPopup();
      return { ok: true, message: "Navi browser popup opened" };
    }
    if (action.type === "attach") {
      const tab2 = await browser.tabs.get(action.tab);
      if (!/^https?:/.test(tab2.url || "")) throw new Error("Select a regular web page");
      await browser.scripting.executeScript({ target: { tabId: tab2.id }, files: ["content.js"] });
      attached = tab2.id;
      attachedOrigin = new URL(tab2.url).protocol + "//" + new URL(tab2.url).hostname + "/*";
      lastSnapshot = 0;
      await browser.tabs.update(tab2.id, { active: true });
      await browser.windows.update(tab2.windowId, { focused: true });
      return { ok: true, message: "Named browser tab attached" };
    }
    if (action.type === "tab") {
      await browser.tabs.update(action.tab, { active: true });
      const tab2 = await browser.tabs.get(action.tab);
      await browser.windows.update(tab2.windowId, { focused: true });
      return { ok: true, message: "Browser tab selected" };
    }
    if (action.tab !== attached || attached === null) throw new Error("This tab is not attached for control");
    const tab = await browser.tabs.get(attached);
    await browser.tabs.update(attached, { active: true });
    await browser.windows.update(tab.windowId, { focused: true });
    return browser.tabs.sendMessage(attached, action);
  }
  browser.tabs.onRemoved.addListener((id) => {
    if (id === attached) revoke();
  });
  browser.tabs.onUpdated.addListener((id, changes) => {
    if (id === attached && (changes.status === "loading" || changes.url)) revoke();
  });
  browser.runtime.onMessage.addListener(async (message, sender) => {
    if (sender.tab || sender.id !== browser.runtime.id || sender.url !== browser.runtime.getURL("popup.html")) return void 0;
    if (message.type === "status") return { connected, attached };
    if (message.type === "detach") {
      await revoke();
      return { connected, attached };
    }
    if (message.type === "attach") {
      const tabs = await browser.tabs.query({ active: true, currentWindow: true });
      const tab = tabs[0];
      if (!tab || !/^https?:/.test(tab.url || "")) throw new Error("Select a regular web page first");
      await browser.scripting.executeScript({ target: { tabId: tab.id }, files: ["content.js"] });
      attached = tab.id;
      attachedOrigin = new URL(tab.url).protocol + "//" + new URL(tab.url).hostname + "/*";
      lastSnapshot = 0;
      return { connected, attached };
    }
    return void 0;
  });
  setInterval(() => synchronize(), 350);
  connect();
})();
