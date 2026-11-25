"""In-memory one-shot approval broker for future whole-project candidate snapshots.

PREPARATORY PERMISSION STATE MACHINE ONLY:
- Performs NO project reads, NO filesystem verification, NO candidate snapshot
  creation, NO test execution, and NO disk persistence (daemon restart
  invalidates all requests and grants).
- Cannot grant authority on its own and cannot verify the filesystem itself.
  In-process Python attributes and object sentinels are an API layering guard
  against accidental misuse, NOT an authentication boundary against malicious
  same-process code. The trusted daemon process owns channel authentication,
  process isolation, and componentwise root-identity derivation/revalidation.
- Snapshot generation and candidate execution remain unavailable until a
  daemon-bound owner route, a separately reviewed exclusive cgroup launcher,
  an owner provisioning decision, and capped tmpfs support are integrated.
"""
from collections import deque
from dataclasses import dataclass
import hashlib
import hmac
import json
import math
from pathlib import Path
import re
import secrets
import sys
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from project_candidate_test import (
    ALLOWED_EXTENSIONS,
    MAX_DIR_DEPTH,
    MAX_FILE_BYTES,
    MAX_SNAPSHOT_DIRS,
    MAX_SNAPSHOT_FILES,
    MAX_TOTAL_BYTES,
    MAX_VISITED_ENTRIES,
    RUNNER_NAME,
)
from project_edits import ProjectReadError

AUTHORITY_MODEL = 'DELEGATED_TO_FUTURE_DAEMON_OWNER_CHANNEL'
STANDALONE_EXECUTION_ENABLED = False

DESCRIPTOR_SCHEMA = 'navi-whole-project-snapshot-scope-v1'
ALLOWED_DECISIONS = ('allow_once', 'deny', 'cancel')
CANONICAL_ALLOWED_EXTENSIONS = tuple(sorted(ALLOWED_EXTENSIONS))

DEFAULT_REQUEST_TIMEOUT_SECONDS = 60.0
MIN_REQUEST_TIMEOUT_SECONDS = 1.0
MAX_REQUEST_TIMEOUT_SECONDS = 300.0

DEFAULT_GRANT_TTL_SECONDS = 10.0
MIN_GRANT_TTL_SECONDS = 1.0
MAX_GRANT_TTL_SECONDS = 30.0

MAX_PROJECT_ID_LENGTH = 128
MAX_INT64 = (1 << 63) - 1
# Bounded history is for collision avoidance and diagnostics only; stale IDs
# evicted beyond 512 still never match the single active request or grant.
MAX_FINALIZED_HISTORY = 512
MAX_MINT_ATTEMPTS = 8

_REQUEST_ID_RE = re.compile(r'^snapreq-[0-9a-f]{48}$')
_GRANT_TOKEN_RE = re.compile(r'^snapgrant-[0-9a-f]{48}$')
_HEX64_RE = re.compile(r'^[0-9a-f]{64}$')
_UNSAFE_TEXT_RE = re.compile(r'[\x00-\x1f\x7f\u202a-\u202e\u2066-\u2069]')

_CHANNEL_SENTINEL = object()


class SnapshotApprovalError(ProjectReadError):
    """Fail-closed error for invalid, stale, replayed, or unauthorized snapshot approvals."""


@dataclass(frozen=True)
class SnapshotApprovalContext:
    """Live daemon session/project/root/connection binding.

    Must be supplied exclusively by the trusted daemon (never from model or
    project content). `root_identity_sha256` is an opaque 64-hex SHA-256 hash
    derived by the daemon from the componentwise-reopened project root identity
    (e.g., domain-tagged st_dev/st_ino, never exposing source paths). This
    module cannot verify the filesystem itself; the future daemon must derive
    and revalidate `root_identity_sha256` before snapshot read, spawn, and return.
    """
    project_id: str
    root_identity_sha256: str
    generation: int
    project_epoch: int
    connection_epoch: int


@dataclass(frozen=True)
class SnapshotScopeDescriptor:
    """Fixed snapshot caps and eligibility descriptor bound into the approval digest.

    Until candidate snapshot enforcement is parameterized and reviewed, ONLY
    the exact compiled default descriptor (`FIXED_SNAPSHOT_DESCRIPTOR`) is accepted.
    """
    schema: str = DESCRIPTOR_SCHEMA
    runner: str = RUNNER_NAME
    allowed_extensions: tuple = CANONICAL_ALLOWED_EXTENSIONS
    max_files: int = MAX_SNAPSHOT_FILES
    max_dirs: int = MAX_SNAPSHOT_DIRS
    max_file_bytes: int = MAX_FILE_BYTES
    max_total_bytes: int = MAX_TOTAL_BYTES
    max_visited_entries: int = MAX_VISITED_ENTRIES
    max_dir_depth: int = MAX_DIR_DEPTH
    exclude_hidden: bool = True
    regular_utf8_no_nul_only: bool = True
    reject_symlinks_hardlinks_special: bool = True
    local_only_no_model_disclosure: bool = True
    primary_project_read_only: bool = True


FIXED_SNAPSHOT_DESCRIPTOR = SnapshotScopeDescriptor()


@dataclass(frozen=True)
class SnapshotApprovalPrompt:
    """Owner-UI prompt metadata only (no source paths, file contents, or Always option)."""
    request_id: str
    scope_digest: str
    descriptor_digest: str
    project_id: str
    root_identity_sha256: str
    runner: str
    proposal_digest: str
    descriptor: SnapshotScopeDescriptor
    allowed_decisions: tuple
    expires_in_seconds: float
    notice: str


@dataclass(frozen=True)
class OwnerDecision:
    """Structured decision returned by a daemon-wired trusted owner callback."""
    request_id: str
    scope_digest: str
    decision: str


@dataclass(frozen=True)
class OwnerDecisionOutcome:
    """Result of validating one owner decision on the trusted owner channel."""
    decision: str
    status: str
    request_id: str
    scope_digest: str
    descriptor_digest: str
    grant_token: object
    grant_expires_at: object


@dataclass(frozen=True)
class ConsumedSnapshotReceipt:
    """One-shot verification receipt; does NOT authorize standalone snapshot reads or execution."""
    request_id: str
    scope_digest: str
    descriptor_digest: str
    project_id: str
    root_identity_sha256: str
    generation: int
    project_epoch: int
    connection_epoch: int
    runner: str
    proposal_digest: str
    one_shot_consumed: bool = True
    grants_standalone_authority: bool = False


@dataclass(frozen=True)
class _PendingRequest:
    request_id: str
    scope_digest: str
    descriptor_digest: str
    context: SnapshotApprovalContext
    runner: str
    proposal_digest: str
    descriptor: SnapshotScopeDescriptor
    created_at: float
    expires_at: float
    prompt: SnapshotApprovalPrompt


@dataclass(frozen=True)
class _ActiveGrant:
    grant_token: str
    request_id: str
    scope_digest: str
    descriptor_digest: str
    context: SnapshotApprovalContext
    runner: str
    proposal_digest: str
    approved_at: float
    expires_at: float


def _canonical_json(value):
    return json.dumps(value, ensure_ascii=True, separators=(',', ':'), sort_keys=True)


def _validate_non_negative_int(value, field_name):
    if type(value) is not int or not (0 <= value <= MAX_INT64):
        raise SnapshotApprovalError(f'Invalid {field_name}')


def validate_root_identity_sha256(value):
    if type(value) is not str or _HEX64_RE.fullmatch(value) is None:
        raise SnapshotApprovalError(
            'Exact opaque 64-hex SHA-256 root_identity_sha256 is required'
        )
    return value


def validate_context(context):
    if type(context) is not SnapshotApprovalContext:
        raise SnapshotApprovalError('Exact SnapshotApprovalContext is required')
    pid = context.project_id
    if (
        type(pid) is not str
        or not (1 <= len(pid) <= MAX_PROJECT_ID_LENGTH)
        or pid.strip() != pid
        or _UNSAFE_TEXT_RE.search(pid) is not None
    ):
        raise SnapshotApprovalError('Invalid project_id in snapshot approval context')
    validate_root_identity_sha256(context.root_identity_sha256)
    _validate_non_negative_int(context.generation, 'generation')
    _validate_non_negative_int(context.project_epoch, 'project_epoch')
    _validate_non_negative_int(context.connection_epoch, 'connection_epoch')
    return context


def validate_runner(runner):
    if type(runner) is not str or runner != RUNNER_NAME:
        raise SnapshotApprovalError('Unsupported candidate runner for snapshot approval')
    return runner


def validate_proposal_digest(proposal_digest):
    if type(proposal_digest) is not str or _HEX64_RE.fullmatch(proposal_digest) is None:
        raise SnapshotApprovalError('Exact 64-hex SHA-256 proposal_digest is required')
    return proposal_digest


def validate_snapshot_descriptor(descriptor):
    """Require the exact fixed default snapshot descriptor; reject narrower or wider variants."""
    if type(descriptor) is not SnapshotScopeDescriptor:
        raise SnapshotApprovalError('Exact SnapshotScopeDescriptor is required')
    if (
        type(descriptor.schema) is not str
        or descriptor.schema != DESCRIPTOR_SCHEMA
        or type(descriptor.runner) is not str
        or descriptor.runner != RUNNER_NAME
        or type(descriptor.allowed_extensions) is not tuple
        or descriptor.allowed_extensions != CANONICAL_ALLOWED_EXTENSIONS
        or type(descriptor.max_files) is not int
        or descriptor.max_files != MAX_SNAPSHOT_FILES
        or type(descriptor.max_dirs) is not int
        or descriptor.max_dirs != MAX_SNAPSHOT_DIRS
        or type(descriptor.max_file_bytes) is not int
        or descriptor.max_file_bytes != MAX_FILE_BYTES
        or type(descriptor.max_total_bytes) is not int
        or descriptor.max_total_bytes != MAX_TOTAL_BYTES
        or type(descriptor.max_visited_entries) is not int
        or descriptor.max_visited_entries != MAX_VISITED_ENTRIES
        or type(descriptor.max_dir_depth) is not int
        or descriptor.max_dir_depth != MAX_DIR_DEPTH
        or descriptor.exclude_hidden is not True
        or descriptor.regular_utf8_no_nul_only is not True
        or descriptor.reject_symlinks_hardlinks_special is not True
        or descriptor.local_only_no_model_disclosure is not True
        or descriptor.primary_project_read_only is not True
    ):
        raise SnapshotApprovalError(
            'Only the exact fixed default snapshot descriptor is supported'
        )
    return descriptor


def compute_descriptor_digest(descriptor):
    """Compute canonical SHA-256 digest of the exact fixed snapshot descriptor."""
    validate_snapshot_descriptor(descriptor)
    payload = {
        'schema': descriptor.schema,
        'runner': descriptor.runner,
        'allowed_extensions': list(descriptor.allowed_extensions),
        'max_files': descriptor.max_files,
        'max_dirs': descriptor.max_dirs,
        'max_file_bytes': descriptor.max_file_bytes,
        'max_total_bytes': descriptor.max_total_bytes,
        'max_visited_entries': descriptor.max_visited_entries,
        'max_dir_depth': descriptor.max_dir_depth,
        'exclude_hidden': True,
        'regular_utf8_no_nul_only': True,
        'reject_symlinks_hardlinks_special': True,
        'local_only_no_model_disclosure': True,
        'primary_project_read_only': True,
    }
    raw = ('navi-snapshot-descriptor-v1:' + _canonical_json(payload)).encode('utf-8')
    return hashlib.sha256(raw).hexdigest()


FIXED_DESCRIPTOR_DIGEST = compute_descriptor_digest(FIXED_SNAPSHOT_DESCRIPTOR)


def compute_scope_digest(context, *, runner, proposal_digest, descriptor=FIXED_SNAPSHOT_DESCRIPTOR):
    """Compute canonical SHA-256 binding live context, root identity, runner, proposal, and descriptor."""
    validate_context(context)
    validate_runner(runner)
    validate_proposal_digest(proposal_digest)
    validate_snapshot_descriptor(descriptor)
    descriptor_digest = compute_descriptor_digest(descriptor)
    payload = {
        'project_id': context.project_id,
        'root_identity_sha256': context.root_identity_sha256,
        'generation': context.generation,
        'project_epoch': context.project_epoch,
        'connection_epoch': context.connection_epoch,
        'runner': runner,
        'proposal_digest': proposal_digest,
        'descriptor_digest': descriptor_digest,
    }
    raw = ('navi-snapshot-approval-scope-v1:' + _canonical_json(payload)).encode('utf-8')
    return descriptor_digest, hashlib.sha256(raw).hexdigest()


def _format_owner_notice(descriptor):
    exts = ', '.join(descriptor.allowed_extensions)
    return (
        f'Whole-project local snapshot approval (Allow once / Deny / Cancel only; no Always). '
        f'Eligible non-hidden regular UTF-8 files ({exts}) up to {descriptor.max_files} files, '
        f'{descriptor.max_dirs} directories, {descriptor.max_file_bytes} bytes per file, and '
        f'{descriptor.max_total_bytes} bytes total may be copied into private temporary storage '
        f'for local-only verification. Source files can still contain secrets; primary project '
        f'files are never written and snapshot/output contents are never automatically disclosed '
        f'to models.'
    )


class ProjectSnapshotApprovalBroker:
    """Single-active-request, in-memory one-shot approval state machine.

    The paired channel key is an in-process API layering guard only, NOT an
    authentication boundary against malicious same-process code; the trusted
    daemon process owns owner-channel authentication and isolation.
    """

    def __init__(
        self,
        *,
        _channel_key=None,
        clock=time.monotonic,
        request_timeout_seconds=DEFAULT_REQUEST_TIMEOUT_SECONDS,
        grant_ttl_seconds=DEFAULT_GRANT_TTL_SECONDS,
    ):
        if _channel_key is None:
            raise SnapshotApprovalError(
                'Use create_snapshot_approval_broker() to construct a paired broker and owner channel'
            )
        if not callable(clock):
            raise SnapshotApprovalError('A callable monotonic clock is required')
        if (
            type(request_timeout_seconds) is bool
            or not isinstance(request_timeout_seconds, (int, float))
            or not math.isfinite(request_timeout_seconds)
            or not (
                MIN_REQUEST_TIMEOUT_SECONDS
                <= float(request_timeout_seconds)
                <= MAX_REQUEST_TIMEOUT_SECONDS
            )
        ):
            raise SnapshotApprovalError('Invalid request_timeout_seconds')
        if (
            type(grant_ttl_seconds) is bool
            or not isinstance(grant_ttl_seconds, (int, float))
            or not math.isfinite(grant_ttl_seconds)
            or not (
                MIN_GRANT_TTL_SECONDS <= float(grant_ttl_seconds) <= MAX_GRANT_TTL_SECONDS
            )
        ):
            raise SnapshotApprovalError('Invalid grant_ttl_seconds')

        self._channel_key = _channel_key
        self._clock = clock
        self._request_timeout_seconds = float(request_timeout_seconds)
        self._grant_ttl_seconds = float(grant_ttl_seconds)
        self._lock = threading.Lock()
        self._last_observed_time = None
        self._pending = None
        self._grant = None
        self._finalized_requests = set()
        self._finalized_requests_order = deque()
        self._finalized_grants = set()
        self._finalized_grants_order = deque()

    def _record_finalized_locked(self, token, seen_set, order_deque):
        if token in seen_set:
            return
        if len(order_deque) >= MAX_FINALIZED_HISTORY:
            oldest = order_deque.popleft()
            seen_set.discard(oldest)
        seen_set.add(token)
        order_deque.append(token)

    def _mint_unique_token_locked(self, prefix, seen_set, active_token=None):
        for _ in range(MAX_MINT_ATTEMPTS):
            candidate = prefix + secrets.token_hex(24)
            if candidate not in seen_set and candidate != active_token:
                return candidate
        raise SnapshotApprovalError('Failed to mint unique opaque approval token')

    def _clear_active_locked(self):
        if self._pending is not None:
            self._record_finalized_locked(
                self._pending.request_id,
                self._finalized_requests,
                self._finalized_requests_order,
            )
            self._pending = None
        if self._grant is not None:
            self._record_finalized_locked(
                self._grant.grant_token,
                self._finalized_grants,
                self._finalized_grants_order,
            )
            self._grant = None

    def _now_locked(self):
        value = self._clock()
        if type(value) is bool or not isinstance(value, (int, float)) or not math.isfinite(value):
            self._clear_active_locked()
            raise SnapshotApprovalError('Monotonic clock returned an invalid timestamp')
        now = float(value)
        if self._last_observed_time is not None and now < self._last_observed_time:
            self._clear_active_locked()
            raise SnapshotApprovalError(
                'Monotonic clock regressed below last observed timestamp; active state invalidated'
            )
        self._last_observed_time = now
        return now

    def _purge_expired_locked(self, now):
        if self._pending is not None and now >= self._pending.expires_at:
            self._record_finalized_locked(
                self._pending.request_id,
                self._finalized_requests,
                self._finalized_requests_order,
            )
            self._pending = None
        if self._grant is not None and now >= self._grant.expires_at:
            self._record_finalized_locked(
                self._grant.grant_token,
                self._finalized_grants,
                self._finalized_grants_order,
            )
            self._grant = None

    def request_approval(
        self, context, *, runner=RUNNER_NAME, proposal_digest, descriptor=FIXED_SNAPSHOT_DESCRIPTOR
    ):
        """Open a single fresh one-shot whole-project snapshot approval request."""
        descriptor_digest, scope_digest = compute_scope_digest(
            context,
            runner=runner,
            proposal_digest=proposal_digest,
            descriptor=descriptor,
        )
        with self._lock:
            now = self._now_locked()
            self._purge_expired_locked(now)
            if self._pending is not None or self._grant is not None:
                raise SnapshotApprovalError('A whole-project snapshot approval is already active')
            request_id = self._mint_unique_token_locked(
                'snapreq-', self._finalized_requests
            )
            prompt = SnapshotApprovalPrompt(
                request_id=request_id,
                scope_digest=scope_digest,
                descriptor_digest=descriptor_digest,
                project_id=context.project_id,
                root_identity_sha256=context.root_identity_sha256,
                runner=runner,
                proposal_digest=proposal_digest,
                descriptor=descriptor,
                allowed_decisions=ALLOWED_DECISIONS,
                expires_in_seconds=self._request_timeout_seconds,
                notice=_format_owner_notice(descriptor),
            )
            self._pending = _PendingRequest(
                request_id=request_id,
                scope_digest=scope_digest,
                descriptor_digest=descriptor_digest,
                context=context,
                runner=runner,
                proposal_digest=proposal_digest,
                descriptor=descriptor,
                created_at=now,
                expires_at=now + self._request_timeout_seconds,
                prompt=prompt,
            )
            return prompt

    def _submit_owner_decision(self, channel_key, *, request_id, scope_digest, decision, context):
        if channel_key is not self._channel_key:
            raise SnapshotApprovalError('Untrusted owner decision channel')
        if type(decision) is not str or decision not in ALLOWED_DECISIONS:
            raise SnapshotApprovalError(
                'Owner decision must be allow_once, deny, or cancel; Always and raw booleans are forbidden'
            )
        if type(request_id) is not str or _REQUEST_ID_RE.fullmatch(request_id) is None:
            raise SnapshotApprovalError('Invalid snapshot approval request_id')
        if type(scope_digest) is not str or _HEX64_RE.fullmatch(scope_digest) is None:
            raise SnapshotApprovalError('Invalid snapshot approval scope_digest')
        validate_context(context)

        with self._lock:
            now = self._now_locked()
            self._purge_expired_locked(now)
            pending = self._pending
            if pending is None:
                if request_id in self._finalized_requests:
                    raise SnapshotApprovalError(
                        'Snapshot approval request was already decided, expired, or invalidated'
                    )
                raise SnapshotApprovalError('No matching active snapshot approval request')
            if not hmac.compare_digest(request_id, pending.request_id):
                raise SnapshotApprovalError('Snapshot approval request_id mismatch')
            if (
                not hmac.compare_digest(scope_digest, pending.scope_digest)
                or context != pending.context
            ):
                self._record_finalized_locked(
                    pending.request_id,
                    self._finalized_requests,
                    self._finalized_requests_order,
                )
                self._pending = None
                raise SnapshotApprovalError(
                    'Snapshot approval scope or live context drifted; pending request invalidated'
                )

            self._record_finalized_locked(
                pending.request_id,
                self._finalized_requests,
                self._finalized_requests_order,
            )
            self._pending = None

            if decision != 'allow_once':
                return OwnerDecisionOutcome(
                    decision=decision,
                    status='denied' if decision == 'deny' else 'canceled',
                    request_id=pending.request_id,
                    scope_digest=pending.scope_digest,
                    descriptor_digest=pending.descriptor_digest,
                    grant_token=None,
                    grant_expires_at=None,
                )

            grant_token = self._mint_unique_token_locked(
                'snapgrant-', self._finalized_grants
            )
            grant_expires_at = now + self._grant_ttl_seconds
            self._grant = _ActiveGrant(
                grant_token=grant_token,
                request_id=pending.request_id,
                scope_digest=pending.scope_digest,
                descriptor_digest=pending.descriptor_digest,
                context=pending.context,
                runner=pending.runner,
                proposal_digest=pending.proposal_digest,
                approved_at=now,
                expires_at=grant_expires_at,
            )
            return OwnerDecisionOutcome(
                decision='allow_once',
                status='approved_once',
                request_id=pending.request_id,
                scope_digest=pending.scope_digest,
                descriptor_digest=pending.descriptor_digest,
                grant_token=grant_token,
                grant_expires_at=grant_expires_at,
            )

    def consume_grant(
        self,
        *,
        grant_token,
        request_id,
        scope_digest,
        context,
        runner=RUNNER_NAME,
        proposal_digest,
        descriptor=FIXED_SNAPSHOT_DESCRIPTOR,
    ):
        """Consume a one-shot grant after recomputing and revalidating full context/scope."""
        if type(grant_token) is not str or _GRANT_TOKEN_RE.fullmatch(grant_token) is None:
            raise SnapshotApprovalError('Invalid snapshot approval grant_token')

        with self._lock:
            now = self._now_locked()
            self._purge_expired_locked(now)
            grant = self._grant
            if grant is None:
                if grant_token in self._finalized_grants:
                    raise SnapshotApprovalError(
                        'Snapshot approval grant was already consumed, expired, or invalidated'
                    )
                raise SnapshotApprovalError('No active unconsumed snapshot approval grant')
            if not hmac.compare_digest(grant_token, grant.grant_token):
                raise SnapshotApprovalError('Snapshot approval grant_token mismatch')

            # Burn the matching grant unconditionally before checking fields so it cannot be retried.
            self._record_finalized_locked(
                grant.grant_token,
                self._finalized_grants,
                self._finalized_grants_order,
            )
            self._grant = None

            if type(request_id) is not str or _REQUEST_ID_RE.fullmatch(request_id) is None:
                raise SnapshotApprovalError('Invalid snapshot approval request_id; grant burned')
            if type(scope_digest) is not str or _HEX64_RE.fullmatch(scope_digest) is None:
                raise SnapshotApprovalError('Invalid snapshot approval scope_digest; grant burned')
            expected_descriptor_digest, expected_scope_digest = compute_scope_digest(
                context,
                runner=runner,
                proposal_digest=proposal_digest,
                descriptor=descriptor,
            )

            if (
                not hmac.compare_digest(request_id, grant.request_id)
                or not hmac.compare_digest(scope_digest, grant.scope_digest)
                or not hmac.compare_digest(expected_scope_digest, grant.scope_digest)
                or not hmac.compare_digest(expected_descriptor_digest, grant.descriptor_digest)
                or context != grant.context
                or runner != grant.runner
                or not hmac.compare_digest(proposal_digest, grant.proposal_digest)
            ):
                raise SnapshotApprovalError(
                    'Snapshot approval context or scope changed since approval; grant burned'
                )

            return ConsumedSnapshotReceipt(
                request_id=grant.request_id,
                scope_digest=grant.scope_digest,
                descriptor_digest=grant.descriptor_digest,
                project_id=grant.context.project_id,
                root_identity_sha256=grant.context.root_identity_sha256,
                generation=grant.context.generation,
                project_epoch=grant.context.project_epoch,
                connection_epoch=grant.context.connection_epoch,
                runner=grant.runner,
                proposal_digest=grant.proposal_digest,
                one_shot_consumed=True,
                grants_standalone_authority=False,
            )

    def invalidate_stop(self):
        """Invalidate any pending request or unconsumed grant on Stop."""
        with self._lock:
            self._clear_active_locked()

    def invalidate_project_switch(self):
        """Invalidate any pending request or unconsumed grant on project switch."""
        with self._lock:
            self._clear_active_locked()

    def invalidate_disconnect(self):
        """Invalidate any pending request or unconsumed grant on owner disconnect."""
        with self._lock:
            self._clear_active_locked()

    def active_state(self):
        with self._lock:
            now = self._now_locked()
            self._purge_expired_locked(now)
            if self._pending is not None:
                return 'pending'
            if self._grant is not None:
                return 'approved_unconsumed'
            return 'idle'

    def pending_prompt(self):
        with self._lock:
            now = self._now_locked()
            self._purge_expired_locked(now)
            return self._pending.prompt if self._pending is not None else None


class TrustedOwnerDecisionChannel:
    """Owner-RPC decision entrypoint paired with one broker.

    In-process Python attributes are NOT a security boundary against malicious
    same-process code; the trusted daemon process owns channel binding and must
    never expose this channel to model or project-derived inputs.
    """

    def __init__(self, *, _capability=None, _broker=None, _channel_key=None):
        if _capability is not _CHANNEL_SENTINEL or _broker is None or _channel_key is None:
            raise SnapshotApprovalError(
                'TrustedOwnerDecisionChannel can only be created via create_snapshot_approval_broker()'
            )
        self._broker = _broker
        self._channel_key = _channel_key

    def submit_decision(self, *, request_id, scope_digest, decision, context):
        """Validate and apply one explicit owner decision (allow_once / deny / cancel)."""
        return self._broker._submit_owner_decision(
            self._channel_key,
            request_id=request_id,
            scope_digest=scope_digest,
            decision=decision,
            context=context,
        )

    def invoke_callback(self, callback, *, prompt, get_live_context):
        """Invoke a daemon-wired trusted owner callback and validate its returned OwnerDecision."""
        if not callable(callback) or not callable(get_live_context):
            raise SnapshotApprovalError(
                'Trusted owner callback and live context provider are required'
            )
        if type(prompt) is not SnapshotApprovalPrompt:
            raise SnapshotApprovalError('Exact SnapshotApprovalPrompt is required')
        response = callback(prompt)
        if type(response) is not OwnerDecision:
            raise SnapshotApprovalError(
                'Owner callback must return an exact OwnerDecision; raw booleans and dicts are refused'
            )
        live_context = get_live_context()
        return self.submit_decision(
            request_id=response.request_id,
            scope_digest=response.scope_digest,
            decision=response.decision,
            context=live_context,
        )


def create_snapshot_approval_broker(
    *,
    clock=time.monotonic,
    request_timeout_seconds=DEFAULT_REQUEST_TIMEOUT_SECONDS,
    grant_ttl_seconds=DEFAULT_GRANT_TTL_SECONDS,
):
    """Create one paired (ProjectSnapshotApprovalBroker, TrustedOwnerDecisionChannel)."""
    channel_key = object()
    broker = ProjectSnapshotApprovalBroker(
        _channel_key=channel_key,
        clock=clock,
        request_timeout_seconds=request_timeout_seconds,
        grant_ttl_seconds=grant_ttl_seconds,
    )
    owner_channel = TrustedOwnerDecisionChannel(
        _capability=_CHANNEL_SENTINEL,
        _broker=broker,
        _channel_key=channel_key,
    )
    return broker, owner_channel


if __name__ == '__main__':
    print(
        json.dumps(
            {
                'error': (
                    'project_snapshot_approvals.py is a preparatory permission state-machine '
                    'library only. It cannot authorize whole-project reads or execute tests '
                    'without a daemon-wired trusted owner channel.'
                ),
                'status': 'library_only_not_wired',
                'authorized': False,
                'snapshot_read': False,
                'executed_project_code': False,
            },
            ensure_ascii=True,
        ),
        flush=True,
    )
    raise SystemExit(1)