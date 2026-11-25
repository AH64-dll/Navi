#!/usr/bin/env python3
"""Private-pipe project worker for the opt-in approved local project route.

Only the owner daemon may spawn this with an owner-selected root/scope. Both
control pipe ends belong to that daemon; never attach a model to this protocol.
Model proposals belong inside the request payload, not control records.
"""
import json
import os
from pathlib import Path
import queue
import select
import sys
import threading
import time
# Isolated Python (-I) omits script directory. Only this installed worker's
# own directory may supply sibling modules, never the selected project cwd.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from project_channel import ProjectChannel, ProjectReadError, decode_request

MAX_CONTROL_BYTES = 8192


def serve(root, scope, source, output_fd):
    events = queue.Queue(maxsize=32)
    stopped = threading.Event()
    failures = []
    channel = ProjectChannel(root, scope, events.put_nowait)
    os.set_blocking(output_fd, False)

    def writer():
        try:
            while not stopped.is_set():
                try:
                    event = events.get(timeout=.05)
                except queue.Empty:
                    continue
                # Suppress queued events after cancellation, not only reads
                # that were in flight when it arrived.
                if stopped.is_set():
                    return
                data = json.dumps(event, ensure_ascii=True).encode('ascii') + b'\n'
                if len(data) > 300000:
                    raise ProjectReadError('Project event exceeds pipe limit')
                offset = 0
                deadline = time.monotonic() + 1
                while offset < len(data):
                    if stopped.is_set():
                        return
                    if time.monotonic() >= deadline:
                        raise ProjectReadError('Project owner stopped reading')
                    _, ready, _ = select.select([], [output_fd], [], .05)
                    if not ready:
                        continue
                    try:
                        offset += os.write(output_fd, data[offset:])
                    except BlockingIOError:
                        continue
        except BaseException as error:
            failures.append(type(error).__name__)
            stopped.set()
            channel.cancel()

    thread = threading.Thread(target=writer, daemon=True)
    thread.start()
    active = None
    seen = 0
    try:
        events.put_nowait({'type': 'ready', 'scope': scope})
        while not stopped.is_set():
            # A malformed/overlong frame is terminal, not an invitation to
            # allocate or resynchronize through an unbounded attacker stream.
            frame = source.readline(MAX_CONTROL_BYTES + 1)
            if not frame:
                break
            seen += 1
            if len(frame) > MAX_CONTROL_BYTES or not frame.endswith(b'\n') or seen > 64:
                raise ProjectReadError('Invalid project control frame')
            # Reuse strict duplicate/nonfinite JSON parsing in the decoder,
            # wrapping the owner record as data, not a model request.
            wrapper = b'{"operation":"control","args":' + frame.rstrip(b'\n') + b'}\n'
            if len(wrapper) > 4096:
                raise ProjectReadError('Project control record exceeded limit')
            record = decode_request(wrapper)['args']
            if type(record) is not dict:
                raise ProjectReadError('Invalid project owner record')
            kind = record.get('type')
            if kind == 'cancel' and set(record) == {'type'}:
                break
            if kind == 'request' and set(record) == {'type', 'request'}:
                channel.request(json.dumps(record['request'], ensure_ascii=True).encode('ascii') + b'\n')
            elif kind == 'decision' and set(record) == {'type', 'id', 'decision'}:
                if active and active.is_alive():
                    raise ProjectReadError('Project decision already running')
                if type(record['id']) is not str or type(record['decision']) is not str:
                    raise ProjectReadError('Invalid project owner decision')
                def decide(token=record['id'], decision=record['decision']):
                    try:
                        channel.owner_decide(token, decision)
                    except BaseException:
                        stopped.set()
                        channel.cancel()
                active = threading.Thread(target=decide, daemon=True)
                active.start()
            else:
                raise ProjectReadError('Unknown project owner record')
    finally:
        stopped.set()
        channel.cancel()
        thread.join(1.2)
        # The parent must reap/kill its process group. Do not wait on a stuck
        # filesystem operation after owner EOF or cancellation.
    return not failures


if __name__ == '__main__':
    if len(sys.argv) != 3:
        sys.exit(2)
    try:
        ok = serve(Path(sys.argv[1]), sys.argv[2], sys.stdin.buffer, sys.stdout.fileno())
    except BaseException:
        # No exception paths, root names or project bytes on stderr.
        sys.exit(1)
    sys.exit(0 if ok else 1)
