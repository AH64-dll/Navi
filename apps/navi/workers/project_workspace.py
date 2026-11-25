"""Materialize an exact preview as private inert review files, never a project edit.

The caller supplies an owner-private storage directory, not a model-selected
root. No project path is opened. No tool/daemon route imports this helper yet.
"""
import json
import os
from pathlib import Path
import secrets
import hashlib
import re
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
import stat
from project_edit_reviews import proposal_digest
from project_read import ProjectReadError


def _storage(root):
    root = Path(root)
    if not root.is_absolute() or '..' in root.parts:
        raise ProjectReadError('Private owner storage must be absolute')
    fd = os.open('/', os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for component in root.parts[1:]:
            next_fd = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
            os.close(fd)
            fd = next_fd
            info = os.fstat(fd)
            # Trusted root-owned system ancestors may be readable, never writable.
            if info.st_mode & 0o022 or info.st_uid not in (0, os.getuid()):
                raise ProjectReadError('Review storage crosses an unsafe ancestor')
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) & 0o077:
            raise ProjectReadError('Review storage must be owner-only')
        return fd
    except BaseException:
        os.close(fd)
        raise


def create_review_workspace(storage, proposal, *, cancel=None, request_id=None):
    """Create unique inert .txt snapshots. Never touches proposal.path on disk.

    A return means private review files were created, not applied or approved.
    Cancellation before return cleans this new directory. After return, callers
    must retain/report the result rather than retry blindly on lost delivery.
    """
    digest = proposal_digest(proposal)
    def current():
        if cancel is not None and cancel.is_set():
            raise ProjectReadError('Review workspace canceled')
    current()
    parent = _storage(storage)
    directory = None
    if request_id is not None:
        if type(request_id) is not str or re.fullmatch(r'[0-9a-f]{32}', request_id) is None:
            os.close(parent)
            raise ProjectReadError('An owner-generated request identifier is required')
        name = 'review-' + hashlib.sha256(('review-request-v1:'+request_id).encode()).hexdigest()[:32]
    else:
        name = 'review-' + secrets.token_hex(16)
    written = []
    created = False
    try:
        current()
        try:
            os.mkdir(name, 0o700, dir_fd=parent)
        except FileExistsError:
            if request_id is None:
                raise ProjectReadError('Review identifier collision; no retry performed')
            # Concurrent/partial creation is a blocked state, never an excuse
            # to overwrite or allocate another workspace for the same request.
            recovered = inspect_review_workspace(storage, name)
            if recovered['proposal_digest'] != digest:
                raise ProjectReadError('Review request is already bound to another proposal')
            current()
            return {'created': False, 'recovered': True, 'applied': False,
                    'review_id': name, 'proposal_digest': digest,
                    'files': ['before.txt','proposed-after.txt','review.json'],
                    'notice': 'Existing private review recovered. No project file changed.'}
        created = True
        directory = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
        manifest = {'schema': 'navi-private-review-v1', 'applied': False,
                    'requires_fresh_validation': True, 'original_path': proposal.path,
                    'proposal_digest': digest, 'before_sha256': proposal.before_sha256,
                    'after_sha256': proposal.after_sha256,
                    'notice': 'Review files only. No project file was changed. Do not execute these snapshots.'}
        files = [('before.txt', proposal.before.encode()), ('proposed-after.txt', proposal.after.encode()),
                 ('review.json', (json.dumps(manifest, ensure_ascii=True, indent=2)+'\n').encode())]
        for filename, data in files:
            current()
            fd = os.open(filename, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=directory)
            written.append(filename)
            try:
                offset = 0
                while offset < len(data):
                    current()
                    count = os.write(fd, data[offset:])
                    if count <= 0:
                        raise ProjectReadError('Review snapshot write failed')
                    offset += count
                os.fsync(fd)
            finally:
                os.close(fd)
        current()
        os.fsync(directory)
        os.fsync(parent)
        current()
        return {'created': True, 'applied': False, 'review_id': name,
                'proposal_digest': digest, 'files': [item[0] for item in files],
                'notice': 'Private inert review workspace created. No project file changed.'}
    except BaseException:
        if directory is not None:
            for filename in written:
                try:os.unlink(filename, dir_fd=directory)
                except OSError:pass
        if created:
            try:os.rmdir(name, dir_fd=parent)
            except OSError:pass
        raise
    finally:
        if directory is not None:
            os.close(directory)
        os.close(parent)


def inspect_review_workspace(storage, review_id):
    """Reopen only a fixed review ID and verify its complete inert snapshot set.

    A valid result proves the review files match their own manifest. It does
    not prove the source file is still current, approved or safe to execute.
    """
    from project_edits import propose_edit
    if type(review_id) is not str or re.fullmatch(r'review-[0-9a-f]{32}', review_id) is None:
        raise ProjectReadError('Invalid private review identifier')
    parent = _storage(storage)
    folder = None
    try:
        folder = os.open(review_id, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
        info = os.fstat(folder)
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise ProjectReadError('Review directory is not private')
        expected = {'before.txt', 'proposed-after.txt', 'review.json'}
        if set(os.listdir(folder)) != expected:
            raise ProjectReadError('Review workspace is incomplete or contains extra files')
        values = {}
        for filename in expected:
            fd = os.open(filename, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=folder)
            try:
                before = os.fstat(fd)
                if not stat.S_ISREG(before.st_mode) or before.st_uid != os.getuid() or before.st_nlink != 1 or stat.S_IMODE(before.st_mode) != 0o600 or before.st_size > 16384:
                    raise ProjectReadError('Invalid review snapshot file')
                data = bytearray()
                while len(data) <= 16384:
                    chunk = os.read(fd, 16385-len(data))
                    if not chunk:
                        break
                    data.extend(chunk)
                after = os.fstat(fd)
                if len(data) > 16384 or (before.st_size,before.st_mtime_ns,before.st_ctime_ns) != (after.st_size,after.st_mtime_ns,after.st_ctime_ns):
                    raise ProjectReadError('Review snapshot changed or exceeded limits')
                values[filename] = data.decode('utf-8')
            finally:
                os.close(fd)
        def pairs(items):
            result = {}
            for key, value in items:
                if key in result:
                    raise ValueError()
                result[key] = value
            return result
        manifest = json.loads(values['review.json'], object_pairs_hook=pairs)
        keys = {'schema','applied','requires_fresh_validation','original_path','proposal_digest','before_sha256','after_sha256','notice'}
        if type(manifest) is not dict or set(manifest) != keys or manifest['schema'] != 'navi-private-review-v1' or manifest['applied'] is not False or manifest['requires_fresh_validation'] is not True:
            raise ProjectReadError('Invalid private review manifest')
        proposal = propose_edit(manifest['original_path'],manifest['before_sha256'],values['before.txt'],values['proposed-after.txt'])
        if proposal.after_sha256 != manifest['after_sha256'] or proposal_digest(proposal) != manifest['proposal_digest']:
            raise ProjectReadError('Review workspace hash mismatch')
        return {'valid': True, 'review_id': review_id, 'applied': False,
                'requires_fresh_validation': True, 'proposal_digest': manifest['proposal_digest'],
                'proposal': proposal.review()}
    except (ValueError, UnicodeError, KeyError, TypeError) as error:
        raise ProjectReadError('Invalid private review workspace') from error
    finally:
        if folder is not None:
            os.close(folder)
        os.close(parent)


def owner_review_storage(data_root):
    """Create only the fixed review directory beneath verified private owner data."""
    parent=_storage(data_root)
    try:
        try:os.mkdir('proposal-reviews',0o700,dir_fd=parent)
        except FileExistsError:pass
        fd=os.open('proposal-reviews',os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=parent)
        try:
            info=os.fstat(fd)
            if info.st_uid!=os.getuid() or stat.S_IMODE(info.st_mode)!=0o700:
                raise ProjectReadError('Review storage is not owner-private')
        finally:os.close(fd)
        return Path(data_root)/'proposal-reviews'
    finally:os.close(parent)


def workspace_request(storage, frame, *, cancel=None):
    """Fixed create/recover endpoint, storage supplied by owner process only."""
    from project_validate import decode_preview_frame
    if type(frame) is not bytes or len(frame)>230000 or not frame.endswith(b'\n'):
        raise ProjectReadError('Invalid review workspace frame')
    def pairs(items):
        result={}
        for key,value in items:
            if key in result:raise ValueError()
            result[key]=value
        return result
    try:
        request=json.loads(frame,object_pairs_hook=pairs)
        if type(request) is not dict or set(request)!={'request_id','preview'}:
            raise ValueError()
        proposal=decode_preview_frame((json.dumps(request['preview'])+'\n').encode())
    except (ValueError,TypeError,UnicodeError,RecursionError) as error:
        raise ProjectReadError('Invalid exact workspace request') from error
    return create_review_workspace(storage,proposal,cancel=cancel,request_id=request['request_id'])


def main():
    import signal
    import threading
    cancel=threading.Event()
    signal.signal(signal.SIGTERM,lambda *_:cancel.set())
    signal.signal(signal.SIGINT,lambda *_:cancel.set())
    try:
        if len(sys.argv) not in (2,3):raise ProjectReadError('Owner storage argument required')
        storage=owner_review_storage(sys.argv[2]) if len(sys.argv)==3 and sys.argv[1]=='--owner-data' else sys.argv[1] if len(sys.argv)==2 else None
        if storage is None:raise ProjectReadError('Invalid owner storage mode')
        result=workspace_request(storage,sys.stdin.buffer.readline(230001),cancel=cancel)
        print(json.dumps(result),flush=True)
        return 0
    except (ProjectReadError,OSError):
        print(json.dumps({'error':'Private review could not be created or recovered. Retry only the same request identifier.','applied':False}),flush=True)
        return 1


if __name__=='__main__':raise SystemExit(main())
