"""Live smoke test: one prompt through the running `opencode serve`.

Uses the user service on NAVI_OPENCODE_URL (default http://127.0.0.1:49374)
and its password from ~/.config/opencode/service.json (or the
NAVI_OPENCODE_PASSWORD / OPENCODE_SERVER_PASSWORD env vars). Never spawns or
stops a server.

    python3 smoke_opencode.py [prompt]
"""
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
import opencode_bridge as bridge
import persona


def main():
    client = bridge.OpenCodeBridge(autospawn=False)
    if not client.reachable():
        print(f"FAIL: opencode serve unreachable at {client.base_url}")
        print("hint: start `opencode serve` or check NAVI_OPENCODE_PASSWORD")
        return 1
    p = persona.get("jarvis")
    prompt = " ".join(sys.argv[1:]) or "Say exactly the word: smoke-ok"
    print(f"server:  {client.base_url}")
    print(f"model:   {client.model_ref}")
    print(f"persona: {p.name}")
    started = time.time()
    try:
        reply = client.prompt(prompt, system_prompt=p.system_prompt or None)
    except bridge.BridgeError as error:
        print(f"FAIL: {error}")
        return 1
    elapsed = time.time() - started
    print(f"reply ({elapsed:.1f}s): {reply}")
    print(f"session: {client.session_id}")
    print("PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
