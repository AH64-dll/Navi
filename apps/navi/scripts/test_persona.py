"""Persona loading, merging, aliasing, and TTS sanitization checks."""
import json
import os
import tempfile
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "workers"))
import persona


class PersonaTests(unittest.TestCase):
    def test_builtins_resolve(self):
        p = persona.get("jarvis")
        self.assertEqual(p.name, "jarvis")
        self.assertIn("Jarvis", p.system_prompt)
        self.assertEqual(persona.get("concise").name, "concise")
        self.assertEqual(persona.get("friendly").name, "friendly")

    def test_unknown_persona_falls_back_to_default(self):
        p = persona.get("nonexistent-persona")
        self.assertEqual(p.name, persona.DEFAULT_PERSONA_NAME)

    def test_names_sorted_and_include_builtins(self):
        names = persona.names()
        self.assertEqual(names, sorted(names))
        for name in ("jarvis", "friendly", "concise"):
            self.assertIn(name, names)

    def test_aliases_resolve(self):
        aliases = persona.aliases()
        self.assertEqual(aliases["jarvis"], "jarvis")
        self.assertEqual(aliases["concise"], "concise")
        self.assertEqual(aliases["brief"], "concise")
        self.assertEqual(aliases["casual"], "friendly")

    def test_user_file_overrides_builtin(self):
        with tempfile.TemporaryDirectory() as folder:
            override = Path(folder) / "personas.toml"
            override.write_text(
                '[jarvis]\n'
                'system_prompt = "custom jarvis"\n'
                'voice = "en-test"\n'
                'rate = "+99%"\n'
            )
            old = persona.USER_PERSONAS_PATH
            old_cache = dict(persona._cache)
            try:
                persona.USER_PERSONAS_PATH = str(override)
                persona._cache["mtime"] = 0.0  # force reload
                p = persona.get("jarvis")
                self.assertEqual(p.system_prompt, "custom jarvis")
                self.assertEqual(p.voice, "en-test")
                self.assertEqual(p.rate, "+99%")
            finally:
                persona.USER_PERSONAS_PATH = old
                persona._cache.update(old_cache)

    def test_user_file_adds_new_persona(self):
        with tempfile.TemporaryDirectory() as folder:
            override = Path(folder) / "personas.toml"
            override.write_text(
                '[robot]\n'
                'system_prompt = "beep boop"\n'
            )
            old = persona.USER_PERSONAS_PATH
            old_cache = dict(persona._cache)
            try:
                persona.USER_PERSONAS_PATH = str(override)
                persona._cache["mtime"] = 0.0
                self.assertIn("robot", persona.names())
                p = persona.get("robot")
                self.assertEqual(p.system_prompt, "beep boop")
            finally:
                persona.USER_PERSONAS_PATH = old
                persona._cache.update(old_cache)

    def test_json_user_file_also_accepted(self):
        with tempfile.TemporaryDirectory() as folder:
            override = Path(folder) / "personas.json"
            override.write_text(json.dumps({
                "jarvis": {"system_prompt": "json override"},
            }))
            old = persona.USER_PERSONAS_PATH
            old_cache = dict(persona._cache)
            try:
                persona.USER_PERSONAS_PATH = str(override)
                persona._cache["mtime"] = 0.0
                self.assertEqual(persona.get("jarvis").system_prompt, "json override")
            finally:
                persona.USER_PERSONAS_PATH = old
                persona._cache.update(old_cache)


class SanitizeTests(unittest.TestCase):
    def test_fenced_code_removed(self):
        text = "Here:\n```python\nprint(1)\n```\ndone."
        self.assertEqual(persona.sanitize_for_tts(text), "Here: done.")

    def test_inline_code_kept_as_prose(self):
        self.assertEqual(
            persona.sanitize_for_tts("Run `cargo test` now."),
            "Run cargo test now.",
        )

    def test_bold_and_headings_stripped(self):
        text = "## Result\n\n**bold** answer"
        self.assertEqual(persona.sanitize_for_tts(text), "Result. bold answer")

    def test_markdown_link_collapses_to_label(self):
        self.assertEqual(
            persona.sanitize_for_tts("see [the docs](https://x.test) here"),
            "see the docs here",
        )

    def test_bullets_and_numbering_stripped(self):
        text = "- one\n- two\n1. first\n2. second"
        out = persona.sanitize_for_tts(text)
        self.assertNotIn("-", out)
        self.assertNotIn("1.", out)

    def test_empty_input(self):
        self.assertEqual(persona.sanitize_for_tts(""), "")
        self.assertIsNone(persona.sanitize_for_tts(None))

    def test_paragraphs_stay_sentences(self):
        self.assertEqual(
            persona.sanitize_for_tts("## Result\n\n**bold** answer"),
            "Result. bold answer",
        )


class SentenceLimitTests(unittest.TestCase):
    def test_caps_to_three_sentences(self):
        text = "One. Two! Three? Four. Five."
        self.assertEqual(persona.first_sentences(text), "One. Two! Three?")

    def test_short_reply_untouched(self):
        self.assertEqual(persona.first_sentences("All systems nominal."), "All systems nominal.")

    def test_long_single_sentence_is_cut_on_a_word(self):
        out = persona.first_sentences("word " * 300)
        self.assertLessEqual(len(out), 601)
        self.assertTrue(out.endswith("."))
        self.assertNotIn("  ", out)

    def test_code_is_not_spoken(self):
        out = persona.first_sentences("Run this:\n```sh\nrm -rf /\n```\nthen stop.")
        self.assertNotIn("rm -rf", out)
        self.assertIn("then stop.", out)


class SpeechOutTests(unittest.TestCase):
    def test_full_reply_kept_but_only_three_sentences_spoken(self):
        import speech_out

        calls = {}

        class FakeTTS:
            @staticmethod
            def speak(text, voice=None, rate=None, cancel_event=None):
                calls.update(text=text, voice=voice, rate=rate)
                return True

        old_module, old_checked = speech_out._tts, speech_out._tts_checked
        try:
            speech_out._tts, speech_out._tts_checked = FakeTTS, True
            payload = speech_out.speak(
                "One. Two! Three? Four. Five.", "concise"
            )
        finally:
            speech_out._tts, speech_out._tts_checked = old_module, old_checked
        self.assertEqual(payload["text"], "One. Two! Three? Four. Five.")
        self.assertEqual(payload["spoken_text"], "One. Two! Three?")
        self.assertTrue(payload["spoken"])
        self.assertEqual(calls["text"], "One. Two! Three?")
        self.assertEqual(calls["voice"], persona.get("concise").voice)
        self.assertEqual(calls["rate"], persona.get("concise").rate)


class ResolveTests(unittest.TestCase):
    def test_names_and_aliases_resolve(self):
        self.assertEqual(persona.resolve("jarvis"), "jarvis")
        self.assertEqual(persona.resolve("Concise"), "concise")
        self.assertEqual(persona.resolve("brief"), "concise")
        self.assertEqual(persona.resolve("iron man"), "jarvis")
        self.assertEqual(persona.resolve("casual"), "friendly")
        self.assertIsNone(persona.resolve("nonsense"))
        self.assertIsNone(persona.resolve(""))


if __name__ == "__main__":
    unittest.main()
