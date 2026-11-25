"""Bounded read-only project seam for the opt-in approved local project route.

A caller must obtain owner approval for the exact path/operation before calling.
No shell, writes, network, auto-discovery, or model-selected roots.
"""
import os
import hashlib
from pathlib import Path
import stat

class ProjectReadError(ValueError):
    pass

# Deliberately narrow initial policy. Explicit attachment workflows handle other data.
TEXT_EXTENSIONS = frozenset({'.py', '.rs', '.ts', '.tsx', '.js', '.jsx', '.mjs', '.cjs',
    '.json', '.md', '.txt', '.toml', '.yaml', '.yml', '.css', '.html', '.sh',
    '.c', '.h', '.cpp', '.hpp', '.go', '.java', '.sql'})
DENIED_NAMES = frozenset({'credentials', 'secrets', 'id_rsa', 'id_ed25519',
    'private-key', 'private_key', 'token', 'tokens', 'password', 'passwords'})
MAX_FILE_BYTES = 65536
MAX_READ_BYTES = 16384
MAX_ENTRIES = 200


def _parts(value, allow_root=False):
    if not isinstance(value, str) or len(value.encode('utf-8')) > 1024:
        raise ProjectReadError('Invalid relative project path')
    if allow_root and value == '':
        return []
    parts = value.split('/')
    if any(not p or p in {'.', '..'} or p.startswith('.') or '\\' in p
           or any(ord(c) < 32 or ord(c) == 127 for c in p)
           or p.casefold().split('.')[0] in DENIED_NAMES for p in parts):
        raise ProjectReadError('Hidden, sensitive or non-relative project path refused')
    return parts


class ProjectReader:
    """One root descriptor per explicit project, pinned until close.

    Every descendant is opened relative to pinned directory descriptors with
    O_NOFOLLOW. A symlink or concurrent directory replacement cannot escape it.
    An already-open regular file is checked before and after bounded reading.
    """
    def __init__(self, root):
        root = Path(root)
        if not root.is_absolute():
            raise ProjectReadError('Project root must be absolute without symlinks')
        fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            for part in root.parts[1:]:
                if part in {'.', '..'}:
                    raise ProjectReadError('Project root must not contain traversal')
                next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
                os.close(fd)
                fd = next_fd
        except BaseException:
            os.close(fd)
            raise
        self.fd = fd
        self.closed = False

    def close(self):
        if not self.closed:
            self.closed = True
            os.close(self.fd)

    def __enter__(self):
        return self

    def __exit__(self, *unused):
        self.close()

    def _directory(self, parts):
        if self.closed:
            raise ProjectReadError('Project reader is closed')
        fd = os.dup(self.fd)
        try:
            for part in parts:
                next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
                os.close(fd)
                fd = next_fd
            return fd
        except BaseException:
            os.close(fd)
            raise

    def list(self, path=''):
        parts = _parts(path, allow_root=True)
        fd = self._directory(parts)
        try:
            entries = []
            truncated = False
            # Iterate, never materialize an unbounded directory before applying limits.
            with os.scandir(fd) as scan:
                for visited, entry in enumerate(scan):
                    if visited >= 512:
                        truncated = True
                        break
                    try:
                        _parts(entry.name)
                    except ProjectReadError:
                        continue
                    if len(entries) == MAX_ENTRIES:
                        truncated = True
                        break
                    info = entry.stat(follow_symlinks=False)
                    if stat.S_ISDIR(info.st_mode):
                        kind = 'directory'
                    elif stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                        kind = 'file'
                    else:
                        continue
                    entries.append({'name': entry.name, 'kind': kind})
            entries.sort(key=lambda e: e['name'])
            return {'source':'untrusted-project-data', 'path':path,
                    'entries':entries, 'truncated':truncated}
        finally:
            os.close(fd)

    def read(self, path, start_line=1, max_lines=200):
        parts = _parts(path)
        if Path(parts[-1]).suffix.casefold() not in TEXT_EXTENSIONS:
            raise ProjectReadError('Only approved text/code extensions are supported')
        if type(start_line) is not int or not 1 <= start_line <= 10000 or type(max_lines) is not int or not 1 <= max_lines <= 200:
            raise ProjectReadError('Invalid line bounds')
        directory = self._directory(parts[:-1])
        try:
            fd = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=directory)
        finally:
            os.close(directory)
        try:
            before = os.fstat(fd)
            if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size > MAX_FILE_BYTES:
                raise ProjectReadError('Read requires a single-link regular file of at most 64 KiB')
            data = bytearray()
            while len(data) <= MAX_FILE_BYTES:
                chunk = os.read(fd, min(8192, MAX_FILE_BYTES + 1 - len(data)))
                if not chunk:
                    break
                data.extend(chunk)
            after = os.fstat(fd)
            if len(data) > MAX_FILE_BYTES or (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (after.st_size, after.st_mtime_ns, after.st_ctime_ns):
                raise ProjectReadError('Project file changed or exceeded the read limit')
            try:
                text = data.decode('utf-8')
            except UnicodeError as error:
                raise ProjectReadError('Project file is not UTF-8 text') from error
            if any((ord(c) < 32 and c not in '\t\r\n') or ord(c) == 127 for c in text):
                raise ProjectReadError('Project file contains binary controls')
            lines = text.splitlines(keepends=True)
            selected = ''.join(lines[start_line - 1:start_line - 1 + max_lines])
            raw = selected.encode('utf-8')
            clipped = len(raw) > MAX_READ_BYTES
            if clipped:
                selected = raw[:MAX_READ_BYTES].decode('utf-8', errors='ignore')
            truncated = clipped or len(lines) >= start_line + max_lines
            result = {'source':'untrusted-project-data', 'path':path, 'start_line':start_line,
                      'text':selected, 'truncated':truncated}
            # Only a complete approved snapshot can become an edit basis. Never
            # hash selected lines and imply they describe the full file.
            if start_line == 1 and not truncated and selected.encode('utf-8') == data:
                result['snapshot_sha256'] = hashlib.sha256(data).hexdigest()
                result['snapshot_bytes'] = len(data)
            return result
        finally:
            os.close(fd)
