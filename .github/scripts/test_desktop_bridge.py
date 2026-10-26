"""Изолированные контракты адаптера; не являются живым подтверждением Anki."""
import copy
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

PAYLOAD = Path(__file__).resolve().parents[2] / 'tools/anki-repo/src/desktop/payload.py'


class BridgeContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.root = self.base / 'addons21/2055492159/user_files/toolkit'
        self.root.mkdir(parents=True)
        (self.base / 'addons21/1788670778').mkdir()
        self.secret = 'a' * 64
        (self.root / 'settings.json').write_text(json.dumps({'token': self.secret}))
        manifest = {'bootstrap': '# current-bootstrap', 'previous_bootstrap': '# previous-bootstrap', 'anki_versions': ['26.9.2'], 'addons': {i: {'files': {}} for i in ('2055492159', '1788670778')}}
        (self.root / 'compatibility.json').write_text(json.dumps(manifest))
        self.meta = {i: SimpleNamespace(enabled=True, compatible=lambda: True) for i in manifest['addons']}
        manager = SimpleNamespace(addonsFolder=lambda i='': str(self.base / 'addons21' / i), addon_meta=lambda i: self.meta[i])
        self.mw = SimpleNamespace(pm=SimpleNamespace(base=str(self.base), name='Synthetic'), addonManager=manager, col=object(), state='deckBrowser')
        self.scheduled = []
        self.modules = {'aqt': SimpleNamespace(mw=self.mw, appVersion='26.09.2', gui_hooks=SimpleNamespace(profile_will_close=[])),
                        'aqt.qt': SimpleNamespace(QTimer=SimpleNamespace(singleShot=lambda delay, call: self.scheduled.append(call)),
                                                  QApplication=SimpleNamespace(activeModalWidget=lambda: None)),
                        '2055492159': SimpleNamespace(), '1788670778': SimpleNamespace(), '1788670778.main': SimpleNamespace()}
        class Completed:
            def __init__(self, value):
                self.value = value
            def map(self, callback):
                return Completed(callback(self.value))
        class ApiError(Exception):
            def __init__(self, kind, message):
                self.kind = kind
                super().__init__(message)
        def reply(identifier, error):
            return {'jsonrpc': '2.0', 'id': identifier,
                    'error': {'code': {'busy': 1010, 'invalidRequest': -32600}[error.kind],
                              'message': str(error), 'data': {'type': error.kind}}}
        self.modules['2055492159.protocol'] = SimpleNamespace(
            completed=Completed, settle=lambda items: Completed([item.value for item in items]),
            ApiError=ApiError, Dispatcher=SimpleNamespace(reply=reply))
        self.modules['2055492159.contract'] = SimpleNamespace(SCHEMA={'x-limits': {'maxBatchItems': 100}})
        class Connect:
            def __init__(self):
                self.server = SimpleNamespace(rpc_handler=lambda request, *args, **kw: ('rpc', request, args, kw))
            def handler_async(self, request, *args, **kwargs):
                return ('upstream', request)
        self.upstream = {'AnkiConnect': Connect, 'util': SimpleNamespace(setting=lambda k: {'apiKey': 'upstream-secret', 'webBindAddress': '127.0.0.1', 'apiVersion': 6}[k]),
                         'format_success_reply': lambda v, result: {'result': result, 'error': None},
                         'format_exception_reply': lambda v, error: {'result': None, 'error': str(error)}, 'completed': lambda value: value}
        self.upstream['ac'] = SimpleNamespace(runtime=SimpleNamespace(gui=SimpleNamespace(guard_lifecycle=lambda: None)))
        self.g = {'upstream': self.upstream, '__file__': str(self.root / 'payload.py')}
        self.mock_modules = patch.dict('sys.modules', self.modules)
        self.mock_modules.start()
        self.addCleanup(self.mock_modules.stop)
        exec(compile(PAYLOAD.read_text(), str(PAYLOAD), 'exec'), self.g)
        self.client = Connect()

    def call(self, action, **params):
        default = {'token': self.secret, 'installation': str(self.base), 'profile': 'Synthetic'}
        default.update(params)
        return self.client.handler_async({'action': action, 'version': 6, 'key': 'upstream-secret', 'params': default})

    def ticket(self, kind='export'):
        identifier = 'b' * 64
        path = self.base / 'toolkit-desktop/spool' / identifier
        path.mkdir(parents=True)
        (path / 'request.json').write_text(json.dumps({'approved': True, 'expires': 99999999999, 'profile': 'Synthetic', 'action': kind, 'deck': 'Synthetic'}))
        return identifier, path

    def test_upstream_is_forwarded_unchanged(self):
        request = {'action': 'version'}
        self.assertEqual(self.client.handler_async(request), ('upstream', request))

    def test_handshake_requires_both_authorizations_and_profile(self):
        result = self.call('toolkitStatus', nonce='challenge')['result']
        self.assertEqual(result['nonce'], 'challenge')
        self.assertEqual(result['upstream_api_version'], 6)
        self.assertEqual(result['patch_version'], '1.0.2')
        for params, reason in [({'token': 'bad'}, 'unauthorized'), ({'profile': 'Other'}, 'wrong_profile'), ({'installation': '/'}, 'wrong_installation')]:
            self.assertIn(reason, self.call('toolkitStatus', **params)['error'])
        req = {'action': 'toolkitStatus', 'params': {'token': self.secret}, 'key': 'bad'}
        self.assertIn('upstream apiKey', self.client.handler_async(req)['error'])

    def test_installed_enabled_loaded_compatible_are_distinct(self):
        self.assertTrue(self.call('toolkitStatus')['result']['addons_ready'])
        self.meta['1788670778'].enabled = False
        self.assertEqual(self.call('toolkitStatus')['result']['addons'][1]['state'], 'disabled')
        self.meta['1788670778'].enabled = True
        with patch.dict('sys.modules', {'1788670778.main': None}):
            # Отсутствие модуля диагностируется независимо от файлов.
            del __import__('sys').modules['1788670778.main']
            self.assertEqual(self.call('toolkitStatus')['result']['addons'][1]['state'], 'load_error')
        self.meta['1788670778'].compatible = lambda: False
        self.assertEqual(self.call('toolkitStatus')['result']['addons'][1]['state'], 'incompatible')
        self.meta['1788670778'].compatible = lambda: True
        self.assertTrue(self.call('toolkitStatus')['result']['addons_ready'])
        (self.base / 'addons21/1788670778/unknown.py').write_text('unknown = 1')
        result = self.call('toolkitStatus')['result']
        self.assertFalse(result['addons'][1]['supported'])
        self.assertEqual(result['addons'][1]['state'], 'incompatible')
        self.assertFalse(result['addons_ready'])

    def test_bootstrap_hash_check_accepts_only_one_exact_known_block(self):
        folder = self.base / 'bootstrap-check'
        folder.mkdir()
        target = folder / '__init__.py'
        plain = b'upstream = True\n'
        import hashlib
        expected = {'__init__.py': hashlib.sha256(plain).hexdigest()}
        for block in ('bootstrap', 'previous_bootstrap'):
            target.write_bytes(self.g['MANIFEST'][block].encode() + plain)
            self.assertTrue(self.g['verify_tree'](folder, expected))
        target.write_bytes(b'# current-bootstrap# previous-bootstrap' + plain)
        with self.assertRaisesRegex(ValueError, 'unknown_bootstrap'):
            self.g['verify_tree'](folder, expected)

    def model(self):
        return {
            'crowdanki_uuid': 'model-guid', 'name': 'Synthetic',
            'flds': [{'ord': 0, 'name': 'Front', 'font': 'Existing', 'sticky': False}],
            'tmpls': [{'ord': 0, 'name': 'Card 1', 'qfmt': 'Existing', 'afmt': 'Back'}],
        }

    def test_import_preflight_matches_full_crowdanki_json_comparison(self):
        existing = self.model()
        self.mw.col = SimpleNamespace(models=SimpleNamespace(all=lambda: [existing]))
        # Порядок ключей и свойства вне flds/tmpls не вызывают преобразования upstream.
        incoming = copy.deepcopy(existing)
        incoming['flds'][0] = dict(reversed(list(incoming['flds'][0].items())))
        incoming.update(css='Updated', mod=123)
        deck = {'note_models': [incoming], 'children': [], 'media_files': []}
        self.g['import_preflight'](self.mw, deck, self.base)
        for key, field, value in [('tmpls', 'qfmt', 'Updated'), ('tmpls', 'afmt', 'Updated'),
                                  ('flds', 'font', 'Updated'), ('flds', 'sticky', True),
                                  ('flds', 'name', 'Renamed')]:
            with self.subTest(key=key, field=field):
                changed = copy.deepcopy(existing)
                changed[key][0][field] = value
                deck['note_models'] = [changed]
                with self.assertRaisesRegex(ValueError, 'model_conversion_requires_manual_import'):
                    self.g['import_preflight'](self.mw, deck, self.base)
        changed.pop('tmpls')
        with self.assertRaisesRegex(ValueError, 'invalid_model_schema: tmpls'):
            self.g['import_preflight'](self.mw, deck, self.base)

    def test_model_conversion_is_rejected_before_loading_importer(self):
        identifier, path = self.ticket('import')
        existing = self.model()
        incoming = copy.deepcopy(existing)
        incoming['tmpls'][0]['qfmt'] = 'Updated'
        self.mw.col = SimpleNamespace(models=SimpleNamespace(all=lambda: [existing]))
        (path / 'source').mkdir()
        (path / 'source/deck.json').write_text(json.dumps({'note_models': [incoming]}))
        self.assertEqual(self.call('toolkitImport', id=identifier)['result']['state'], 'queued')
        with patch('importlib.import_module') as importer:
            self.scheduled[0]()
            importer.assert_not_called()
        value = self.call('toolkitJob', id=identifier)['result']
        self.assertEqual(value['error'], 'model_conversion_requires_manual_import')
        self.assertFalse(value['partial_changes_possible'])

    def test_import_preflight_failure_reports_no_possible_collection_changes(self):
        identifier, path = self.ticket('import')
        source = path / 'source'
        source.mkdir()
        (source / 'deck.json').write_text(json.dumps({
            'note_models': [{
                'crowdanki_uuid': 'model-guid',
                'name': 'Synthetic',
                'flds': [{'ord': 0, 'name': 'Front'}],
            }],
            'children': [],
            'media_files': [],
        }))
        self.mw.col = SimpleNamespace(models=SimpleNamespace(all=lambda: []))

        queued = self.call('toolkitImport', id=identifier)
        self.assertEqual(queued['result']['state'], 'queued')
        self.scheduled[0]()

        result = self.call('toolkitJob', id=identifier)['result']
        self.assertEqual(result['state'], 'failed')
        self.assertEqual(result['error'], 'invalid_model_schema: tmpls')
        self.assertFalse(result['partial_changes_possible'])

    def test_submission_returns_before_scheduled_work_and_is_idempotent(self):
        identifier, path = self.ticket()
        self.assertEqual(path.parent, self.base / 'toolkit-desktop/spool')
        self.assertFalse((self.root / 'spool').exists())
        first = self.call('toolkitExport', id=identifier)
        self.assertEqual(first['result']['state'], 'queued')
        self.assertEqual(len(self.scheduled), 1)
        self.assertEqual(self.call('toolkitExport', id=identifier), first)
        self.assertEqual(len(self.scheduled), 1)
        self.assertEqual(json.loads((path / 'result.json').read_text())['state'], 'queued')

    def test_legacy_job_remains_readable_until_upgrade_cleanup(self):
        identifier = 'c' * 64
        path = self.root / 'spool' / identifier
        path.mkdir(parents=True)
        (path / 'request.json').write_text(json.dumps({'profile': 'Synthetic'}))

        self.assertEqual(self.g['job_path'](identifier, self.mw), path)

    def test_cancelled_import_is_not_success(self):
        identifier, path = self.ticket('import')
        source = path / 'source'
        source.mkdir()
        (source / 'deck.json').write_text(json.dumps({'children': [], 'note_models': [], 'media_files': []}))
        self.mw.col = SimpleNamespace(models=SimpleNamespace(all=lambda: []))
        self.mw.reset = lambda: None
        imported = SimpleNamespace(AnkiJsonImporter=lambda col: SimpleNamespace(load_deck=lambda directory: False))
        self.assertEqual(self.call('toolkitImport', id=identifier)['result']['state'], 'queued')
        with patch('importlib.import_module', return_value=imported):
            self.scheduled[0]()
        self.assertEqual(self.call('toolkitJob', id=identifier)['result']['state'], 'cancelled')
        self.assertEqual(self.g['EVIDENCE'], {})
        self.assertFalse(self.g['BUSY'])

    def test_restart_refuses_user_process_and_active_study(self):
        self.assertIn('user_process_not_owned', self.call('toolkitShutdown', owner='x')['error'])
        with patch.dict('os.environ', {'ANKI_TOOLKIT_OWNER': 'x'}):
            self.mw.state = 'review'
            self.assertIn('busy_or_studying', self.call('toolkitShutdown', owner='x')['error'])
        self.assertEqual(self.scheduled, [])

    def test_paths_cannot_be_supplied_over_http(self):
        for identifier in ('../x', '/tmp', 'C:\\file', 'a' * 63):
            self.assertIn('invalid_job_id', self.call('toolkitImport', id=identifier)['error'])
        for name in ('../secret', 'C:secret', 'a\\b', '/secret'):
            with self.assertRaisesRegex(ValueError, 'unsafe_media'):
                self.g['media_name'](name)
        identifier, path = self.ticket()
        (path / 'request.json').unlink()
        (path / 'request.json').symlink_to(self.base / 'outside')
        self.assertIn('unsafe_path', self.call('toolkitExport', id=identifier)['error'])

    def test_restart_marks_unfinished_persisted_job_interrupted(self):
        identifier, path = self.ticket()
        (path / 'result.json').write_text(json.dumps({'state': 'running', 'process': 'previous'}))
        self.assertEqual(self.call('toolkitJob', id=identifier)['result']['state'], 'interrupted')

    def test_sensitive_operations_block_upstream_writes_and_clear_evidence_on_unload(self):
        self.g['BUSY'] = True
        reply = self.client.handler_async({'action': 'deleteNotes', 'version': 6})
        self.assertIn('anki_busy', reply['error'])
        self.assertEqual(self.client.handler_async({'action': 'version'}), ('upstream', {'action': 'version'}))
        self.g['EVIDENCE']['Synthetic'] = {'export': True}
        self.modules['aqt'].gui_hooks.profile_will_close[0]()
        self.assertEqual(self.g['EVIDENCE'], {})

    def test_rpc_busy_batch_preserves_ids_notifications_and_invalid_requests(self):
        rpc = self.client.server.rpc_handler
        self.g['BUSY'] = True
        request = lambda **kw: {'jsonrpc': '2.0', 'method': 'notes.delete', **kw}
        batch = [request(id='first'), request(), request(id=None), request(id=0), 42, [request(id='nested')]]
        responses = rpc(batch).value
        self.assertEqual([r['id'] for r in responses], ['first', None, 0, None, None])
        self.assertEqual([r['error']['code'] for r in responses], [1010, 1010, 1010, -32600, -32600])
        self.assertEqual(rpc(request(id=7)).value['id'], 7)
        self.assertIsNone(rpc(request()).value)
        self.assertIsNone(rpc([request(), request()]).value)
        for bad in ([], [request(id=1)] * 101, None, {}, request(id=True), request(id=[])):
            self.assertEqual(rpc(bad).value['error']['code'], -32600)
        self.g['BUSY'] = False
        self.assertEqual(rpc(batch, {'origin': 'test'}), ('rpc', batch, ({'origin': 'test'},), {}))

    def test_final_result_write_failure_is_controlled_and_clears_busy(self):
        for operation_state in ('succeeded', 'cancelled', 'failed'):
            with self.subTest(operation_state=operation_state), tempfile.TemporaryDirectory() as directory:
                path = Path(directory)
                (path / 'source').mkdir()
                (path / 'source/deck.json').write_text('{}')
                self.mw.col = SimpleNamespace(models=SimpleNamespace(all=lambda: []))
                self.mw.reset = lambda: None
                def load_deck(directory):
                    if operation_state == 'failed':
                        raise ValueError('import_error')
                    return operation_state == 'succeeded'
                imported = SimpleNamespace(AnkiJsonImporter=lambda col: SimpleNamespace(load_deck=load_deck))
                original_save = self.g['save_result']
                def fail_final(path, value):
                    if value['state'] != 'running':
                        raise OSError('disk_unavailable')
                    original_save(path, value)
                self.g['BUSY'] = True
                self.g['save_result'] = fail_final
                try:
                    with patch('sys.stderr') as stderr, patch('importlib.import_module', return_value=imported):
                        self.g['execute_job']('job', self.mw, 'import', path, {'profile': 'Synthetic'})
                    self.assertTrue(stderr.write.called)
                finally:
                    self.g['save_result'] = original_save
                value = self.g['JOBS']['job']
                self.assertFalse(self.g['BUSY'])
                self.assertEqual(value['state'], 'interrupted')
                self.assertEqual(value['operation_state'], operation_state)
                self.assertFalse(value['result_persisted'])
                self.assertTrue(value['partial_changes_possible'])
                self.assertEqual(json.loads((path / 'result.json').read_text())['state'], 'running')
                self.assertEqual(self.g['EVIDENCE'], {})
                # Перезапуск читает старый running консервативно; операция не запускается повторно.

    def test_editor_conflict_refuses_admission_before_any_write(self):
        identifier, path = self.ticket()
        def reject_editor():
            raise ValueError('editorConflict')
        self.upstream['ac'].runtime.gui.guard_lifecycle = reject_editor
        self.assertIn('editorConflict', self.call('toolkitExport', id=identifier)['error'])
        self.assertFalse((path / 'result.json').exists())
        self.assertEqual(self.scheduled, [])


if __name__ == '__main__':
    unittest.main()
