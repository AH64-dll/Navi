import { focusElement } from "../../../packages/navi/browser/content/actions.ts";
import { collectObservation } from "../../../packages/navi/browser/content/collector.ts";
import type { RefTable } from "../../../packages/navi/browser/content/refs.ts";
import type { DomNode } from "../../../packages/navi/src/protocol/types.ts";

declare const browser: { runtime: { onMessage: { addListener(listener: (message: unknown) => Promise<unknown> | undefined): void } } };
const scope = globalThis as typeof globalThis & { naviStationAttached?: boolean };
if (!scope.naviStationAttached) {
    scope.naviStationAttached = true;
    const documentId = crypto.randomUUID();
    let observationId = "";
    let refs: RefTable | undefined;
    let nodes: DomNode[] = [];
    const currentTarget = (node: Element, name: string) => {
        const fresh = collectObservation({ observationId: crypto.randomUUID(), binding: { grantId: documentId, tabId: 0, documentId, pageEpoch: 0 } });
        if (!fresh.ok) return false;
        const valid = fresh.observation.nodes.some(item => fresh.refs.get(item.ref) === node && item.name === name && item.interactive && !item.editable && !item.offscreen);
        fresh.refs.revoke();
        return valid;
    };
    browser.runtime.onMessage.addListener((incoming) => {
        if (!incoming || typeof incoming !== "object") return undefined;
        const message = incoming as Record<string, unknown>;
        if (message.type === "observe") {
            const nextId = crypto.randomUUID();
            const result = collectObservation({ observationId: nextId, binding: { grantId: documentId, tabId: 0, documentId, pageEpoch: 0 } });
            if (!result.ok) { refs?.revoke(); refs = undefined; return Promise.resolve({ error: result.message, elements: [] }); }
            const next = result.observation.nodes.filter(node => node.interactive && !node.editable && !node.offscreen && node.name);
            const unchanged = refs && nodes.length === next.length && nodes.every((node, index) => node.name === next[index].name && node.role === next[index].role && node.tag === next[index].tag && refs?.get(node.ref) === result.refs.get(next[index].ref));
            // Polls of an unchanged page retain the same observation. Otherwise
            // CPU inference longer than the poll interval would always expire.
            if (unchanged) result.refs.revoke();
            else { refs?.revoke(); observationId = nextId; refs = result.refs; nodes = next; }
            return Promise.resolve({ document: `${documentId}:${observationId}`, title: result.observation.title, elements: nodes.map(node => ({ ref: node.ref, name: node.name, role: node.role ?? node.tag })), omitted: result.observation.omitted });
        }
        if (message.document !== `${documentId}:${observationId}` || typeof message.ref !== "string") return Promise.resolve({ ok: false, error: "Observation expired; observe the attached tab again" });
        const node = refs?.get(message.ref);
        const previous = nodes.find(item => item.ref === message.ref);
        if (!node || !previous || typeof previous.name !== "string") return Promise.resolve({ ok: false, error: "Element is stale" });
        if (!currentTarget(node, previous.name)) return Promise.resolve({ ok: false, error: "Element changed; observe again" });
        try {
            if (message.type !== "click") return Promise.resolve({ ok: false, error: "Unregistered browser action" });
            // Foundation focus validation rechecks connectedness, visibility,
            // disabled/secret/editable ancestry and the foreground document.
            if (focusElement(node) !== "applied") return Promise.resolve({ ok: false, error: "Browser control focus outcome unknown" });
            if (!currentTarget(node, previous.name)) return Promise.resolve({ ok: false, error: "Control changed during focus; click was not dispatched" });
            if (!(node instanceof HTMLElement)) return Promise.resolve({ ok: false, error: "Unsupported control" });
            node.click();
            return Promise.resolve({ ok: true, message: "Browser control clicked" });
        } catch (error) { return Promise.resolve({ ok: false, error: error instanceof Error ? error.message : "Browser action failed" }); }
    });
}
