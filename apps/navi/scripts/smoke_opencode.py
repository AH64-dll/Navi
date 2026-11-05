"""Live smoke against the running OpenCode service: models, one prompt, /help.

Never starts, stops or restarts the shared OpenCode service.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
import opencode_gateway as gateway_module


def main():
    gateway = gateway_module.Gateway()
    models = gateway.models()
    print(f"models: {len(models)} (first: {models[0]['providerID']}/{models[0]['id']})")
    print("agents:", ", ".join(item["id"] for item in gateway.agents() if item["id"] != "navi") or "-")
    commands = gateway.commands()
    print(f"commands: {len(commands)} (first: /{commands[0]['name']})")
    records = []
    reply = gateway.ask("Say exactly: smoke ok", emit=records.append)
    deltas = sum(1 for record in records if record.get("type") == "delta")
    tools = sum(1 for record in records if record.get("type") == "tool")
    usage = (gateway.last_result or {}).get("usage") or {}
    print(f"prompt reply: {reply!r} ({deltas} deltas, {tools} tools, "
          f"tokens={usage.get('tokens')}, cost={usage.get('cost')})")
    assert "smoke ok" in reply.lower(), reply
    print("PASS")

if __name__ == "__main__":
    try:
        main()
    except Exception as error:  # noqa: BLE001 - smoke output
        print(f"FAIL: {type(error).__name__}: {error}")
        sys.exit(1)
