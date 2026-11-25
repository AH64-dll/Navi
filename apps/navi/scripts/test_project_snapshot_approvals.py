"""Synthetic local verification tests for preparatory project_snapshot_approvals.py.

Verifies the whole-project snapshot approval state machine only. No snapshot
reads, project file opens, execution, persistence, UI, or Rust wiring occur.
Local tests are verification, not proof of safety.
"""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import threading
import unittest
from unittest import mock

WORKERS_DIR = Path(__file__).resolve().parent.parent / 'workers'
sys.path.insert(0, str(WORKERS_DIR))

import project_candidate_test as pct
import project_snapshot_approvals as psa
from project_snapshot_approvals import (
    FIXED_SNAPSHOT_DESCRIPTOR,
    OwnerDecision,
    SnapshotApprovalContext,
    SnapshotApprovalError,
    SnapshotScopeDescriptor,
    create_snapshot_approval_broker,
)


class FakeClock:
    def __init__(self, start=100.0):
        self.value = float(start)

    def __call__(self):
        return self.value

    def advance(self, delta):
        self.value += float(delta)


def _digest(label):
    return hashlib.sha256(label.encode('utf-8')).hexdigest()


def _root_id(tag='dev42:ino9001'):
    return _digest('navi-root-identity-v1:' + tag)


def _ctx(
    project_id='proj-alpha',
    root_identity_sha256=None,
    generation=3,
    project_epoch=7,
    connection_epoch=11,
):
    if root_identity_sha256 is None:
        root_identity_sha256 = _root_id()
    return SnapshotApprovalContext(
        project_id=project_id,
        root_identity_sha256=root_identity_sha256,
        generation=generation,
        project_epoch=project_epoch,
        connection_epoch=connection_epoch,
    )


class TestDescriptorAndDigestBinding(unittest.TestCase):
    def test_fixed_descriptor_matches_candidate_snapshot_caps_and_hashes_deterministically(self):
        desc = FIXED_SNAPSHOT_DESCRIPTOR
        self.assertEqual(desc.runner, pct.RUNNER_NAME)
        self.assertEqual(desc.allowed_extensions, tuple(sorted(pct.ALLOWED_EXTENSIONS)))
        self.assertEqual(desc.max_files, pct.MAX_SNAPSHOT_FILES)
        self.assertEqual(desc.max_dirs, pct.MAX_SNAPSHOT_DIRS)
        self.assertEqual(desc.max_file_bytes, pct.MAX_FILE_BYTES)
        self.assertEqual(desc.max_total_bytes, pct.MAX_TOTAL_BYTES)
        self.assertEqual(desc.max_visited_entries, pct.MAX_VISITED_ENTRIES)
        self.assertEqual(desc.max_dir_depth, pct.MAX_DIR_DEPTH)

        d1 = psa.compute_descriptor_digest(desc)
        d2 = psa.compute_descriptor_digest(SnapshotScopeDescriptor())
        self.assertEqual(d1, d2)
        self.assertEqual(d1, psa.FIXED_DESCRIPTOR_DIGEST)
        self.assertEqual(len(d1), 64)

    def test_descriptor_rejects_every_narrower_and_wider_field_and_extension_subset(self):
        non_default_descriptors = [
            # Schema / runner
            SnapshotScopeDescriptor(schema='navi-whole-project-snapshot-scope-v2'),
            SnapshotScopeDescriptor(runner='node-test'),
            # Extension subsets, reordering, duplicates, extras, wrong container type
            SnapshotScopeDescriptor(allowed_extensions=()),
            SnapshotScopeDescriptor(allowed_extensions=('.py',)),
            SnapshotScopeDescriptor(allowed_extensions=('.json', '.py')),
            SnapshotScopeDescriptor(allowed_extensions=('.json', '.py', '.toml')),
            SnapshotScopeDescriptor(allowed_extensions=('.py', '.txt')),
            SnapshotScopeDescriptor(allowed_extensions=('.py', '.json', '.toml', '.txt')),
            SnapshotScopeDescriptor(allowed_extensions=('.json', '.py', '.py', '.toml', '.txt')),
            SnapshotScopeDescriptor(allowed_extensions=('.json', '.py', '.sh', '.toml', '.txt')),
            SnapshotScopeDescriptor(allowed_extensions=['.json', '.py', '.toml', '.txt']),
            # Narrower AND wider individual numeric caps + bool confusion
            SnapshotScopeDescriptor(max_files=pct.MAX_SNAPSHOT_FILES - 1),
            SnapshotScopeDescriptor(max_files=pct.MAX_SNAPSHOT_FILES + 1),
            SnapshotScopeDescriptor(max_files=True),
            SnapshotScopeDescriptor(max_dirs=pct.MAX_SNAPSHOT_DIRS - 1),
            SnapshotScopeDescriptor(max_dirs=pct.MAX_SNAPSHOT_DIRS + 1),
            SnapshotScopeDescriptor(max_file_bytes=pct.MAX_FILE_BYTES - 1),
            SnapshotScopeDescriptor(max_file_bytes=pct.MAX_FILE_BYTES + 1),
            SnapshotScopeDescriptor(max_total_bytes=pct.MAX_TOTAL_BYTES - 1),
            SnapshotScopeDescriptor(max_total_bytes=pct.MAX_TOTAL_BYTES + 1),
            SnapshotScopeDescriptor(max_visited_entries=pct.MAX_VISITED_ENTRIES - 1),
            SnapshotScopeDescriptor(max_visited_entries=pct.MAX_VISITED_ENTRIES + 1),
            SnapshotScopeDescriptor(max_dir_depth=pct.MAX_DIR_DEPTH - 1),
            SnapshotScopeDescriptor(max_dir_depth=pct.MAX_DIR_DEPTH + 1),
            # Weakened boolean invariants
            SnapshotScopeDescriptor(exclude_hidden=False),
            SnapshotScopeDescriptor(regular_utf8_no_nul_only=False),
            SnapshotScopeDescriptor(reject_symlinks_hardlinks_special=False),
            SnapshotScopeDescriptor(local_only_no_model_disclosure=False),
            SnapshotScopeDescriptor(primary_project_read_only=False),
            SnapshotScopeDescriptor(exclude_hidden=1),
        ]
        for desc in non_default_descriptors:
            with self.assertRaises(SnapshotApprovalError, msg=f'should reject {desc}'):
                psa.validate_snapshot_descriptor(desc)
        with self.assertRaises(SnapshotApprovalError):
            psa.validate_snapshot_descriptor({'max_files': pct.MAX_SNAPSHOT_FILES})

        # Broker request_approval also rejects narrower descriptor before opening a request.
        broker, _ = create_snapshot_approval_broker(clock=FakeClock())
        with self.assertRaises(SnapshotApprovalError):
            broker.request_approval(
                _ctx(),
                proposal_digest=_digest('narrow'),
                descriptor=SnapshotScopeDescriptor(max_files=64),
            )
        self.assertEqual(broker.active_state(), 'idle')

    def test_context_including_root_identity_and_proposal_digest_validation(self):
        for bad_ctx in (
            {
                'project_id': 'proj-alpha',
                'root_identity_sha256': _root_id(),
                'generation': 1,
                'project_epoch': 1,
                'connection_epoch': 1,
            },
            _ctx(project_id=''),
            _ctx(project_id=' proj-alpha'),
            _ctx(project_id='proj\nalpha'),
            _ctx(project_id='proj\u202ealpha'),
            _ctx(project_id='a' * (psa.MAX_PROJECT_ID_LENGTH + 1)),
            _ctx(root_identity_sha256=''),
            _ctx(root_identity_sha256='/home/owner/secret-repo'),
            _ctx(root_identity_sha256='A' * 64),
            _ctx(root_identity_sha256='g' * 64),
            _ctx(root_identity_sha256='0' * 63),
            _ctx(root_identity_sha256=123),
            _ctx(generation=True),
            _ctx(generation=-1),
            _ctx(project_epoch=False),
            _ctx(connection_epoch=-5),
        ):
            with self.assertRaises(SnapshotApprovalError):
                psa.validate_context(bad_ctx)

        for bad_digest in ('', 'abc', 'Z' * 64, 'A' * 64, 123, True):
            with self.assertRaises(SnapshotApprovalError):
                psa.validate_proposal_digest(bad_digest)

    def test_scope_digest_changes_on_context_root_identity_or_proposal_change(self):
        base_ctx = _ctx()
        base_prop = _digest('proposal-1')
        _, base_scope = psa.compute_scope_digest(
            base_ctx, runner=pct.RUNNER_NAME, proposal_digest=base_prop
        )

        variants = [
            (_ctx(project_id='proj-beta'), base_prop),
            (_ctx(root_identity_sha256=_root_id('dev42:ino9999')), base_prop),
            (_ctx(generation=4), base_prop),
            (_ctx(project_epoch=8), base_prop),
            (_ctx(connection_epoch=12), base_prop),
            (base_ctx, _digest('proposal-2')),
        ]
        for ctx, prop in variants:
            _, scope = psa.compute_scope_digest(
                ctx, runner=pct.RUNNER_NAME, proposal_digest=prop
            )
            self.assertNotEqual(scope, base_scope)

    def test_tokens_and_digests_contain_no_source_paths_or_secrets_and_prompt_has_honest_notice(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx(root_identity_sha256=_root_id('dev7:ino1234'))
        prop_digest = _digest('secret-path:/home/owner/repo/secrets.py:API_KEY=sk-test')
        prompt = broker.request_approval(ctx, proposal_digest=prop_digest)

        self.assertRegex(prompt.request_id, r'^snapreq-[0-9a-f]{48}$')
        self.assertRegex(prompt.root_identity_sha256, r'^[0-9a-f]{64}$')
        self.assertEqual(prompt.allowed_decisions, ('allow_once', 'deny', 'cancel'))
        self.assertNotIn('always', prompt.allowed_decisions)
        self.assertIn('.py', prompt.notice)
        self.assertIn('secrets', prompt.notice.lower())
        self.assertIn('local-only', prompt.notice.lower())
        self.assertIn('never automatically disclosed', prompt.notice.lower())

        outcome = owner.submit_decision(
            request_id=prompt.request_id,
            scope_digest=prompt.scope_digest,
            decision='allow_once',
            context=ctx,
        )
        self.assertRegex(outcome.grant_token, r'^snapgrant-[0-9a-f]{48}$')
        self.assertNotEqual(outcome.grant_token, prompt.request_id)
        for token in (
            prompt.request_id,
            outcome.grant_token,
            prompt.scope_digest,
            prompt.descriptor_digest,
            prompt.root_identity_sha256,
        ):
            self.assertNotIn('secrets.py', token)
            self.assertNotIn('/home/owner', token)
            self.assertNotIn('API_KEY', token)
            self.assertNotIn(ctx.project_id, token)

    def test_invalid_timeout_or_ttl_rejected_at_broker_creation(self):
        for bad_timeout in (0.0, -5.0, 301.0, float('nan'), float('inf'), True):
            with self.assertRaises(SnapshotApprovalError):
                create_snapshot_approval_broker(request_timeout_seconds=bad_timeout)
        for bad_ttl in (0.0, -1.0, 31.0, float('nan'), False):
            with self.assertRaises(SnapshotApprovalError):
                create_snapshot_approval_broker(grant_ttl_seconds=bad_ttl)
        with self.assertRaises(SnapshotApprovalError):
            create_snapshot_approval_broker(clock='not-callable')


class TestSingleActiveRequestAndDecisions(unittest.TestCase):
    def test_allow_once_happy_path_and_exact_one_shot_consumption(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('p1')

        self.assertEqual(broker.active_state(), 'idle')
        prompt = broker.request_approval(ctx, proposal_digest=prop)
        self.assertEqual(broker.active_state(), 'pending')
        self.assertEqual(broker.pending_prompt(), prompt)

        outcome = owner.submit_decision(
            request_id=prompt.request_id,
            scope_digest=prompt.scope_digest,
            decision='allow_once',
            context=ctx,
        )
        self.assertEqual(outcome.status, 'approved_once')
        self.assertEqual(broker.active_state(), 'approved_unconsumed')
        self.assertIsNone(broker.pending_prompt())

        receipt = broker.consume_grant(
            grant_token=outcome.grant_token,
            request_id=prompt.request_id,
            scope_digest=prompt.scope_digest,
            context=ctx,
            proposal_digest=prop,
        )
        self.assertTrue(receipt.one_shot_consumed)
        self.assertFalse(receipt.grants_standalone_authority)
        self.assertEqual(receipt.request_id, prompt.request_id)
        self.assertEqual(receipt.scope_digest, prompt.scope_digest)
        self.assertEqual(receipt.root_identity_sha256, ctx.root_identity_sha256)
        self.assertEqual(broker.active_state(), 'idle')

    def test_single_active_request_enforced_while_pending_and_while_grant_unconsumed(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        p1 = _digest('p1')
        p2 = _digest('p2')

        prompt = broker.request_approval(ctx, proposal_digest=p1)
        with self.assertRaises(SnapshotApprovalError):
            broker.request_approval(ctx, proposal_digest=p2)

        outcome = owner.submit_decision(
            request_id=prompt.request_id,
            scope_digest=prompt.scope_digest,
            decision='allow_once',
            context=ctx,
        )
        with self.assertRaises(SnapshotApprovalError):
            broker.request_approval(ctx, proposal_digest=p2)

        broker.consume_grant(
            grant_token=outcome.grant_token,
            request_id=prompt.request_id,
            scope_digest=prompt.scope_digest,
            context=ctx,
            proposal_digest=p1,
        )
        prompt2 = broker.request_approval(ctx, proposal_digest=p2)
        self.assertNotEqual(prompt2.request_id, prompt.request_id)

    def test_deny_and_cancel_clear_pending_request_without_minting_grant(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()

        for decision, expected_status in (('deny', 'denied'), ('cancel', 'canceled')):
            prompt = broker.request_approval(ctx, proposal_digest=_digest(decision))
            outcome = owner.submit_decision(
                request_id=prompt.request_id,
                scope_digest=prompt.scope_digest,
                decision=decision,
                context=ctx,
            )
            self.assertEqual(outcome.status, expected_status)
            self.assertIsNone(outcome.grant_token)
            self.assertEqual(broker.active_state(), 'idle')

    def test_rejects_always_raw_booleans_and_arbitrary_json_dicts_as_authority(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prompt = broker.request_approval(ctx, proposal_digest=_digest('p1'))

        for bad_decision in ('always', 'allow_always', 'allow', 'approve', True, False, 1, {'approved': True}):
            with self.assertRaises(SnapshotApprovalError):
                owner.submit_decision(
                    request_id=prompt.request_id,
                    scope_digest=prompt.scope_digest,
                    decision=bad_decision,
                    context=ctx,
                )
        self.assertFalse(hasattr(broker, 'approve'))
        self.assertFalse(hasattr(broker, 'submit_decision'))

    def test_exact_request_id_and_scope_digest_required_for_decision(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prompt = broker.request_approval(ctx, proposal_digest=_digest('p1'))

        with self.assertRaises(SnapshotApprovalError):
            owner.submit_decision(
                request_id='snapreq-' + ('0' * 48),
                scope_digest=prompt.scope_digest,
                decision='allow_once',
                context=ctx,
            )
        self.assertEqual(broker.active_state(), 'pending')

        with self.assertRaises(SnapshotApprovalError):
            owner.submit_decision(
                request_id=prompt.request_id,
                scope_digest='0' * 64,
                decision='allow_once',
                context=ctx,
            )
        self.assertEqual(broker.active_state(), 'idle')

    def test_direct_construction_and_cross_broker_channel_refused(self):
        clock = FakeClock()
        broker_a, _owner_a = create_snapshot_approval_broker(clock=clock)
        _broker_b, owner_b = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prompt_a = broker_a.request_approval(ctx, proposal_digest=_digest('pa'))

        with self.assertRaises(SnapshotApprovalError):
            psa.TrustedOwnerDecisionChannel(_broker=broker_a, _channel_key=object())
        with self.assertRaises(SnapshotApprovalError):
            psa.ProjectSnapshotApprovalBroker()

        with self.assertRaises(SnapshotApprovalError):
            owner_b.submit_decision(
                request_id=prompt_a.request_id,
                scope_digest=prompt_a.scope_digest,
                decision='allow_once',
                context=ctx,
            )
        with self.assertRaises(SnapshotApprovalError):
            broker_a._submit_owner_decision(
                object(),
                request_id=prompt_a.request_id,
                scope_digest=prompt_a.scope_digest,
                decision='allow_once',
                context=ctx,
            )

    def test_trusted_callback_wrapper_validates_owner_decision_and_rejects_raw_bool_or_dict(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prompt = broker.request_approval(ctx, proposal_digest=_digest('cb'))

        for bad_return in (True, {'approved': True}, 'allow_once', None):
            with self.assertRaises(SnapshotApprovalError):
                owner.invoke_callback(
                    lambda _p, val=bad_return: val,
                    prompt=prompt,
                    get_live_context=lambda: ctx,
                )

        outcome = owner.invoke_callback(
            lambda p: OwnerDecision(
                request_id=p.request_id,
                scope_digest=p.scope_digest,
                decision='allow_once',
            ),
            prompt=prompt,
            get_live_context=lambda: ctx,
        )
        self.assertEqual(outcome.status, 'approved_once')
        self.assertIsNotNone(outcome.grant_token)


class TestReplayStalenessClockEntropyAndInvalidation(unittest.TestCase):
    def test_replay_of_decided_request_id_or_consumed_grant_token_refused(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('replay')
        prompt = broker.request_approval(ctx, proposal_digest=prop)

        outcome = owner.submit_decision(
            request_id=prompt.request_id,
            scope_digest=prompt.scope_digest,
            decision='allow_once',
            context=ctx,
        )
        with self.assertRaises(SnapshotApprovalError):
            owner.submit_decision(
                request_id=prompt.request_id,
                scope_digest=prompt.scope_digest,
                decision='allow_once',
                context=ctx,
            )

        broker.consume_grant(
            grant_token=outcome.grant_token,
            request_id=prompt.request_id,
            scope_digest=prompt.scope_digest,
            context=ctx,
            proposal_digest=prop,
        )
        with self.assertRaises(SnapshotApprovalError):
            broker.consume_grant(
                grant_token=outcome.grant_token,
                request_id=prompt.request_id,
                scope_digest=prompt.scope_digest,
                context=ctx,
                proposal_digest=prop,
            )

    def test_scope_or_proposal_or_descriptor_drift_at_consumption_burns_grant(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('proposal-approved')

        drift_calls = [
            lambda p, out: dict(
                grant_token=out.grant_token,
                request_id=p.request_id,
                scope_digest=p.scope_digest,
                context=ctx,
                proposal_digest=_digest('proposal-different'),
            ),
            lambda p, out: dict(
                grant_token=out.grant_token,
                request_id=p.request_id,
                scope_digest='0' * 64,
                context=ctx,
                proposal_digest=prop,
            ),
            lambda p, out: dict(
                grant_token=out.grant_token,
                request_id='snapreq-' + ('0' * 48),
                scope_digest=p.scope_digest,
                context=ctx,
                proposal_digest=prop,
            ),
            lambda p, out: dict(
                grant_token=out.grant_token,
                request_id=p.request_id,
                scope_digest=p.scope_digest,
                context=ctx,
                proposal_digest=prop,
                descriptor=SnapshotScopeDescriptor(max_files=128),
            ),
        ]
        for make_bad_kwargs in drift_calls:
            prompt = broker.request_approval(ctx, proposal_digest=prop)
            outcome = owner.submit_decision(
                request_id=prompt.request_id,
                scope_digest=prompt.scope_digest,
                decision='allow_once',
                context=ctx,
            )
            with self.assertRaises(SnapshotApprovalError):
                broker.consume_grant(**make_bad_kwargs(prompt, outcome))
            self.assertEqual(broker.active_state(), 'idle')
            # Original exact args also fail because the grant was burned.
            with self.assertRaises(SnapshotApprovalError):
                broker.consume_grant(
                    grant_token=outcome.grant_token,
                    request_id=prompt.request_id,
                    scope_digest=prompt.scope_digest,
                    context=ctx,
                    proposal_digest=prop,
                )

    def test_context_and_root_identity_drift_at_decision_or_consumption_burns_state(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        base = _ctx()
        prop = _digest('ctx-drift')

        drifted_contexts = (
            _ctx(project_id='proj-other'),
            _ctx(root_identity_sha256=_root_id('dev42:ino-replaced')),
            _ctx(generation=base.generation + 1),
            _ctx(project_epoch=base.project_epoch + 1),
            _ctx(connection_epoch=base.connection_epoch + 1),
        )

        # Drift at decision time burns pending request.
        for drifted in drifted_contexts:
            prompt = broker.request_approval(base, proposal_digest=prop)
            with self.assertRaises(SnapshotApprovalError):
                owner.submit_decision(
                    request_id=prompt.request_id,
                    scope_digest=prompt.scope_digest,
                    decision='allow_once',
                    context=drifted,
                )
            self.assertEqual(broker.active_state(), 'idle')
            # Retrying with original context also fails because request was burned.
            with self.assertRaises(SnapshotApprovalError):
                owner.submit_decision(
                    request_id=prompt.request_id,
                    scope_digest=prompt.scope_digest,
                    decision='allow_once',
                    context=base,
                )

        # Drift at consumption time burns approved grant.
        for drifted in drifted_contexts:
            prompt = broker.request_approval(base, proposal_digest=prop)
            outcome = owner.submit_decision(
                request_id=prompt.request_id,
                scope_digest=prompt.scope_digest,
                decision='allow_once',
                context=base,
            )
            with self.assertRaises(SnapshotApprovalError):
                broker.consume_grant(
                    grant_token=outcome.grant_token,
                    request_id=prompt.request_id,
                    scope_digest=prompt.scope_digest,
                    context=drifted,
                    proposal_digest=prop,
                )
            self.assertEqual(broker.active_state(), 'idle')
            with self.assertRaises(SnapshotApprovalError):
                broker.consume_grant(
                    grant_token=outcome.grant_token,
                    request_id=prompt.request_id,
                    scope_digest=prompt.scope_digest,
                    context=base,
                    proposal_digest=prop,
                )

    def test_invalidation_on_stop_project_switch_and_disconnect_clears_pending_and_grant(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('inv')

        for invalidate_name in ('invalidate_stop', 'invalidate_project_switch', 'invalidate_disconnect'):
            invalidate = getattr(broker, invalidate_name)

            p1 = broker.request_approval(ctx, proposal_digest=prop)
            invalidate()
            self.assertEqual(broker.active_state(), 'idle')
            with self.assertRaises(SnapshotApprovalError):
                owner.submit_decision(
                    request_id=p1.request_id,
                    scope_digest=p1.scope_digest,
                    decision='allow_once',
                    context=ctx,
                )

            p2 = broker.request_approval(ctx, proposal_digest=prop)
            out2 = owner.submit_decision(
                request_id=p2.request_id,
                scope_digest=p2.scope_digest,
                decision='allow_once',
                context=ctx,
            )
            invalidate()
            self.assertEqual(broker.active_state(), 'idle')
            with self.assertRaises(SnapshotApprovalError):
                broker.consume_grant(
                    grant_token=out2.grant_token,
                    request_id=p2.request_id,
                    scope_digest=p2.scope_digest,
                    context=ctx,
                    proposal_digest=prop,
                )

    def test_request_timeout_and_grant_ttl_expiry_refuse_late_decision_or_consumption(self):
        clock = FakeClock(start=50.0)
        broker, owner = create_snapshot_approval_broker(
            clock=clock, request_timeout_seconds=15.0, grant_ttl_seconds=5.0
        )
        ctx = _ctx()
        prop = _digest('timeout')

        p1 = broker.request_approval(ctx, proposal_digest=prop)
        clock.advance(15.0)
        self.assertEqual(broker.active_state(), 'idle')
        with self.assertRaises(SnapshotApprovalError):
            owner.submit_decision(
                request_id=p1.request_id,
                scope_digest=p1.scope_digest,
                decision='allow_once',
                context=ctx,
            )

        p2 = broker.request_approval(ctx, proposal_digest=prop)
        clock.advance(1.0)
        out2 = owner.submit_decision(
            request_id=p2.request_id,
            scope_digest=p2.scope_digest,
            decision='allow_once',
            context=ctx,
        )
        clock.advance(5.0)
        self.assertEqual(broker.active_state(), 'idle')
        with self.assertRaises(SnapshotApprovalError):
            broker.consume_grant(
                grant_token=out2.grant_token,
                request_id=p2.request_id,
                scope_digest=p2.scope_digest,
                context=ctx,
                proposal_digest=prop,
            )

    def test_clock_regression_after_intermediate_advance_invalidates_pending_and_grant(self):
        # Pending request: 100 -> 150 (observed) -> 140 (partial rollback, still >= created_at)
        clock = FakeClock(start=100.0)
        broker, owner = create_snapshot_approval_broker(
            clock=clock, request_timeout_seconds=60.0, grant_ttl_seconds=10.0
        )
        ctx = _ctx()
        prop = _digest('clock-partial-rollback')

        p1 = broker.request_approval(ctx, proposal_digest=prop)
        clock.value = 150.0
        self.assertEqual(broker.active_state(), 'pending')
        clock.value = 140.0
        with self.assertRaises(SnapshotApprovalError):
            owner.submit_decision(
                request_id=p1.request_id,
                scope_digest=p1.scope_digest,
                decision='allow_once',
                context=ctx,
            )
        # High-water mark stays at 150.0: 145.0 still fails closed.
        clock.value = 145.0
        with self.assertRaises(SnapshotApprovalError):
            broker.active_state()
        # Catching back up to 150.0 confirms pending request remained invalidated.
        clock.value = 150.0
        self.assertEqual(broker.active_state(), 'idle')

        # Approved grant: 200 -> 208 (observed) -> 205 (partial rollback, still >= approved_at)
        clock.value = 200.0
        p2 = broker.request_approval(ctx, proposal_digest=prop)
        out2 = owner.submit_decision(
            request_id=p2.request_id,
            scope_digest=p2.scope_digest,
            decision='allow_once',
            context=ctx,
        )
        clock.value = 208.0
        self.assertEqual(broker.active_state(), 'approved_unconsumed')
        clock.value = 205.0
        with self.assertRaises(SnapshotApprovalError):
            broker.consume_grant(
                grant_token=out2.grant_token,
                request_id=p2.request_id,
                scope_digest=p2.scope_digest,
                context=ctx,
                proposal_digest=prop,
            )
        clock.value = 209.0
        self.assertEqual(broker.active_state(), 'idle')
        with self.assertRaises(SnapshotApprovalError):
            broker.consume_grant(
                grant_token=out2.grant_token,
                request_id=p2.request_id,
                scope_digest=p2.scope_digest,
                context=ctx,
                proposal_digest=prop,
            )

    def test_non_finite_or_bool_clock_fails_closed_and_invalidates(self):
        clock = FakeClock(start=100.0)
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('bad-clock')

        for bad_val in (float('nan'), float('inf'), True):
            p = broker.request_approval(ctx, proposal_digest=prop)
            clock.value = bad_val
            with self.assertRaises(SnapshotApprovalError):
                owner.submit_decision(
                    request_id=p.request_id,
                    scope_digest=p.scope_digest,
                    decision='allow_once',
                    context=ctx,
                )
            clock.value = 101.0
            self.assertEqual(broker.active_state(), 'idle')

    def test_forced_repeated_entropy_retries_then_fails_closed_for_request_and_grant_ids(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('entropy')

        req_a = 'a' * 48
        grant_b = 'b' * 48
        req_c = 'c' * 48
        grant_d = 'd' * 48
        req_e = 'e' * 48

        # Seed finalized history with req_a and grant_b.
        with mock.patch.object(psa.secrets, 'token_hex', side_effect=[req_a, grant_b]):
            p1 = broker.request_approval(ctx, proposal_digest=prop)
            out1 = owner.submit_decision(
                request_id=p1.request_id,
                scope_digest=p1.scope_digest,
                decision='allow_once',
                context=ctx,
            )
        broker.consume_grant(
            grant_token=out1.grant_token,
            request_id=p1.request_id,
            scope_digest=p1.scope_digest,
            context=ctx,
            proposal_digest=prop,
        )

        # Transient collisions retry within MAX_MINT_ATTEMPTS and pick fresh IDs.
        with mock.patch.object(
            psa.secrets, 'token_hex', side_effect=[req_a, req_a, req_c, grant_b, grant_b, grant_d]
        ):
            p2 = broker.request_approval(ctx, proposal_digest=prop)
            self.assertEqual(p2.request_id, 'snapreq-' + req_c)
            out2 = owner.submit_decision(
                request_id=p2.request_id,
                scope_digest=p2.scope_digest,
                decision='allow_once',
                context=ctx,
            )
            self.assertEqual(out2.grant_token, 'snapgrant-' + grant_d)
        broker.consume_grant(
            grant_token=out2.grant_token,
            request_id=p2.request_id,
            scope_digest=p2.scope_digest,
            context=ctx,
            proposal_digest=prop,
        )

        # Repeated request_id collisions fail closed after 8 attempts.
        with mock.patch.object(psa.secrets, 'token_hex', return_value=req_a) as mocked:
            with self.assertRaises(SnapshotApprovalError):
                broker.request_approval(ctx, proposal_digest=prop)
            self.assertEqual(mocked.call_count, psa.MAX_MINT_ATTEMPTS)
        self.assertEqual(broker.active_state(), 'idle')

        # Repeated grant_token collisions fail closed after 8 attempts and clear pending request.
        with mock.patch.object(psa.secrets, 'token_hex', return_value=req_e):
            p3 = broker.request_approval(ctx, proposal_digest=prop)
        with mock.patch.object(psa.secrets, 'token_hex', return_value=grant_b) as mocked:
            with self.assertRaises(SnapshotApprovalError):
                owner.submit_decision(
                    request_id=p3.request_id,
                    scope_digest=p3.scope_digest,
                    decision='allow_once',
                    context=ctx,
                )
            self.assertEqual(mocked.call_count, psa.MAX_MINT_ATTEMPTS)
        self.assertEqual(broker.active_state(), 'idle')
        with self.assertRaises(SnapshotApprovalError):
            broker.consume_grant(
                grant_token='snapgrant-' + grant_b,
                request_id=p3.request_id,
                scope_digest=p3.scope_digest,
                context=ctx,
                proposal_digest=prop,
            )

    def test_stale_ids_evicted_beyond_512_history_still_never_match(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('evict')

        first_prompt = broker.request_approval(ctx, proposal_digest=prop)
        first_out = owner.submit_decision(
            request_id=first_prompt.request_id,
            scope_digest=first_prompt.scope_digest,
            decision='allow_once',
            context=ctx,
        )
        broker.consume_grant(
            grant_token=first_out.grant_token,
            request_id=first_prompt.request_id,
            scope_digest=first_prompt.scope_digest,
            context=ctx,
            proposal_digest=prop,
        )

        # Exceed MAX_FINALIZED_HISTORY so the earliest IDs are evicted from diagnostic history.
        for _ in range(psa.MAX_FINALIZED_HISTORY + 5):
            p = broker.request_approval(ctx, proposal_digest=prop)
            out = owner.submit_decision(
                request_id=p.request_id,
                scope_digest=p.scope_digest,
                decision='allow_once',
                context=ctx,
            )
            broker.consume_grant(
                grant_token=out.grant_token,
                request_id=p.request_id,
                scope_digest=p.scope_digest,
                context=ctx,
                proposal_digest=prop,
            )

        self.assertEqual(len(broker._finalized_requests), psa.MAX_FINALIZED_HISTORY)
        self.assertEqual(len(broker._finalized_grants), psa.MAX_FINALIZED_HISTORY)
        self.assertNotIn(first_prompt.request_id, broker._finalized_requests)
        self.assertNotIn(first_out.grant_token, broker._finalized_grants)

        # Evicted stale IDs still fail closed against idle and against a new active request.
        with self.assertRaises(SnapshotApprovalError):
            owner.submit_decision(
                request_id=first_prompt.request_id,
                scope_digest=first_prompt.scope_digest,
                decision='allow_once',
                context=ctx,
            )
        with self.assertRaises(SnapshotApprovalError):
            broker.consume_grant(
                grant_token=first_out.grant_token,
                request_id=first_prompt.request_id,
                scope_digest=first_prompt.scope_digest,
                context=ctx,
                proposal_digest=prop,
            )

    def test_restart_fresh_broker_invalidates_prior_requests_and_grants_with_no_persistence(self):
        clock = FakeClock()
        broker1, owner1 = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('restart')

        prompt = broker1.request_approval(ctx, proposal_digest=prop)
        outcome = owner1.submit_decision(
            request_id=prompt.request_id,
            scope_digest=prompt.scope_digest,
            decision='allow_once',
            context=ctx,
        )

        broker2, owner2 = create_snapshot_approval_broker(clock=clock)
        with self.assertRaises(SnapshotApprovalError):
            owner2.submit_decision(
                request_id=prompt.request_id,
                scope_digest=prompt.scope_digest,
                decision='allow_once',
                context=ctx,
            )
        with self.assertRaises(SnapshotApprovalError):
            broker2.consume_grant(
                grant_token=outcome.grant_token,
                request_id=prompt.request_id,
                scope_digest=prompt.scope_digest,
                context=ctx,
                proposal_digest=prop,
            )


class TestConcurrencyAndNoSideEffects(unittest.TestCase):
    def test_concurrent_consume_allows_at_most_one_winner(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('race-consume')

        prompt = broker.request_approval(ctx, proposal_digest=prop)
        outcome = owner.submit_decision(
            request_id=prompt.request_id,
            scope_digest=prompt.scope_digest,
            decision='allow_once',
            context=ctx,
        )

        barrier = threading.Barrier(8)
        winners = []
        errors = []
        lock = threading.Lock()

        def worker():
            barrier.wait()
            try:
                receipt = broker.consume_grant(
                    grant_token=outcome.grant_token,
                    request_id=prompt.request_id,
                    scope_digest=prompt.scope_digest,
                    context=ctx,
                    proposal_digest=prop,
                )
                with lock:
                    winners.append(receipt)
            except SnapshotApprovalError as err:
                with lock:
                    errors.append(err)

        threads = [threading.Thread(target=worker) for _ in range(8)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=5)

        self.assertEqual(len(winners), 1)
        self.assertEqual(len(errors), 7)
        self.assertEqual(broker.active_state(), 'idle')

    def test_concurrent_decide_and_disconnect_never_yields_consumable_grant_after_disconnect(self):
        clock = FakeClock()
        broker, owner = create_snapshot_approval_broker(clock=clock)
        ctx = _ctx()
        prop = _digest('race-disconnect')

        for _ in range(25):
            prompt = broker.request_approval(ctx, proposal_digest=prop)
            barrier = threading.Barrier(2)
            minted = []

            def approve_thread():
                barrier.wait()
                try:
                    out = owner.submit_decision(
                        request_id=prompt.request_id,
                        scope_digest=prompt.scope_digest,
                        decision='allow_once',
                        context=ctx,
                    )
                    minted.append(out.grant_token)
                except SnapshotApprovalError:
                    pass

            def disconnect_thread():
                barrier.wait()
                broker.invalidate_disconnect()

            t1 = threading.Thread(target=approve_thread)
            t2 = threading.Thread(target=disconnect_thread)
            t1.start()
            t2.start()
            t1.join(timeout=5)
            t2.join(timeout=5)

            broker.invalidate_disconnect()
            self.assertEqual(broker.active_state(), 'idle')
            if minted:
                with self.assertRaises(SnapshotApprovalError):
                    broker.consume_grant(
                        grant_token=minted[0],
                        request_id=prompt.request_id,
                        scope_digest=prompt.scope_digest,
                        context=ctx,
                        proposal_digest=prop,
                    )

    def test_module_performs_no_snapshot_reads_execution_or_cli_authority(self):
        self.assertEqual(psa.AUTHORITY_MODEL, 'DELEGATED_TO_FUTURE_DAEMON_OWNER_CHANNEL')
        self.assertFalse(psa.STANDALONE_EXECUTION_ENABLED)

        proc = subprocess.run(
            [sys.executable, '-I', str(WORKERS_DIR / 'project_snapshot_approvals.py')],
            capture_output=True,
            timeout=5,
        )
        self.assertEqual(proc.returncode, 1)
        payload = json.loads(proc.stdout.decode('utf-8'))
        self.assertEqual(payload['status'], 'library_only_not_wired')
        self.assertFalse(payload['authorized'])
        self.assertFalse(payload['snapshot_read'])
        self.assertFalse(payload['executed_project_code'])


if __name__ == '__main__':
    unittest.main()