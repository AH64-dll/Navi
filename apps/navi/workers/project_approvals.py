"""One-shot owner-decision broker for the opt-in local project route.

Only a trusted owner transport may call decide(). Tool/page data cannot approve.
An approval allows one exact bounded read/list, not a future path or operation.
"""
import os
import math
from pathlib import Path
import secrets
import threading
import time
from project_read import ProjectReader, ProjectReadError, _parts, TEXT_EXTENSIONS


class ProjectApprovals:
    def __init__(self, root, scope, *, clock=time.monotonic, timeout=30, max_calls=16):
        if not isinstance(scope, str) or not scope or len(scope) > 256:
            raise ProjectReadError('Invalid owner project scope')
        if type(timeout) not in (int, float) or not math.isfinite(timeout) or not 0 < timeout <= 60:
            raise ProjectReadError('Invalid approval deadline')
        if type(max_calls) is not int or not 1 <= max_calls <= 16:
            raise ProjectReadError('Invalid project call budget')
        self.reader = ProjectReader(root)
        self.root = Path(root)
        info = os.fstat(self.reader.fd)
        self.identity = (info.st_dev, info.st_ino)
        self.scope = scope
        self.clock = clock
        self.timeout = timeout
        self.remaining = max_calls
        self.pending = None
        self.closed = False
        self.lock = threading.Lock()

    def _same_root(self):
        # Re-open component by component: path replacement invalidates approval,
        # even though the original reader itself cannot follow the replacement.
        try:
            with ProjectReader(self.root) as reader:
                info = os.fstat(reader.fd)
                return (info.st_dev, info.st_ino) == self.identity
        except (OSError, ProjectReadError):
            return False

    def request(self, operation, args, scope):
        with self.lock:
            if self.closed or scope != self.scope or not self._same_root():
                raise ProjectReadError('Project scope is no longer current')
            if self.pending and self.clock() >= self.pending['deadline']:
                self.pending = None
            if self.pending:
                raise ProjectReadError('One project approval is already pending')
            if self.remaining <= 0:
                raise ProjectReadError('Project call budget exhausted')
            self.remaining -= 1
            if type(args) is not dict or operation not in ('read', 'list'):
                raise ProjectReadError('Invalid project operation')
            allowed = {'path'} if operation == 'list' else {'path', 'start_line', 'max_lines'}
            if set(args) - allowed or 'path' not in args:
                raise ProjectReadError('Invalid project arguments')
            path = args['path']
            parts = _parts(path, allow_root=operation == 'list')
            parameters = {'path': path}
            if operation == 'read':
                if Path(parts[-1]).suffix.casefold() not in TEXT_EXTENSIONS:
                    raise ProjectReadError('Unsupported project text extension')
                start, count = args.get('start_line', 1), args.get('max_lines', 200)
                if type(start) is not int or not 1 <= start <= 10000 or type(count) is not int or not 1 <= count <= 200:
                    raise ProjectReadError('Invalid project line range')
                parameters.update(start_line=start, max_lines=count)
            token = 'project-perm-' + secrets.token_hex(16)
            self.pending = {'id': token, 'operation': operation, 'args': parameters,
                            'deadline': self.clock() + self.timeout}
            # A copy prevents caller mutation changing the eventual approved path.
            return {'type': 'permission', 'id': token, 'scope': self.scope,
                    'tool': 'project_' + operation, 'input': dict(parameters),
                    'choices': ['allow', 'deny']}

    def decide(self, token, decision, scope):
        with self.lock:
            pending = self.pending
            if self.closed or scope != self.scope or not pending or token != pending['id']:
                raise ProjectReadError('Project approval is absent or stale')
            if decision not in ('allow', 'deny'):
                raise ProjectReadError('Only allow once or deny is supported')
            # Consume first. Duplicate decisions, read failures and changed roots
            # never restore this approval or automatically retry the operation.
            self.pending = None
            if self.clock() >= pending['deadline'] or not self._same_root():
                raise ProjectReadError('Project approval expired or root changed')
            if decision == 'deny':
                return {'denied': True}
            if pending['operation'] == 'read':
                return self.reader.read(**pending['args'])
            return self.reader.list(**pending['args'])

    def close(self):
        with self.lock:
            if not self.closed:
                self.closed = True
                self.pending = None
                self.reader.close()
