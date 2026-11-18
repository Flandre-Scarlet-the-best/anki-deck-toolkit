set pagination off
set confirm off
set verbose off
set print thread-events off
set disable-randomization off
set startup-with-shell off
set detach-on-fork on
set debuginfod enabled off
python
import gdb
import hashlib
import json
import os
import pathlib

store = pathlib.Path(os.environ['ANKI_E2E_CRASH_STORE']).resolve()
mode = os.environ['ANKI_E2E_CRASH_WINDOW']
expected = os.environ['ANKI_E2E_CRASH_SHA256']
if mode not in ('blob_sync', 'checkpoint_sync', 'outcome_sync'):
    raise gdb.GdbError('неизвестное окно аварии %r: ожидается blob_sync, checkpoint_sync или outcome_sync' % mode)
batch = store / '.runtime' / 'batches' / 'window'
state_path = batch / 'state.json'
entering = {}
evidence = None

def read_json(path):
    return json.loads(path.read_bytes())

def imported(item):
    return (item.get('outcome') or {}).get('code') == 'acquired'

def on_fsync():
    global evidence
    if evidence is not None:
        return
    thread = gdb.selected_thread().global_num
    entry = not entering.get(thread, False)
    entering[thread] = entry
    if not entry:
        return
    pid = gdb.selected_inferior().pid
    fd = int(gdb.selected_frame().read_register('rdi'))
    try:
        path = pathlib.Path(os.readlink('/proc/%d/fd/%d' % (pid, fd)))
        if not path.is_relative_to(store):
            return
        previous_bytes = state_path.read_bytes()
        state = json.loads(previous_bytes)
        if len(state['items']) != 2 or not imported(state['items'][0]):
            return
        second = state['items'][1]
        saved = second.get('acquisition') is not None
        target = False
        temporary_sha = None
        if mode == 'blob_sync':
            if path.parent == batch / 'candidates' and path.name.startswith('.tmp-') and not saved:
                temporary_sha = hashlib.sha256(path.read_bytes()).hexdigest()
                target = temporary_sha == expected
        elif mode == 'checkpoint_sync':
            if path.parent == batch and path.name.startswith('.tmp-') and not saved:
                candidate = read_json(path)['items'][1]
                target = candidate.get('acquisition') is not None and candidate.get('outcome') is None
        elif mode == 'outcome_sync':
            if path.parent == batch and path.name.startswith('.tmp-') and saved and second.get('outcome') is None:
                candidate = read_json(path)['items'][1]
                original = candidate.get('acquisition') or {}
                blob = original.get('blob') or {}
                target = imported(candidate) and blob.get('sha256') == expected and hashlib.sha256((batch / blob['storage_path']).read_bytes()).hexdigest() == expected
        if target:
            syscall = {74: 'fsync', 75: 'fdatasync'}.get(int(gdb.selected_frame().read_register('orig_rax')), 'unknown')
            evidence = {'window': mode, 'syscall': syscall, 'edge': 'entry',
                'inferior_pid': pid,
                'state_sha256': hashlib.sha256(previous_bytes).hexdigest(),
                'first_imported': True, 'second_checkpointed': saved,
                'temporary_sha256': temporary_sha,
                'fd_relative': str(path.relative_to(store))}
            gdb.set_convenience_variable('crashpoint', gdb.Value(1))
    except (OSError, ValueError, KeyError, IndexError):
        # До первого checkpoint state.json ещё нет. При ошибке
        # наблюдения окно не считается достигнутым: доказательство останется null.
        return
end
set $crashpoint = 0
catch syscall fsync fdatasync
commands
silent
python on_fsync()
if $crashpoint
kill
else
continue
end
end
python
driver_error = None
try:
    gdb.execute('run')
except gdb.error as error:
    driver_error = str(error)
try:
    signalled = int(gdb.parse_and_eval('$_exitsignal'))
except (gdb.error, TypeError):
    signalled = None
print('ANKI_E2E_CRASH=' + json.dumps({'point': evidence, 'exit_signal': signalled, 'driver_error': driver_error}, sort_keys=True))
end
