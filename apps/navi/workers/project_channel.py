"""In-process owner channel, not a network/RPC endpoint.

The daemon must retain this object. Model adapters receive request() only, never
owner_decide(), cancel() or the broker. No transport integration exists yet.
"""
import json
import threading
from project_approvals import ProjectApprovals, ProjectReadError

MAX_REQUEST_BYTES = 4096
MAX_RESULT_BYTES = 262144


def decode_request(frame):
    """Decode one bounded JSONL model request, never an owner decision."""
    if type(frame) is not bytes or not frame.endswith(b'\n') or len(frame) > MAX_REQUEST_BYTES:
        raise ProjectReadError('Invalid project request frame')
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ProjectReadError('Duplicate project request key')
            result[key] = value
        return result
    try:
        value = json.loads(frame, object_pairs_hook=pairs,
                           parse_constant=lambda unused: (_ for _ in ()).throw(ValueError()))
    except (ValueError, UnicodeError, RecursionError) as error:
        raise ProjectReadError('Invalid project request JSON') from error
    if type(value) is not dict or set(value) != {'operation', 'args'}:
        raise ProjectReadError('Project requests cannot contain owner decisions or roots')
    return value


class ProjectChannel:
    """One owner-selected turn. cancel() gates disclosure of in-flight reads.

    emit is a trusted nonblocking local queue writer, not a callback supplied by
    the model. Cancellation and result enqueue are linearized under one lock.
    Once an item is enqueued, cancellation cannot retract it; the downstream
    adapter must also discard queued items for canceled turns before disclosure.
    """
    def __init__(self, root, scope, emit):
        if not callable(emit):
            raise ProjectReadError('A trusted owner event queue is required')
        self._broker = ProjectApprovals(root, scope)
        self._scope = scope
        self._emit = emit
        self._lock = threading.Lock()
        self._closed = False
        self._busy = False
        self._waiting = None

    def request(self, frame):
        value = decode_request(frame)
        with self._lock:
            if self._closed or self._busy or self._waiting:
                raise ProjectReadError('Project turn is closed or busy')
            permission = self._broker.request(value['operation'], value['args'], self._scope)
            self._waiting = permission['id']
            try:
                self._emit(permission)
            except BaseException:
                self._closed = True
                self._waiting = None
                self._broker.close()
                raise

    def owner_decide(self, token, decision):
        # Called only by the authenticated owner path, never by decoded frames.
        with self._lock:
            if self._closed or self._busy or token != self._waiting:
                raise ProjectReadError('Project decision is stale')
            if decision not in ('allow', 'deny'):
                raise ProjectReadError('Only allow once or deny is supported')
            self._busy = True
            self._waiting = None
        try:
            try:
                result = self._broker.decide(token, decision, self._scope)
                encoded = json.dumps(result, ensure_ascii=True).encode('ascii')
                if len(encoded) > MAX_RESULT_BYTES:
                    raise ProjectReadError('Project result exceeded transport limit')
                event = {'type': 'project_result', 'id': token, 'scope': self._scope, 'result': result}
            except (OSError, ValueError, UnicodeError):
                # Never leak absolute paths or file contents via exception text.
                event = {'type': 'project_error', 'id': token, 'scope': self._scope,
                         'error': 'Project operation failed'}
            with self._lock:
                if not self._closed:
                    try:
                        self._emit(event)
                    except BaseException:
                        self._closed = True
                        raise
        finally:
            with self._lock:
                self._busy = False
                closed = self._closed
            if closed:
                self._broker.close()

    def cancel(self):
        with self._lock:
            self._closed = True
            self._waiting = None
            busy = self._busy
        # A bounded read may finish, but its result cannot pass the closed gate.
        if not busy:
            self._broker.close()

    close = cancel
