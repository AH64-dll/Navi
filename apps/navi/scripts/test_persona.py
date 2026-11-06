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
        p = persona.get("navi")
        self.assertEqual(p.name, "navi")
        self.assertIn("Navi", p.system_prompt)
        self.assertNotIn("Jarvis", p.system_prompt)
        self.assertIsNone(persona.resolve("jarvis"))

    def test_unknown_persona_falls_back_to_default(self):
        p = persona.get("nonexistent-persona")
        self.assertEqual(p.name, persona.DEFAULT_PERSONA_NAME)

    def test_names_sorted_and_include_builtins(self):
        names = persona.names()
        self.assertEqual(names, sorted(names))
        self.assertEqual(names, ["navi"])

    def test_aliases_resolve(self):
        aliases = persona.aliases()
        self.assertEqual(aliases["navi"], "navi")
        self.assertNotIn("jarvis", aliases)

    def test_user_file_overrides_builtin(self):
        with tempfile.TemporaryDirectory() as folder:
            override = Path(folder) / "personas.toml"
            override.write_text(
                '[navi]\n'
                'system_prompt = "custom navi"\n'
                'voice = "en-test"\n'
                'rate = "+99%"\n'
            )
            old = persona.USER_PERSONAS_PATH
            old_cache = dict(persona._cache)
            try:
                persona.USER_PERSONAS_PATH = str(override)
                persona._cache["mtime"] = 0.0  # force reload
                p = persona.get("navi")
                self.assertEqual(p.system_prompt, "custom navi")
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
                "navi": {"system_prompt": "json override"},
            }))
            old = persona.USER_PERSONAS_PATH
            old_cache = dict(persona._cache)
            try:
                persona.USER_PERSONAS_PATH = str(override)
                persona._cache["mtime"] = 0.0
                self.assertEqual(persona.get("navi").system_prompt, "json override")
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

    def test_a_dot_inside_a_number_or_short_form_ends_nothing(self):
        self.assertEqual(persona.first_sentences("Version 1.2.3 is out, e.g. on npm. It works."),
                         "Version 1.2.3 is out, e.g. on npm. It works.")
        self.assertEqual(persona.split_sentences("Version 1.2.3 is out, e.g. on npm. It works."),
                         ["Version 1.2.3 is out, e.g. on npm.", "It works."])

    def test_links_are_not_read_out(self):
        self.assertEqual(persona.first_sentences("Done. See https://example.com/a/b?x=1 for details."),
                         "Done. See a link for details.")

    def test_paths_say_only_the_file_name(self):
        self.assertEqual(
            persona.first_sentences("Edited `apps/navi/src/main.rs` and /home/amr/Documents/navi/README.md today."),
            "Edited main.rs and README.md today.")

    def test_a_slash_in_words_is_left_alone(self):
        self.assertEqual(persona.sanitize_for_tts("Use and/or TCP/IP on 1/2."), "Use and/or TCP/IP on 1/2.")

    def test_emoji_are_not_spoken(self):
        self.assertEqual(persona.first_sentences("Great news! \U0001F600 All tests pass."), "Great news! All tests pass.")

    def test_progress_lines_are_not_spoken(self):
        text = "Fixed the bug.\n\nPROGRESS part \"Voice\" 80 | tests pass\nPROGRESS overall 70 | read the code"
        self.assertEqual(persona.first_sentences(text), "Fixed the bug.")


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
                "One. Two! Three? Four. Five.", "navi"
            )
        finally:
            speech_out._tts, speech_out._tts_checked = old_module, old_checked
        self.assertEqual(payload["text"], "One. Two! Three? Four. Five.")
        self.assertEqual(payload["spoken_text"], "One. Two! Three?")
        self.assertTrue(payload["spoken"])
        self.assertEqual(calls["text"], "One. Two! Three?")
        self.assertEqual(calls["voice"], persona.get("navi").voice)
        self.assertEqual(calls["rate"], persona.get("navi").rate)


class ResolveTests(unittest.TestCase):
    def test_names_and_aliases_resolve(self):
        self.assertEqual(persona.resolve("navi"), "navi")
        self.assertEqual(persona.resolve("Navi"), "navi")
        self.assertIsNone(persona.resolve("concise"))
        self.assertIsNone(persona.resolve("jarvis"))
        self.assertIsNone(persona.resolve("nonsense"))
        self.assertIsNone(persona.resolve(""))


if __name__ == "__main__":
    unittest.main()
