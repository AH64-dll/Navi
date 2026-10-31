"use strict";
(() => {
  // packages/navi/src/protocol/limits.ts
  var MAX_ENVELOPE_BYTES = 64 * 1024;
  var MAX_OBSERVATION_BYTES = 16 * 1024;
  var MAX_NODES = 100;
  var MAX_TEXT_CHARS = 200;
  var MAX_VISITED_ELEMENTS = 1500;
  var MAX_QUEUE_BYTES = 2 * MAX_ENVELOPE_BYTES;
  var MAX_URL_CHARS = 512;
  var MAX_TITLE_CHARS = 200;
  var MAX_SEQUENCE = Number.MAX_SAFE_INTEGER;

  // packages/navi/browser/content/actions.ts
  var ActionError = class extends Error {
    constructor(code, message) {
      super(message);
      this.name = "ActionError";
      this.code = code;
    }
  };
  var EXCLUDED_TAGS = /* @__PURE__ */ new Set([
    "INPUT",
    "TEXTAREA",
    "SELECT",
    "OPTION",
    "SCRIPT",
    "STYLE",
    "IFRAME",
    "OBJECT",
    "EMBED"
  ]);
  function activeDocument(doc, signal) {
    if (signal?.aborted) throw new ActionError("CANCELLED", "Action cancelled before dispatch");
    if (doc.visibilityState !== "visible" || !doc.hasFocus()) {
      throw new ActionError("TAB_NOT_ACTIVE", "The attached document is not focused and visible");
    }
  }
  function safeElement(element) {
    if (!element.isConnected || element.getRootNode() !== element.ownerDocument) {
      throw new ActionError("UNKNOWN_REF", "The observed node is no longer in the document");
    }
    let ancestor = element;
    for (let depth = 0; ancestor !== null; depth += 1) {
      if (depth >= 64) throw new ActionError("UNKNOWN_REF", "Target ancestry exceeds the action limit");
      const autocomplete = ancestor.getAttribute("autocomplete") ?? "";
      const editable = ancestor.getAttribute("contenteditable");
      if (EXCLUDED_TAGS.has(ancestor.tagName.toUpperCase()) || ancestor.hasAttribute("hidden") || ancestor.hasAttribute("inert") || ancestor.hasAttribute("disabled") || ancestor.getAttribute("aria-hidden") === "true" || ancestor.getAttribute("aria-disabled") === "true" || editable !== null && editable !== "false" || /(?:^|\s)(?:one-time-code|current-password|new-password|cc-[a-z-]+)(?:\s|$)/i.test(autocomplete))
        throw new ActionError("UNKNOWN_REF", "Target is excluded from actions");
      const view = ancestor.ownerDocument.defaultView;
      if (!view) throw new ActionError("UNKNOWN_REF", "Target has no live browser view");
      const style = view.getComputedStyle(ancestor);
      if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse" || style.opacity === "0") {
        throw new ActionError("UNKNOWN_REF", "Target is hidden");
      }
      ancestor = ancestor.parentElement;
    }
  }
  function focusElement(element, signal) {
    const doc = element.ownerDocument;
    activeDocument(doc, signal);
    safeElement(element);
    const focusable = element;
    const tag = element.tagName.toUpperCase();
    const tabIndex = element.getAttribute("tabindex");
    const eligible = tag === "BUTTON" || tag === "SUMMARY" || (tag === "A" || tag === "AREA") && element.hasAttribute("href") || tabIndex !== null && /^-?\d+$/.test(tabIndex);
    if (!eligible || typeof focusable.focus !== "function") {
      throw new ActionError("UNKNOWN_REF", "Target is not an eligible focus target");
    }
    activeDocument(doc, signal);
    try {
      focusable.focus({ preventScroll: true });
      return signal?.aborted || doc.activeElement !== element ? "unknown" : "applied";
    } catch {
      return "unknown";
    }
  }

  // packages/navi/browser/content/refs.ts
  var REF_PATTERN = /^n[1-9][0-9]{0,4}$/;
  function createRefTable(observationId) {
    const byRef = /* @__PURE__ */ new Map();
    const byElement = /* @__PURE__ */ new WeakMap();
    let next = 1;
    let revoked = false;
    return {
      observationId,
      get size() {
        return byRef.size;
      },
      put(element) {
        if (revoked) throw new Error("ref table revoked");
        const existing = byElement.get(element);
        if (existing !== void 0) return existing;
        const ref = `n${next}`;
        next += 1;
        byElement.set(element, ref);
        byRef.set(ref, new WeakRef(element));
        return ref;
      },
      get(ref) {
        if (revoked || !REF_PATTERN.test(ref)) return void 0;
        const entry = byRef.get(ref);
        if (entry === void 0) return void 0;
        const element = entry.deref();
        if (element === void 0 || element.isConnected !== true) {
          byRef.delete(ref);
          return void 0;
        }
        return element;
      },
      revoke() {
        revoked = true;
        byRef.clear();
      }
    };
  }

  // packages/navi/browser/content/collector.ts
  var COLLECT_TIME_BUDGET_MS = 250;
  var OBSERVATION_HEADROOM_BYTES = 512;
  var FORBIDDEN_TEXT_CHARACTERS = /[\u0000-\u001F\u007F-\u009F\u202A-\u202E\u2066-\u2069]/g;
  var TAG_PATTERN = /^[a-z][a-z0-9-]*$/;
  var ROLE_PATTERN = /^[a-z][a-z-]*$/;
  var MAX_TOKEN_CHARS = 64;
  var MAX_RECT_EXTENT = 1e6;
  var MAX_TEXT_NODE_READS = 4e3;
  var MAX_ELEMENT_CHECKS = 6e3;
  var MAX_LABELLEDBY_TARGETS = 3;
  var MAX_LABEL_TARGETS = 2;
  var MAX_ANCESTRY_DEPTH = 16;
  var TEXT_SAMPLE_DEPTH = 8;
  var TIME_CHECK_INTERVAL = 64;
  var SKIPPED_TAGS = /* @__PURE__ */ new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "HEAD", "META", "LINK", "BASE", "TITLE"]);
  var COVERAGE_TAGS = {
    IFRAME: "subframes",
    FRAME: "subframes",
    FRAMESET: "subframes",
    CANVAS: "canvas"
  };
  var FORM_CONTENT_TAGS = /* @__PURE__ */ new Set(["OPTION", "OPTGROUP", "DATALIST", "OUTPUT"]);
  var SECRET_AUTOCOMPLETE_TOKENS = /* @__PURE__ */ new Set([
    "one-time-code",
    "current-password",
    "new-password",
    "cc-number",
    "cc-name",
    "cc-given-name",
    "cc-additional-name",
    "cc-family-name",
    "cc-exp",
    "cc-exp-month",
    "cc-exp-year",
    "cc-csc",
    "cc-type"
  ]);
  var TAG_ROLES = {
    A: "link",
    BUTTON: "button",
    SELECT: "combobox",
    TEXTAREA: "textbox",
    SUMMARY: "button",
    IMG: "img",
    NAV: "navigation",
    MAIN: "main",
    HEADER: "banner",
    FOOTER: "contentinfo",
    ASIDE: "complementary",
    FORM: "form",
    TABLE: "table",
    UL: "list",
    OL: "list",
    LI: "listitem",
    DIALOG: "dialog"
  };
  var INPUT_TYPE_ROLES = {
    button: "button",
    submit: "button",
    reset: "button",
    checkbox: "checkbox",
    radio: "radio",
    search: "searchbox",
    range: "slider",
    number: "spinbutton",
    email: "textbox",
    tel: "textbox",
    text: "textbox",
    url: "textbox"
  };
  var INTERACTIVE_ROLES = /* @__PURE__ */ new Set([
    "button",
    "link",
    "textbox",
    "searchbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "slider",
    "spinbutton",
    "switch",
    "tab"
  ]);
  var SKIP_POLICY = { kind: "skip" };
  var textEncoder = typeof TextEncoder === "function" ? new TextEncoder() : void 0;
  function byteLength(value) {
    if (textEncoder) return textEncoder.encode(value).length;
    let bytes = 0;
    for (const character of value) {
      const codePoint = character.codePointAt(0) ?? 0;
      bytes += codePoint < 128 ? 1 : codePoint < 2048 ? 2 : codePoint < 65536 ? 3 : 4;
    }
    return bytes;
  }
  function collapse(value) {
    return value.replace(/\s+/g, " ").trim();
  }
  function cutAt(value, limit) {
    let end = value.length <= limit ? value.length : limit;
    if (end > 0) {
      const last = value.charCodeAt(end - 1);
      if (last >= 55296 && last <= 56319) end -= 1;
    }
    return value.slice(0, end);
  }
  function clampText(value, limit = MAX_TEXT_CHARS) {
    return cutAt(collapse(value), limit);
  }
  function deepFreeze(value) {
    if (value !== null && typeof value === "object") Object.freeze(value);
    return value;
  }
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
  function attribute(element, name) {
    try {
      return element.getAttribute(name);
    } catch {
      return null;
    }
  }
  function hasAttribute(element, name) {
    try {
      return element.hasAttribute(name);
    } catch {
      return false;
    }
  }
  function stripForbiddenCharacters(value) {
    return value.replace(FORBIDDEN_TEXT_CHARACTERS, "");
  }
  function safeTag(element) {
    const tag = element.localName.toLowerCase();
    return tag.length <= MAX_TOKEN_CHARS && TAG_PATTERN.test(tag) ? tag : "unknown";
  }
  function spendCheck(state) {
    state.elementChecks += 1;
    if (state.elementChecks > MAX_ELEMENT_CHECKS) {
      state.exhausted = true;
      return false;
    }
    return true;
  }
  function isHidden(element) {
    if (hasAttribute(element, "hidden")) return true;
    if (attribute(element, "aria-hidden") === "true") return true;
    const checkable = element;
    if (typeof checkable.checkVisibility === "function") {
      try {
        return checkable.checkVisibility({ checkVisibilityCSS: true }) === false;
      } catch {
      }
    }
    const view = element.ownerDocument?.defaultView ?? null;
    if (!view || typeof view.getComputedStyle !== "function") return false;
    try {
      const style = view.getComputedStyle(element);
      return style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse";
    } catch {
      return false;
    }
  }
  function isSecretAutocomplete(value) {
    if (!value) return false;
    return value.toLowerCase().split(/\s+/).some((token) => SECRET_AUTOCOMPLETE_TOKENS.has(token));
  }
  function isContentEditable(element) {
    const value = attribute(element, "contenteditable");
    if (value !== null && value.toLowerCase() !== "false") return true;
    return element.isContentEditable === true;
  }
  function elementPolicy(element, state) {
    if (!spendCheck(state)) return SKIP_POLICY;
    const tag = element.localName.toUpperCase();
    const coverage = COVERAGE_TAGS[tag];
    if (coverage !== void 0) return { kind: "skip", omitted: coverage };
    if (SKIPPED_TAGS.has(tag) || FORM_CONTENT_TAGS.has(tag)) return SKIP_POLICY;
    if (isHidden(element)) return SKIP_POLICY;
    if (tag === "INPUT") {
      const type = (attribute(element, "type") ?? "text").toLowerCase();
      if (type === "hidden" || type === "password" || type === "file" || type === "image") return SKIP_POLICY;
      if (isSecretAutocomplete(attribute(element, "autocomplete"))) return SKIP_POLICY;
      return { kind: "emit", skipSubtree: true, textAllowed: false, editable: true };
    }
    if (tag === "TEXTAREA" || tag === "SELECT") {
      return { kind: "emit", skipSubtree: true, textAllowed: false, editable: true };
    }
    if (isContentEditable(element) || hasAttribute(element, "contenteditable")) {
      return { kind: "emit", skipSubtree: true, textAllowed: false, editable: true };
    }
    const shadow = element.shadowRoot;
    if (shadow !== null && shadow !== void 0) {
      return { kind: "emit", skipSubtree: false, textAllowed: true, editable: false, omitted: "shadow" };
    }
    return { kind: "emit", skipSubtree: false, textAllowed: true, editable: false };
  }
  function excludesText(element, state) {
    const policy = elementPolicy(element, state);
    return policy.kind === "skip" || !policy.textAllowed;
  }
  function ancestryAllowsText(element, state) {
    let current = element;
    for (let depth = 0; current && depth < MAX_ANCESTRY_DEPTH; depth += 1) {
      const policy = elementPolicy(current, state);
      if (policy.kind === "skip" || !policy.textAllowed) return false;
      current = current.parentElement;
    }
    return true;
  }
  function sampleText(root, depthLimit, state) {
    const parts = [];
    let length = 0;
    const stack = [{ element: root, depth: 0, index: 0 }];
    while (stack.length > 0) {
      const frame = stack[stack.length - 1];
      const childNodes = frame.element.childNodes;
      if (frame.index >= childNodes.length) {
        stack.pop();
        continue;
      }
      const node = childNodes[frame.index];
      frame.index += 1;
      if (node.nodeType === 3) {
        state.textNodeReads += 1;
        const data = typeof node.data === "string" ? node.data : "";
        const remaining = MAX_TEXT_CHARS - length;
        if (data.length > 0 && remaining > 0) {
          const piece = cutAt(data, remaining);
          if (piece.length > 0) {
            parts.push(piece);
            length += piece.length;
          }
        }
        if (length >= MAX_TEXT_CHARS || state.textNodeReads >= MAX_TEXT_NODE_READS) break;
        continue;
      }
      if (node.nodeType !== 1 || frame.depth >= depthLimit) continue;
      const child = node;
      if (excludesText(child, state)) continue;
      stack.push({ element: child, depth: frame.depth + 1, index: 0 });
    }
    return collapse(parts.join(" "));
  }
  function deriveRole(element) {
    const declared = attribute(element, "role");
    if (declared !== null) {
      for (const token of declared.toLowerCase().split(/\s+/)) {
        if (token.length <= MAX_TOKEN_CHARS && ROLE_PATTERN.test(token)) return token;
      }
    }
    const tag = element.localName.toUpperCase();
    if (tag === "INPUT") return INPUT_TYPE_ROLES[(attribute(element, "type") ?? "text").toLowerCase()];
    if (/^H[1-6]$/.test(tag)) return "heading";
    return TAG_ROLES[tag];
  }
  function isHeading(element) {
    if (/^H[1-6]$/.test(element.localName)) return true;
    return attribute(element, "role") === "heading";
  }
  function deriveName(element, policy, state) {
    const doc = element.ownerDocument;
    const labelledBy = attribute(element, "aria-labelledby");
    if (labelledBy !== null && doc) {
      const parts = [];
      const ids = labelledBy.split(/\s+/).filter((entry) => entry.length > 0).slice(0, MAX_LABELLEDBY_TARGETS);
      for (const id of ids) {
        let target = null;
        try {
          target = doc.getElementById(id);
        } catch {
          target = null;
        }
        if (!target || target === element) continue;
        if (!ancestryAllowsText(target, state)) continue;
        const text = sampleText(target, 2, state);
        if (text.length > 0) parts.push(text);
      }
      const joined = clampText(parts.join(" "));
      if (joined.length > 0) return joined;
    }
    const ariaLabel = attribute(element, "aria-label");
    if (ariaLabel !== null && collapse(ariaLabel).length > 0) return clampText(ariaLabel);
    const labels = element.labels;
    if (labels) {
      for (const label of Array.from(labels).slice(0, MAX_LABEL_TARGETS)) {
        if (!ancestryAllowsText(label, state)) continue;
        const text = sampleText(label, 2, state);
        if (text.length > 0) return clampText(text);
      }
    }
    if (typeof element.closest === "function") {
      const wrapper = element.closest("label");
      if (wrapper && ancestryAllowsText(wrapper, state)) {
        const text = sampleText(wrapper, 2, state);
        if (text.length > 0) return clampText(text);
      }
    }
    const title = attribute(element, "title");
    if (title !== null && collapse(title).length > 0) return clampText(title);
    if (element.localName === "img") {
      const alt = attribute(element, "alt");
      if (alt !== null && collapse(alt).length > 0) return clampText(alt);
    }
    if (policy.textAllowed) {
      const text = sampleText(element, 3, state);
      if (text.length > 0) return clampText(text);
    }
    if (element.localName === "input") {
      const type = (attribute(element, "type") ?? "text").toLowerCase();
      if (["search", "text", "email", "url", "tel"].includes(type)) {
        const placeholder = attribute(element, "placeholder");
        if (placeholder !== null && collapse(placeholder).length > 0) return clampText(placeholder);
      }
    }
    return void 0;
  }
  function isFocusable(element) {
    if (hasAttribute(element, "disabled")) return false;
    if (hasAttribute(element, "tabindex")) return true;
    const tag = element.localName.toUpperCase();
    if (tag === "A" || tag === "AREA") return hasAttribute(element, "href");
    if (tag === "INPUT" || tag === "BUTTON" || tag === "SELECT" || tag === "TEXTAREA" || tag === "SUMMARY") return true;
    if (hasAttribute(element, "contenteditable")) return true;
    return element.isContentEditable === true;
  }
  function isInteractive(element) {
    if (isFocusable(element)) return true;
    const role = attribute(element, "role");
    if (role === null) return false;
    return INTERACTIVE_ROLES.has(collapse(role).toLowerCase());
  }
  function viewportOf(doc) {
    const view = doc.defaultView;
    if (view && Number.isFinite(view.innerWidth) && Number.isFinite(view.innerHeight)) {
      return { w: view.innerWidth, h: view.innerHeight };
    }
    const root = doc.documentElement;
    if (!root) return void 0;
    const width = root.clientWidth;
    const height = root.clientHeight;
    if (!Number.isFinite(width) || !Number.isFinite(height) || width === 0 || height === 0) return void 0;
    return { w: width, h: height };
  }
  function measureGeometry(element, viewport) {
    const getBounds = element.getBoundingClientRect;
    if (typeof getBounds !== "function") return { offscreen: false };
    let bounds;
    try {
      bounds = getBounds.call(element);
    } catch {
      return { offscreen: false };
    }
    const left = Number(bounds.left);
    const top = Number(bounds.top);
    const width = Number(bounds.width);
    const height = Number(bounds.height);
    if (![left, top, width, height].every((value) => Number.isFinite(value))) return { offscreen: false };
    const offset = (value) => Math.min(MAX_RECT_EXTENT, Math.max(-MAX_RECT_EXTENT, Math.round(value)));
    const extent = (value) => Math.min(MAX_RECT_EXTENT, Math.max(0, Math.round(value)));
    const rect = { x: offset(left), y: offset(top), w: extent(width), h: extent(height) };
    if (!viewport) return { rect, offscreen: false };
    const offscreen = top + height <= 0 || left + width <= 0 || top >= viewport.h || left >= viewport.w;
    return { rect, offscreen };
  }
  function ownDocumentUrl(doc) {
    const location = doc.location;
    const href = typeof location?.href === "string" ? location.href : "";
    if (href.length > 0) return href;
    return typeof doc.URL === "string" ? doc.URL : "";
  }
  function collectObservation(request) {
    const doc = request.document ?? (typeof globalThis.document === "undefined" ? void 0 : globalThis.document);
    if (!doc) return { ok: false, code: "CAPTURE_FAILED", message: "no document available" };
    const root = doc.body ?? doc.documentElement;
    if (!root) return { ok: false, code: "CAPTURE_FAILED", message: "no document root" };
    const url = sanitizeUrl(ownDocumentUrl(doc));
    if (url.length === 0) return { ok: false, code: "CAPTURE_FAILED", message: "unsupported document url" };
    const now = request.now ?? (() => typeof performance === "undefined" ? Date.now() : performance.now());
    const deadline = now() + COLLECT_TIME_BUDGET_MS;
    const state = { visited: 0, elementChecks: 0, textNodeReads: 0, exhausted: false };
    const omitted = /* @__PURE__ */ new Set();
    const candidates = [];
    const viewport = viewportOf(doc);
    const rootPolicy = elementPolicy(root, state);
    if (rootPolicy.omitted !== void 0) omitted.add(rootPolicy.omitted);
    let truncated = rootPolicy.kind === "skip" || rootPolicy.skipSubtree;
    let order = 0;
    const stack = [{ element: root, depth: 0, index: 0 }];
    while (!truncated && stack.length > 0) {
      if (state.visited % TIME_CHECK_INTERVAL === 0 && now() > deadline) {
        truncated = true;
        break;
      }
      const frame = stack[stack.length - 1];
      const children = frame.element.children;
      if (frame.index >= children.length) {
        stack.pop();
        continue;
      }
      const next = children[frame.index];
      frame.index += 1;
      if (!next) continue;
      if (state.visited >= MAX_VISITED_ELEMENTS) {
        truncated = true;
        omitted.add("node-limit");
        break;
      }
      state.visited += 1;
      const policy = elementPolicy(next, state);
      if (policy.kind === "skip") {
        if (policy.omitted !== void 0) omitted.add(policy.omitted);
        continue;
      }
      if (policy.omitted !== void 0) omitted.add(policy.omitted);
      const interactive = isInteractive(next);
      const role = deriveRole(next);
      const heading = isHeading(next);
      const text = policy.textAllowed ? clampText(sampleText(next, TEXT_SAMPLE_DEPTH, state)) : "";
      const tier = interactive ? 1 : heading || role !== void 0 ? 2 : text.length > 0 ? 3 : 0;
      if (tier > 0) {
        const node = {
          ref: "",
          // The protocol bounds depth to MAX_NODES; deeper nodes report the bound.
          depth: Math.min(frame.depth + 1, MAX_NODES),
          tag: safeTag(next),
          interactive,
          editable: policy.editable,
          offscreen: false
        };
        if (role !== void 0) node.role = role;
        if (interactive || heading || role !== void 0) {
          const name = deriveName(next, policy, state);
          if (name !== void 0) node.name = name;
        }
        if (text.length > 0) node.text = text;
        candidates.push({ node, element: next, tier, order: order++ });
      }
      if (policy.skipSubtree) continue;
      stack.push({ element: next, depth: frame.depth + 1, index: 0 });
    }
    if (state.exhausted) truncated = true;
    const ranked = candidates.slice().sort((left, right) => left.tier - right.tier || left.order - right.order);
    const selected = ranked.slice(0, MAX_NODES);
    if (ranked.length > selected.length) {
      truncated = true;
      omitted.add("node-limit");
    }
    selected.sort((left, right) => left.order - right.order);
    const title = clampText(stripForbiddenCharacters(doc.title ?? ""), MAX_TITLE_CHARS);
    const observation = {
      observationId: request.observationId,
      binding: request.binding,
      // The protocol stores capturedAt as a safe integer, so a fractional monotonic clock is rounded.
      capturedAt: Math.max(0, Math.round(now())),
      url,
      nodes: selected.map((candidate) => candidate.node),
      visited: state.visited,
      truncated,
      omitted: []
    };
    if (title.length > 0) observation.title = title;
    const draftRefs = createRefTable(request.observationId);
    for (const candidate of selected) {
      const geometry = measureGeometry(candidate.element, viewport);
      candidate.node.offscreen = geometry.offscreen;
      if (geometry.rect !== void 0) candidate.node.rect = geometry.rect;
      candidate.node.ref = draftRefs.put(candidate.element);
    }
    draftRefs.revoke();
    const finalise = () => {
      observation.truncated = truncated;
      observation.omitted = Array.from(omitted);
    };
    finalise();
    const budget = MAX_OBSERVATION_BYTES - OBSERVATION_HEADROOM_BYTES;
    while (observation.nodes.length > 0 && byteLength(JSON.stringify(observation)) > budget) {
      observation.nodes.pop();
      truncated = true;
      omitted.add("byte-limit");
      finalise();
    }
    finalise();
    if (observation.nodes.length === 0 && byteLength(JSON.stringify(observation)) > budget) {
      return { ok: false, code: "TOO_LARGE", message: "observation metadata exceeds the byte budget" };
    }
    const liveRefs = createRefTable(request.observationId);
    for (const candidate of selected.slice(0, observation.nodes.length)) liveRefs.put(candidate.element);
    const nodes = observation.nodes.map((node) => deepFreeze(node));
    const frozen = deepFreeze({
      ...observation,
      nodes: deepFreeze(nodes),
      omitted: deepFreeze(observation.omitted)
    });
    return { ok: true, observation: frozen, refs: liveRefs, bytes: byteLength(JSON.stringify(frozen)) };
  }

  // apps/navi/browser/content-entry.ts
  var scope = globalThis;
  if (!scope.naviStationAttached) {
    scope.naviStationAttached = true;
    const documentId = crypto.randomUUID();
    let observationId = "";
    let refs;
    let nodes = [];
    const currentTarget = (node, name) => {
      const fresh = collectObservation({ observationId: crypto.randomUUID(), binding: { grantId: documentId, tabId: 0, documentId, pageEpoch: 0 } });
      if (!fresh.ok) return false;
      const valid = fresh.observation.nodes.some((item) => fresh.refs.get(item.ref) === node && item.name === name && item.interactive && !item.editable && !item.offscreen);
      fresh.refs.revoke();
      return valid;
    };
    browser.runtime.onMessage.addListener((incoming) => {
      if (!incoming || typeof incoming !== "object") return void 0;
      const message = incoming;
      if (message.type === "observe") {
        const nextId = crypto.randomUUID();
        const result = collectObservation({ observationId: nextId, binding: { grantId: documentId, tabId: 0, documentId, pageEpoch: 0 } });
        if (!result.ok) {
          refs?.revoke();
          refs = void 0;
          return Promise.resolve({ error: result.message, elements: [] });
        }
        const next = result.observation.nodes.filter((node2) => node2.interactive && !node2.editable && !node2.offscreen && node2.name);
        const unchanged = refs && nodes.length === next.length && nodes.every((node2, index) => node2.name === next[index].name && node2.role === next[index].role && node2.tag === next[index].tag && refs?.get(node2.ref) === result.refs.get(next[index].ref));
        if (unchanged) result.refs.revoke();
        else {
          refs?.revoke();
          observationId = nextId;
          refs = result.refs;
          nodes = next;
        }
        return Promise.resolve({ document: `${documentId}:${observationId}`, title: result.observation.title, elements: nodes.map((node2) => ({ ref: node2.ref, name: node2.name, role: node2.role ?? node2.tag })), omitted: result.observation.omitted });
      }
      if (message.document !== `${documentId}:${observationId}` || typeof message.ref !== "string") return Promise.resolve({ ok: false, error: "Observation expired; observe the attached tab again" });
      const node = refs?.get(message.ref);
      const previous = nodes.find((item) => item.ref === message.ref);
      if (!node || !previous || typeof previous.name !== "string") return Promise.resolve({ ok: false, error: "Element is stale" });
      if (!currentTarget(node, previous.name)) return Promise.resolve({ ok: false, error: "Element changed; observe again" });
      try {
        if (message.type !== "click") return Promise.resolve({ ok: false, error: "Unregistered browser action" });
        if (focusElement(node) !== "applied") return Promise.resolve({ ok: false, error: "Browser control focus outcome unknown" });
        if (!currentTarget(node, previous.name)) return Promise.resolve({ ok: false, error: "Control changed during focus; click was not dispatched" });
        if (!(node instanceof HTMLElement)) return Promise.resolve({ ok: false, error: "Unsupported control" });
        node.click();
        return Promise.resolve({ ok: true, message: "Browser control clicked" });
      } catch (error) {
        return Promise.resolve({ ok: false, error: error instanceof Error ? error.message : "Browser action failed" });
      }
    });
  }
})();
