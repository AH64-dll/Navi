"""opencode_contract.py - the part of OpenCode's server API that Navi depends on.

OpenCode 2.x publishes its whole HTTP API as OpenAPI at GET /openapi.json. Navi
uses a small slice of it. This module names that slice, pins a copy of it, and
checks request bodies against it, so an OpenCode update that changes a shape
Navi relies on fails a test or a `check` run instead of being silently ignored.

Why this exists: the server drops body fields it does not know without an
error. Navi sent `directory` to session.create and `parts` to session.prompt
for months; both were discarded with a 200, so every session ran in $HOME and
every attachment was lost.

  python opencode_contract.py check      live spec vs. what Navi uses; exit 1 on drift
  python opencode_contract.py snapshot   rewrite config/opencode-contract.json from the live spec

After an OpenCode update: run `check`. On drift, fix the gateway, then run
`snapshot` and the gateway tests.
"""
import json
import re
import sys
import urllib.request
from pathlib import Path

from opencode_gateway import Gateway

SNAPSHOT = Path(__file__).resolve().parent.parent / "config" / "opencode-contract.json"

# operationId -> (method, path). Every HTTP call the gateway makes is listed here.
OPERATIONS = {
    "server.info": ("GET", "/api/info"),
    "agent.list": ("GET", "/api/agent"),
    "agent.get": ("GET", "/api/agent/{agentID}"),
    "model.list": ("GET", "/api/model"),
    "model.default": ("GET", "/api/model/default"),
    "command.list": ("GET", "/api/command"),
    "skill.list": ("GET", "/api/skill"),
    "provider.list": ("GET", "/api/provider"),
    "credential.list": ("GET", "/api/credential"),
    "credential.activate": ("POST", "/api/credential/{credentialID}/activate"),
    "integration.list": ("GET", "/api/integration"),
    "integration.oauth.connect": ("POST", "/api/integration/{integrationID}/connect/oauth"),
    "integration.oauth.status": ("GET", "/api/integration/{integrationID}/connect/oauth/{attemptID}"),
    "integration.oauth.complete": ("POST", "/api/integration/{integrationID}/connect/oauth/{attemptID}/complete"),
    "integration.connect.key": ("POST", "/api/integration/{integrationID}/connect/key"),
    "mcp.list": ("GET", "/api/mcp"),
    "experimental.mcp.add": ("PUT", "/api/experimental/mcp/{server}"),
    "experimental.mcp.remove": ("DELETE", "/api/experimental/mcp/{server}"),
    "experimental.mcp.connect": ("POST", "/api/experimental/mcp/{server}/connect"),
    "experimental.mcp.disconnect": ("POST", "/api/experimental/mcp/{server}/disconnect"),
    "permission.request.list": ("GET", "/api/permission/request"),
    "permission.saved.list": ("GET", "/api/permission/saved"),
    "permission.saved.update": ("PATCH", "/api/permission/saved/{id}"),
    "permission.saved.remove": ("DELETE", "/api/permission/saved/{id}"),
    "config.get": ("GET", "/api/config"),
    "experimental.config.update": ("PATCH", "/api/experimental/config"),
    "plugin.list": ("GET", "/api/plugin"),
    "plugin.update": ("POST", "/api/plugin/update"),
    "experimental.usage.limits": ("GET", "/api/experimental/usage/limits"),
    "experimental.session.stats": ("GET", "/api/experimental/session/stats"),
    "event.subscribe": ("GET", "/api/event"),
    "session.create": ("POST", "/api/session"),
    "session.list": ("GET", "/api/session"),
    "session.active": ("GET", "/api/session/active"),
    "session.get": ("GET", "/api/session/{sessionID}"),
    "session.update": ("PATCH", "/api/session/{sessionID}"),
    "session.fork": ("POST", "/api/session/{sessionID}/fork"),
    "session.remove": ("DELETE", "/api/session/{sessionID}"),
    "session.compact": ("POST", "/api/session/{sessionID}/compact"),
    "session.message.list": ("GET", "/api/session/{sessionID}/message"),
    "session.prompt": ("POST", "/api/session/{sessionID}/prompt"),
    "session.command": ("POST", "/api/session/{sessionID}/command"),
    "session.interrupt": ("POST", "/api/session/{sessionID}/interrupt"),
    "session.switchModel": ("POST", "/api/session/{sessionID}/model"),
    "session.switchAgent": ("POST", "/api/session/{sessionID}/agent"),
    "session.permission.reply": ("POST", "/api/session/{sessionID}/permission/{requestID}/reply"),
    "session.form.reply": ("POST", "/api/session/{sessionID}/form/{formID}/reply"),
    "session.form.cancel": ("DELETE", "/api/session/{sessionID}/form/{formID}"),
}

# Event names the gateway acts on. The spec lists event names but not their payloads.
EVENTS = {
    "session.step.started", "session.step.streamed", "session.step.ended", "session.usage.updated",
    "session.execution.started", "session.execution.succeeded", "session.execution.failed",
    "session.execution.interrupted", "session.text.started", "session.text.delta", "session.text.ended",
    "session.reasoning.started", "session.reasoning.delta", "session.reasoning.ended",
    "session.tool.input.started", "session.tool.input.ended", "session.tool.called",
    "session.tool.progress", "session.tool.success", "session.tool.failed",
    "shell.created", "shell.exited", "permission.asked", "permission.replied",
    "form.created", "form.replied", "form.cancelled",
}


def _operations(spec):
    """operationId -> (method, path, request body schema or None) for the whole spec."""
    found = {}
    for path, item in (spec.get("paths") or {}).items():
        for method, operation in item.items():
            if not isinstance(operation, dict) or "operationId" not in operation:
                continue
            body = (((operation.get("requestBody") or {}).get("content") or {})
                    .get("application/json") or {}).get("schema")
            found[operation["operationId"]] = (method.upper(), path, body)
    return found


def _event_names(spec):
    """Every dotted name the spec mentions; event names are listed there as string enums."""
    return set(re.findall(r'"([a-z][a-z-]*(?:\.[a-z][a-z-]*)+)"', json.dumps(spec)))


def problems(spec):
    """What Navi uses that this spec no longer provides, as readable lines."""
    found = _operations(spec)
    out = []
    for name, (method, path) in OPERATIONS.items():
        if name not in found:
            out.append(f"operation {name} ({method} {path}) is gone")
        elif found[name][:2] != (method, path):
            out.append(f"operation {name} moved: {method} {path} -> {found[name][0]} {found[name][1]}")
    names = _event_names(spec)
    for event in sorted(EVENTS - names):
        out.append(f"event {event} is no longer published")
    return out


def _referenced(node, schemas, into):
    if isinstance(node, dict):
        ref = node.get("$ref")
        if isinstance(ref, str):
            name = ref.rsplit("/", 1)[-1]
            if name not in into and name in schemas:
                into[name] = schemas[name]
                _referenced(schemas[name], schemas, into)
        for value in node.values():
            _referenced(value, schemas, into)
    elif isinstance(node, list):
        for value in node:
            _referenced(value, schemas, into)


def extract(spec, version=None):
    """The pinned slice: Navi's operations with their body schemas, and the events."""
    found = _operations(spec)
    schemas = (spec.get("components") or {}).get("schemas") or {}
    operations, referenced = {}, {}
    for name in sorted(OPERATIONS):
        if name not in found:
            continue
        method, path, body = found[name]
        operations[name] = {"method": method, "path": path, "body": body}
        _referenced(body, schemas, referenced)
    return {"opencode": version, "operations": operations,
            "schemas": dict(sorted(referenced.items())), "events": sorted(EVENTS & _event_names(spec))}


def load(path=SNAPSHOT):
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


_TYPES = {"object": dict, "array": list, "string": str, "boolean": bool, "null": type(None)}


def _errors(value, schema, schemas, where):
    """JSON Schema errors for the subset OpenCode's spec uses. Unknown keywords pass."""
    if not isinstance(schema, dict):
        return []
    if "$ref" in schema:
        return _errors(value, schemas.get(schema["$ref"].rsplit("/", 1)[-1], {}), schemas, where)
    for key in ("anyOf", "oneOf"):
        if key in schema:
            options = [_errors(value, option, schemas, where) for option in schema[key]]
            if all(options):
                return [f"{where}: matches none of {len(options)} shapes ({options[0][0]})"]
    if "const" in schema and value != schema["const"]:
        return [f"{where}: must be {schema['const']!r}"]
    if "enum" in schema and value not in schema["enum"]:
        return [f"{where}: {value!r} is not one of {schema['enum']}"]
    kind = schema.get("type")
    if kind in ("number", "integer"):
        if isinstance(value, bool) or not isinstance(value, (int, float)) \
                or (kind == "integer" and not float(value).is_integer()):
            return [f"{where}: must be {kind}"]
    elif kind in _TYPES and not isinstance(value, _TYPES[kind]):
        return [f"{where}: must be {kind}"]
    if isinstance(value, str) and "pattern" in schema and not re.search(schema["pattern"], value):
        return [f"{where}: {value!r} does not match {schema['pattern']}"]
    out = []
    if isinstance(value, dict):
        properties = schema.get("properties") or {}
        for name in schema.get("required") or []:
            if name not in value:
                out.append(f"{where}: missing {name}")
        extra = schema.get("additionalProperties", True)
        for name, item in value.items():
            if name in properties:
                out += _errors(item, properties[name], schemas, f"{where}.{name}")
            elif extra is False:
                out.append(f"{where}: unknown field {name!r} (OpenCode drops it without an error)")
            elif isinstance(extra, dict):
                out += _errors(item, extra, schemas, f"{where}.{name}")
    if isinstance(value, list) and isinstance(schema.get("items"), dict):
        for index, item in enumerate(value):
            out += _errors(item, schema["items"], schemas, f"{where}[{index}]")
    return out


def validate(contract, operation, body):
    """Errors for `body` sent to `operation`, against a pinned or live contract."""
    entry = contract["operations"].get(operation)
    if entry is None:
        return [f"{operation} is not in the contract"]
    if body is None:
        return []  # nothing sent, so nothing for the server to drop
    if entry["body"] is None:
        return [f"{operation} takes no body"]
    return _errors(body, entry["body"], contract["schemas"], operation)


def fetch_live():
    """(spec, version) from the server the gateway would use."""
    gateway = Gateway()
    request = urllib.request.Request(gateway.base_url + "/openapi.json",
                                     headers={"Authorization": f"Basic {gateway._token}"})
    with urllib.request.urlopen(request, timeout=10) as response:
        spec = json.loads(response.read().decode())
    return spec, (gateway._get("/api/info", timeout=5.0) or {}).get("version")


def main(argv):
    if not argv or argv[0] not in ("check", "snapshot"):
        print(__doc__.strip().split("\n\n")[-2], file=sys.stderr)
        return 2
    spec, version = fetch_live()
    if argv[0] == "snapshot":
        SNAPSHOT.write_text(json.dumps(extract(spec, version), indent=1, sort_keys=True) + "\n")
        print(json.dumps({"written": str(SNAPSHOT), "opencode": version}))
        return 0
    found = problems(spec)
    pinned = load().get("opencode")
    print(json.dumps({"opencode": version, "pinned": pinned, "problems": found}, indent=1))
    return 1 if found else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
