"""Pure bounded edit proposals. No filesystem access, writes or model route.

The eventual owner bridge must supply an already-approved full-file snapshot.
This module cannot acquire one or apply a proposal. Digest comparison is a
staleness signal, not an atomic filesystem compare-and-swap guarantee.
"""
from dataclasses import dataclass
import difflib
import hashlib
from pathlib import Path
import re
from project_read import ProjectReadError, TEXT_EXTENSIONS, _parts

MAX_EDIT_BYTES = 16384
MAX_DIFF_BYTES = 65536
MAX_EDIT_LINES = 1000


def _text(value):
    if type(value) is not str:
        raise ProjectReadError('Edit content must be plain text')
    try:
        raw = value.encode('utf-8')
    except UnicodeError as error:
        raise ProjectReadError('Edit content must be UTF-8') from error
    if len(raw) > MAX_EDIT_BYTES or len(value.splitlines()) > MAX_EDIT_LINES:
        raise ProjectReadError('Edit content exceeded preview bounds')
    if any((ord(c) < 32 and c not in '\t\r\n') or ord(c) == 127 for c in value):
        raise ProjectReadError('Edit content contains binary controls')
    # Bidi controls can make a reviewed diff appear to change different code.
    if any(c in value for c in '\u202a\u202b\u202c\u202d\u202e\u2066\u2067\u2068\u2069'):
        raise ProjectReadError('Edit content contains directional controls')
    return raw


@dataclass(frozen=True)
class EditProposal:
    """Immutable exact content binding, not permission to write."""
    path: str
    before_sha256: str
    after_sha256: str
    before: str
    after: str
    diff: str

    def review(self):
        return {'type': 'edit_proposal', 'source': 'untrusted-edit-data',
                'path': self.path, 'before_sha256': self.before_sha256,
                'after_sha256': self.after_sha256, 'before': self.before,
                'after': self.after, 'diff': self.diff,
                'applied': False, 'requires_owner_review': True}


def propose_edit(path, expected_sha256, approved_snapshot, replacement):
    parts = _parts(path)
    if Path(parts[-1]).suffix.casefold() not in TEXT_EXTENSIONS:
        raise ProjectReadError('Unsupported edit text extension')
    if type(expected_sha256) is not str or re.fullmatch('[0-9a-f]{64}', expected_sha256) is None:
        raise ProjectReadError('A full approved snapshot digest is required')
    before, after = _text(approved_snapshot), _text(replacement)
    if hashlib.sha256(before).hexdigest() != expected_sha256:
        raise ProjectReadError('Approved edit snapshot is stale or incomplete')
    if before == after:
        raise ProjectReadError('Edit proposal makes no change')
    # The JSON review includes exact before/after, including EOF-newline state.
    # The unified diff is only a visual aid, never a patch to apply or execute.
    before_lines = approved_snapshot.splitlines(keepends=True)
    after_lines = replacement.splitlines(keepends=True)
    diff = ''.join(difflib.unified_diff(before_lines, after_lines,
                  fromfile='before/' + path, tofile='after/' + path, lineterm='\n'))
    if len(diff.encode('utf-8')) > MAX_DIFF_BYTES:
        raise ProjectReadError('Edit diff exceeded preview bounds')
    return EditProposal(path=path, before_sha256=expected_sha256,
                        after_sha256=hashlib.sha256(after).hexdigest(),
                        before=approved_snapshot, after=replacement, diff=diff)
