"""Owner review lifecycle for pure proposals, never an apply permission.

No production imports this preparatory module. The owner must supply current
project/turn/snapshot identity. Accepting a review can export an exact proposal
for later manual use; it cannot authorize a filesystem mutation.
"""
import hashlib
import json
import math
import secrets
import threading
import time
from project_edits import EditProposal, propose_edit, ProjectReadError


def proposal_digest(proposal):
    if type(proposal) is not EditProposal:
        raise ProjectReadError('An exact edit proposal is required')
    # Revalidate even manually constructed dataclasses before presenting them.
    checked = propose_edit(proposal.path, proposal.before_sha256,
                           proposal.before, proposal.after)
    if checked != proposal:
        raise ProjectReadError('Edit proposal binding is inconsistent')
    payload = json.dumps({'path': proposal.path, 'before_sha256': proposal.before_sha256,
                          'after_sha256': proposal.after_sha256, 'before': proposal.before,
                          'after': proposal.after}, ensure_ascii=True, sort_keys=True,
                         separators=(',', ':')).encode('ascii')
    return hashlib.sha256(payload).hexdigest()


class EditReviews:
    """One pending review bound to an owner project, turn and snapshot digest."""
    def __init__(self, scope, *, clock=time.monotonic, timeout=60, max_reviews=4):
        if type(scope) is not str or not scope or len(scope) > 256:
            raise ProjectReadError('Invalid edit review owner scope')
        if type(timeout) not in (int, float) or not math.isfinite(timeout) or not 0 < timeout <= 120:
            raise ProjectReadError('Invalid edit review deadline')
        if type(max_reviews) is not int or not 1 <= max_reviews <= 4:
            raise ProjectReadError('Invalid edit review budget')
        self.scope, self.clock, self.timeout = scope, clock, timeout
        self.remaining, self.pending, self.closed = max_reviews, None, False
        self.lock = threading.Lock()

    def request(self, proposal, scope):
        with self.lock:
            if self.closed or scope != self.scope or self.remaining <= 0:
                raise ProjectReadError('Edit review scope closed or exhausted')
            if self.pending and self.clock() >= self.pending['deadline']:
                self.pending = None
            if self.pending:
                raise ProjectReadError('An edit review is already pending')
            self.remaining -= 1
            digest = proposal_digest(proposal)
            token = 'edit-review-' + secrets.token_hex(16)
            self.pending = {'id': token, 'digest': digest, 'proposal': proposal,
                            'deadline': self.clock() + self.timeout}
            # Copies contain exact contents. Caller mutation cannot change binding.
            return {'type': 'edit_review', 'id': token, 'scope': self.scope,
                    'proposal_digest': digest, 'proposal': proposal.review(),
                    'choices': ['accept_proposal', 'reject'],
                    'notice': 'Review only. Acceptance does not write or apply changes.'}

    def decide(self, token, digest, decision, scope, current_snapshot_sha256):
        with self.lock:
            pending = self.pending
            if self.closed or scope != self.scope or not pending or token != pending['id']:
                raise ProjectReadError('Edit review is absent or stale')
            if digest != pending['digest'] or decision not in ('accept_proposal', 'reject'):
                raise ProjectReadError('Exact proposal review decision required')
            self.pending = None
            if self.clock() >= pending['deadline']:
                raise ProjectReadError('Edit review expired')
            proposal = pending['proposal']
            if current_snapshot_sha256 != proposal.before_sha256:
                raise ProjectReadError('Edit review snapshot is no longer current')
            if decision == 'reject':
                return {'accepted': False, 'applied': False}
            return {'accepted': True, 'applied': False, 'proposal_digest': pending['digest'],
                    'proposal': proposal.review(),
                    'notice': 'Proposal accepted for manual export only. No write occurred.'}

    def close(self):
        with self.lock:
            self.closed = True
            self.pending = None
