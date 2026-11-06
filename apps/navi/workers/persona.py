"""persona.py - load personas, expose defaults and aliases.

Personas are short system prompts plus TTS hints. Sources, in order of
precedence:

1. ~/.config/navi/personas.toml   (TOML or JSON; parsed best-effort)
2. apps/navi/config/personas.json (repo defaults)
3. hardcoded built-ins

Files are re-read per call when they change (mtime), so edits are picked up
without restarting Navi.
"""

import json
import os
import re
from dataclasses import dataclass

CONFIG_DIR = os.path.join(os.path.dirname(__file__), "..", "config")
DEFAULT_PERSONAS_PATH = os.path.join(CONFIG_DIR, "personas.json")
USER_PERSONAS_PATH = os.path.expanduser("~/.config/navi/personas.toml")
DEFAULT_PERSONA_NAME = "navi"
NAVI_PROMPT = (
    "You are Navi, the user's assistant on their own computer. Your name is "
    "Navi; that is your only name. Answer directly and concisely — one to "
    "three short sentences unless the user asks for detail. Plain prose: no "
    "markdown, no code blocks, no emoji, because your replies may be spoken "
    "aloud. You run inside the Navi desktop app and OpenCode provides your "
    "tools; use them when a request needs real work on the machine."
)

# What a reply may hold that nobody wants read out character by character.
_URL = re.compile(r"(?:https?://|www\.)[^\s)\]>\"']+")
# Absolute, home or relative paths, and "a/b/c.rs": the last part is what is
# said. Requiring a leading slash or an extension leaves "and/or" alone.
_PATH = re.compile(r"(?:(?<![\w/.~])(?:~|\.{1,2})?/(?:[\w.+-]+/)*|\b(?:[\w.+-]+/)+(?=[\w+-]+\.\w))([\w.+-]+)")
_EMOJI = re.compile("[\U0001F000-\U0001FAFF☀-➿️‍]")
_SENTENCE_END = re.compile(r"[.!?]+[\"')\]]*(?=\s|$)")
_SHORT_FORMS = {"e.g.", "i.e.", "etc.", "vs.", "mr.", "mrs.", "ms.", "dr.", "no.", "approx."}

BUILTIN = {
    "navi": {
        "system_prompt": NAVI_PROMPT,
        "voice": "en-GB-RyanNeural",
        "rate": "+0%",
    },
}

DEFAULT_MODEL = "opencode/deepseek-v4.1-flash-free"
FALLBACK_MODEL = "commandcode/deepseek/deepseek-v4.1-flash"


@dataclass
class Persona:
    name: str
    system_prompt: str
    voice: str = "en-US-ChristopherNeural"
    rate: str = "+0%"


_cache = {"mtime": 0.0, "data": {}}


def _strip_toml_markup(text):
    """Turn TOML into parseable JSON by rewriting bare-key lines."""
    # personas.toml is authored as JSON-compatible: if the user writes real
    # TOML we accept a subset: [name] tables, key = "value" lines.
    return text


def _load_toml_subset(text):
    """Minimal TOML subset parser: [personas.<name>] or [name] tables with
    key = "value" pairs. Returns dict like {name: {key: value}}."""
    try:
        import tomllib  # py3.11+
        return tomllib.loads(text)
    except Exception:
        pass
    data = {}
    section = None
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        m = re.match(r"^\[(.+)\]$", line)
        if m:
            section = m.group(1).strip().strip('"')
            parts = section.split(".")
            node = data
            for p in parts:
                node = node.setdefault(p, {})
            continue
        m = re.match(r"^([A-Za-z_][\w-]*)\s*=\s*(.+)$", line)
        if m and section is not None:
            key, val = m.group(1), m.group(2).strip().rstrip(",")
            try:
                val = json.loads(val)
            except Exception:
                val = val.strip('"').strip("'")
            node = data
            for p in section.split("."):
                node = node.setdefault(p, {})
            node[key] = val
    return data


def _load_file(path):
    try:
        with open(path, "rb") as f:
            raw = f.read()
    except OSError:
        return {}
    text = raw.decode("utf-8", errors="replace")
    if path.endswith(".json"):
        try:
            return json.loads(text)
        except Exception:
            return {}
    return _load_toml_subset(text)


def _merge(base, override):
    out = dict(base)
    for name, spec in (override or {}).items():
        if isinstance(spec, dict):
            merged = dict(out.get(name, {}))
            merged.update(spec)
            out[name] = merged
    return out


def personas():
    """Merged persona table: builtin <- defaults file <- user file."""
    files = [DEFAULT_PERSONAS_PATH, USER_PERSONAS_PATH]
    mt = 0.0
    for p in files:
        try:
            mt = max(mt, os.path.getmtime(p))
        except OSError:
            continue
    if mt != _cache["mtime"]:
        data = dict(BUILTIN)
        for p in files:
            data = _merge(data, _load_file(p))
        _cache["mtime"] = mt
        _cache["data"] = data
    return _cache["data"]


def names():
    return sorted(personas().keys())


def aliases():
    """Voice-command aliases -> persona name. Navi is the only identity."""
    out = {}
    for name in personas():
        out[name] = name
    return out


def get(name):
    table = personas()
    resolved = name if name in table else DEFAULT_PERSONA_NAME
    spec = table.get(resolved) or {}
    return Persona(
        name=resolved,
        system_prompt=spec.get("system_prompt") or spec.get("prompt") or "",
        voice=spec.get("voice") or spec.get("tts_voice") or "en-GB-RyanNeural",
        rate=spec.get("rate") or spec.get("tts_rate") or "+0%",
    )


def resolve(target):
    """Map a voice-command alias ("brief", "iron man") to a persona name.

    Returns None when the target is not a known persona or alias.
    """
    if not target:
        return None
    key = target.strip().lower()
    table = personas()
    if key in table:
        return key
    return aliases().get(key)


def sanitize_for_tts(text):
    """Strip markdown / code so spoken output is clean prose."""
    if not text:
        return text
    # Navi's own bookkeeping lines are for the station, never for the ear.
    text = re.sub(r"^\s*(?:PROGRESS|TASK)\s.*$", "", text, flags=re.M)
    text = re.sub(r"```.*?```", " ", text, flags=re.S)   # fenced code
    text = re.sub(r"```.*", " ", text, flags=re.S)      # unterminated fence
    text = re.sub(r"`([^`]*)`", r"\1", text)            # inline code
    text = re.sub(r"^\s{0,3}#{1,6}\s*", "", text, flags=re.M)  # headings
    text = re.sub(r"\*\*?([^*]+)\*\*?", r"\1", text)    # bold/italic
    text = re.sub(r"__([^_]+)__", r"\1", text)
    text = re.sub(r"~~([^~]+)~~", r"\1", text)          # strikethrough
    text = re.sub(r"^\s*[-*+]\s+", "", text, flags=re.M)       # bullets
    text = re.sub(r"^\s*\d+\.\s+", "", text, flags=re.M)       # numbered
    text = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", text)       # links
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", " ", text)          # images
    text = re.sub(r"^\s*>\s?", "", text, flags=re.M)           # quotes
    text = re.sub(r"^\s*[-*_]{3,}\s*$", "", text, flags=re.M)  # rules
    text = re.sub(r"\|", ", ", text)                    # table pipes
    text = _URL.sub("a link", text)
    text = _PATH.sub(lambda match: match.group(1), text)
    text = _EMOJI.sub("", text)
    text = re.sub(r"[ \t]+", " ", text)
    text = re.sub(
        r"\n\s*\n+",
        lambda match: (
            " " if match.string[: match.start()].rstrip()[-1:] in ".!?:" else ". "
        ),
        text,
    )
    text = re.sub(r"\s*\n\s*", " ", text)               # soft line breaks
    return text.strip()


def split_sentences(text):
    """Split where a sentence ends: punctuation, then space or the end.

    "Version 1.2.3 is out, e.g. on npm." is one sentence; a dot inside a number
    or a file name, or after a short form like "e.g.", ends nothing.
    """
    sentences = []
    start = 0
    for match in _SENTENCE_END.finditer(text):
        word = text[start:match.start() + 1].rsplit(None, 1)[-1].lower()
        if word in _SHORT_FORMS:
            continue
        sentences.append(text[start:match.end()].strip())
        start = match.end()
    rest = text[start:].strip()
    if rest:
        sentences.append(rest)
    return sentences


def first_sentences(text, count=3, maximum=600):
    """Return clean prose capped to *count* sentences for speech.

    The UI still shows the full reply; only playback is shortened so a long
    answer does not hold the voice loop for minutes.
    """
    text = sanitize_for_tts(text)
    if not text:
        return text or ""
    spoken = " ".join(split_sentences(text)[:count]).strip()
    if len(spoken) > maximum:
        spoken = spoken[:maximum].rsplit(" ", 1)[0].rstrip(" ,;:") + "."
    return spoken


if __name__ == "__main__":
    import sys
    args = sys.argv[1:]
    if args and args[0] == "--names":
        print(" ".join(names()))
        sys.exit(0)
    if args and args[0] == "--resolve":
        print(resolve(args[1] if len(args) > 1 else "") or "")
        sys.exit(0)
    name = args[0] if args else DEFAULT_PERSONA_NAME
    p = get(name)
    print(f"{p.name}: voice={p.voice} rate={p.rate}")
    print(p.system_prompt)
    print("available:", ", ".join(names()))
