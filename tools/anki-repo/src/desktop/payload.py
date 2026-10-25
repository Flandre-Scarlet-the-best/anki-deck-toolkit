"""Версионированный адаптер, загружаемый проверенным bootstrap AnkiConnect.

HTTP admission принадлежит upstream с дополнительным локальным capability token.
"""
import hashlib
import hmac
import importlib
import json
import os
from pathlib import Path
import re
import sys
import time
import uuid

VERSION = "1.0.1"
CONNECT = "2055492159"
CROWD = "1788670778"
ROOT = Path(__file__).parent
MANIFEST = json.loads((ROOT / "compatibility.json").read_text("utf-8"))
SETTINGS = json.loads((ROOT / "settings.json").read_text("utf-8"))
PROCESS = uuid.uuid4().hex
JOBS = {}
EVIDENCE = {}
BUSY = False


def safe_path(path):
    # Отклоняем символические ссылки POSIX и junction/reparse points Windows.
    path = Path(path)
    for p in (path, *path.parents):
        if p.is_symlink() or (hasattr(p, "is_junction") and p.is_junction()):
            raise ValueError("unsafe_path: символическая ссылка или junction")
    return path


def media_name(name):
    if (not isinstance(name, str) or not name or name in (".", "..")
            or any(c in name for c in '/\\:\x00')
            or any(ord(c) < 32 for c in name)):
        raise ValueError("unsafe_media_name")
    return name


def media_names(deck):
    names = set()
    for name in deck.get("media_files", []):
        names.add(media_name(name))
    for child in deck.get("children", []):
        names.update(media_names(child))
    return names


def verify_tree(folder, expected):
    actual = {}
    def walk(p):
        safe_path(p)
        for child in p.iterdir():
            safe_path(child)
            if child.is_dir():
                if child.name not in ("user_files", "__pycache__"):
                    walk(child)
            elif child.suffix == ".py":
                data = child.read_bytes()
                if child.parent == folder and child.name == "__init__.py":
                    # Убираем единственный точный собственный блок перед проверкой хэша.
                    blocks = [MANIFEST[key].encode('utf-8') for key in ('bootstrap', 'previous_bootstrap')]
                    if sum(data.count(block) for block in blocks) > 1:
                        raise ValueError('unknown_bootstrap: повтор собственного блока')
                    for block in blocks:
                        data = data.replace(block, b'', 1)
                actual[child.relative_to(folder).as_posix()] = hashlib.sha256(data).hexdigest()
    walk(folder)
    return actual == expected


def context(request, params):
    import aqt
    if request.get("key") != upstream["util"].setting("apiKey"):
        raise ValueError("unauthorized: upstream apiKey")
    token = params.get("token")
    if not isinstance(token, str) or not hmac.compare_digest(token, SETTINGS["token"]):
        raise ValueError("unauthorized: toolkit capability")
    if upstream["util"].setting("webBindAddress") not in ("127.0.0.1", "::1"):
        raise ValueError("unsafe_bind_address")
    base = safe_path(Path(aqt.mw.pm.base)).resolve()
    requested = params.get("installation")
    if not isinstance(requested, str) or os.path.normcase(str(base)) != os.path.normcase(str(Path(requested).resolve())):
        raise ValueError("wrong_installation")
    if params.get("profile") is not None and params["profile"] != aqt.mw.pm.name:
        raise ValueError("wrong_profile")
    return aqt.mw


def health(mw):
    import aqt
    result = []
    for addon in (CONNECT, CROWD):
        folder = safe_path(Path(mw.addonManager.addonsFolder(addon)))
        meta = mw.addonManager.addon_meta(addon)
        loaded = addon in sys.modules and (addon != CROWD or addon + ".main" in sys.modules)
        supported = folder.is_dir() and verify_tree(folder, MANIFEST["addons"][addon]["files"])
        result.append(dict(id=addon, enabled=meta.enabled, compatible=meta.compatible(),
                           loaded=loaded, supported=supported,
                           state=("missing" if not folder.is_dir() else "disabled" if not meta.enabled else
                                  "incompatible" if not meta.compatible() or not supported else
                                  "load_error" if not loaded else "loaded")))
    version = tuple(int(x) for x in aqt.appVersion.split('.'))
    supported = any(version == tuple(int(x) for x in s.split('.')) for s in MANIFEST["anki_versions"])
    return result, all(x["state"] == "loaded" for x in result) and supported


def status(mw, params):
    import aqt
    addons, ready = health(mw)
    return dict(protocol=1, patch_version=VERSION, upstream_api_version=upstream["util"].setting("apiVersion"),
                anki_version=aqt.appVersion, profile=mw.pm.name, data_dir=str(Path(mw.pm.base).resolve()),
                addons_dir=mw.addonManager.addonsFolder(), process=PROCESS,
                owner=os.environ.get("ANKI_TOOLKIT_OWNER"), nonce=params.get("nonce"),
                capabilities=["toolkitStatus", "toolkitExport", "toolkitImport", "toolkitJob", "toolkitShutdown"],
                addons=addons, addons_ready=ready,
                functional_evidence=EVIDENCE.get(mw.pm.name, {}), busy=BUSY)


def job_path(identifier, mw):
    if not isinstance(identifier, str) or not re.fullmatch("[0-9a-f]{64}", identifier):
        raise ValueError("invalid_job_id")
    roots = (Path(mw.pm.base).resolve() / "toolkit-desktop" / "spool", ROOT / "spool")
    for root in roots:
        spool = safe_path(root)
        path = safe_path(spool / identifier)
        if path.parent == spool and path.is_dir():
            return path
    raise ValueError("unknown_job")


def read_ticket(identifier, mw):
    path = job_path(identifier, mw)
    ticket = json.loads(safe_path(path / "request.json").read_text("utf-8"))
    if ticket.get("profile") != mw.pm.name or ticket.get("approved") is not True:
        raise ValueError("unapproved_or_wrong_profile")
    if time.time() > ticket.get("expires", 0):
        raise ValueError("expired_ticket")
    return path, ticket


def save_result(path, value):
    safe_path(path / "result.json")
    temp = path / (".result-" + uuid.uuid4().hex)
    try:
        with temp.open("x", encoding="utf-8") as f:
            json.dump(value, f, ensure_ascii=False)
            f.flush()
            os.fsync(f.fileno())
        os.replace(temp, path / "result.json")
    finally:
        temp.unlink(missing_ok=True)


def model_shape(model):
    if not isinstance(model, dict):
        raise ValueError("invalid_model_schema: model")
    shape = {}
    for key in ("flds", "tmpls"):
        items = model.get(key)
        if not isinstance(items, list):
            raise ValueError("invalid_model_schema: " + key)
        for item in items:
            if not isinstance(item, dict):
                raise ValueError("invalid_model_schema: " + key)
            name = item.get("name")
            ordinal = item.get("ord")
            if not isinstance(name, str) or not name or type(ordinal) is not int or ordinal < 0:
                raise ValueError("invalid_model_schema: " + key)
        # CrowdAnki utils.json_compare сравнивает полные записи с сортировкой ключей.
        shape[key] = json.dumps(items, sort_keys=True)
    return shape


def import_preflight(mw, deck, path):
    # Проверяем до CrowdAnki: отмена его дополнительного ChangeModelDialog ненадёжна.
    models = mw.col.models.all()
    seen = set()
    def check(node):
        for model in node.get("note_models", []):
            identity = model.get("crowdanki_uuid")
            if not identity or identity in seen:
                raise ValueError("ambiguous_model_identity")
            seen.add(identity)
            incoming_shape = model_shape(model)
            matches = [m for m in models if m.get("crowdanki_uuid") == identity]
            names = [m for m in models if m["name"] == model["name"]]
            if len(matches) > 1 or len(names) > 1 or (not matches and names):
                raise ValueError("model_conflict_requires_manual_import")
            if matches and model_shape(matches[0]) != incoming_shape:
                raise ValueError("model_conversion_requires_manual_import")
        for child in node.get("children", []):
            check(child)
    check(deck)
    for name in media_names(deck):
        if not safe_path(path / "media" / name).is_file():
            raise ValueError("missing_media: " + name)
    media = path / "media"
    if media.exists():
        for file in safe_path(media).iterdir():
            media_name(file.name)
            if not safe_path(file).is_file():
                raise ValueError("unsafe_media_entry")


def execute_job(identifier, mw, action, path, ticket):
    global BUSY
    value = dict(id=identifier, state="running", process=PROCESS, profile=ticket["profile"], action=action)
    JOBS[identifier] = value
    partial_changes_possible = False
    try:
        save_result(path, value)
        if mw.pm.name != ticket["profile"] or not mw.col or mw.state not in ("deckBrowser", "overview"):
            raise ValueError("profile_changed")
        if not health(mw)[1]:
            raise ValueError("addons_not_ready")
        if action == "export":
            module = importlib.import_module(CROWD + ".export.anki_exporter")
            adapters = importlib.import_module(CROWD + ".anki.adapters.anki_deck")
            config = importlib.import_module(CROWD + ".config.config_settings")
            class CheckedExporter(module.AnkiJsonExporter):
                def _copy_media(self, deck, directory):
                    for name in deck.get_media_file_list():
                        media_name(name)
                        safe_path(Path(mw.col.media.dir()) / name)
                    super()._copy_media(deck, directory)
            deck = mw.col.decks.by_name(ticket["deck"])
            if deck is None:
                raise ValueError("deck_not_found")
            output = safe_path(path / "export")
            output.mkdir()  # Существующий экспорт или произвольный каталог не перезаписывается.
            partial_changes_possible = True
            mw.create_backup_now()
            exporter = CheckedExporter(mw.col, config.ConfigSettings.get_instance())
            exporter.export_to_directory(adapters.AnkiDeck(deck), output, copy_media=True, create_deck_subdirectory=False)
            payload = json.loads((output / "deck.json").read_text("utf-8"))
            for name in media_names(payload):
                if not safe_path(output / "media" / name).is_file():
                    raise ValueError("export_media_copy_failed: " + name)
            value.update(state="succeeded", export_dir=str(output), notes=exporter.last_exported_count)
        else:
            source = safe_path(path / "source")
            payload = json.loads(safe_path(source / "deck.json").read_text("utf-8"))
            import_preflight(mw, payload, source)
            importer = importlib.import_module(CROWD + ".importer.anki_importer")
            # Оригинальный загрузчик с диалогом конфигурации и backup.
            # Выполняется в потоке GUI после завершения HTTP admission.
            partial_changes_possible = True
            ok = importer.AnkiJsonImporter(mw.col).load_deck(source)
            value["state"] = "succeeded" if ok else "cancelled"
        mw.reset()
    except Exception as exc:
        value.update(state="failed", error=str(exc), partial_changes_possible=partial_changes_possible)
    finally:
        JOBS[identifier] = value
        try:
            save_result(path, value)
        except Exception as exc:
            # Запись могла не состояться: не повторяем операцию и не обещаем сохранённый успех.
            value.update(state="interrupted", operation_state=value["state"],
                         error="result_persistence_failed: " + str(exc), result_persisted=False,
                         partial_changes_possible=partial_changes_possible)
            print("Anki Deck Toolkit: не удалось сохранить итог задания " + identifier + ": " + str(exc), file=sys.stderr)
        else:
            if value["state"] == "succeeded":
                EVIDENCE.setdefault(mw.pm.name, {})[action] = True
        finally:
            BUSY = False


def dispatch(request):
    global BUSY
    from aqt.qt import QTimer, QApplication
    action = request["action"]
    params = request.get("params", {})
    mw = context(request, params)
    if action == "toolkitStatus":
        return status(mw, params)
    if action == "toolkitJob":
        identifier = params.get("id")
        path = job_path(identifier, mw)
        ticket = json.loads((path / "request.json").read_text("utf-8"))
        if ticket.get("profile") != mw.pm.name:
            raise ValueError("wrong_profile")
        if identifier in JOBS:
            return JOBS[identifier]
        persisted = path / "result.json"
        if persisted.exists():
            v = json.loads(safe_path(persisted).read_text("utf-8"))
            if v.get("state") in ("running", "queued"):
                v["state"] = "interrupted"
            return v
        return dict(id=identifier, state="prepared")
    if action == "toolkitShutdown":
        owner = os.environ.get("ANKI_TOOLKIT_OWNER")
        if not owner or not hmac.compare_digest(str(params.get("owner", "")), owner):
            raise ValueError("user_process_not_owned")
        if BUSY or mw.state not in ("deckBrowser", "overview") or QApplication.activeModalWidget():
            raise ValueError("restart_required: busy_or_studying")
        upstream["ac"].runtime.gui.guard_lifecycle()
        # Штатный API закрытия AnkiConnect без kill и переключения профиля.
        QTimer.singleShot(200, lambda: upstream["ac"].guiExitAnki())
        return dict(state="closing")
    if action not in ("toolkitExport", "toolkitImport"):
        raise ValueError("unsupported_toolkit_action")
    identifier = params.get("id")
    path, ticket = read_ticket(identifier, mw)
    kind = "export" if action == "toolkitExport" else "import"
    if ticket.get("action") != kind:
        raise ValueError("ticket_action_mismatch")
    if identifier in JOBS:
        return JOBS[identifier]
    if (path / "result.json").exists():
        raise ValueError("consumed_ticket: query toolkitJob")
    if BUSY or mw.state not in ("deckBrowser", "overview") or QApplication.activeModalWidget() or getattr(mw, "_background_op_count", 0):
        raise ValueError("anki_busy")
    upstream["ac"].runtime.gui.guard_lifecycle()
    if not health(mw)[1]:
        raise ValueError("addons_not_ready")
    value = dict(id=identifier, state="queued", process=PROCESS, profile=mw.pm.name, action=kind)
    save_result(path, value)
    JOBS[identifier] = value
    BUSY = True
    QTimer.singleShot(200, lambda: execute_job(identifier, mw, kind, path, ticket))
    return value


def install():
    cls = upstream["AnkiConnect"]
    original = cls.handler_async
    original_init = cls.__init__
    def initialize(self, *args, **kwargs):
        original_init(self, *args, **kwargs)
        rpc = getattr(getattr(self, "server", None), "rpc_handler", None)
        if rpc is not None:
            protocol = importlib.import_module(CONNECT + ".protocol")
            schema = importlib.import_module(CONNECT + ".contract").SCHEMA
            def reject_busy(request):
                def invalid():
                    return protocol.completed(protocol.Dispatcher.reply(None, error=protocol.ApiError(
                        "invalidRequest", "Некорректный запрос JSON-RPC")))
                if isinstance(request, list):
                    if not request or len(request) > schema["x-limits"]["maxBatchItems"]:
                        return invalid()
                    return protocol.settle([reject_busy(item) if isinstance(item, dict) else invalid()
                                            for item in request]).map(
                        lambda replies: [reply for reply in replies if reply is not None] or None)
                if (not isinstance(request, dict) or request.get("jsonrpc") != "2.0"
                        or not isinstance(request.get("method"), str) or not request["method"]
                        or set(request) - {"jsonrpc", "id", "method", "params"}
                        or (request.get("id") is not None and type(request["id"]) not in (str, int))):
                    return invalid()
                return protocol.completed(None if "id" not in request else protocol.Dispatcher.reply(
                    request["id"], error=protocol.ApiError("busy", "Операция CrowdAnki уже выполняется")))
            def guarded_rpc(request, *a, **kw):
                if BUSY:
                    return reject_busy(request)
                return rpc(request, *a, **kw)
            self.server.rpc_handler = guarded_rpc
    def handler(self, request, *args, **kwargs):
        if not isinstance(request, dict) or not str(request.get("action", "")).startswith("toolkit"):
            if BUSY and isinstance(request, dict) and request.get("action") not in ("version", "getActiveProfile", "getProfiles", "apiReflect"):
                return upstream["completed"](upstream["format_exception_reply"](request.get("version", 6), ValueError("anki_busy: операция CrowdAnki уже выполняется")))
            return original(self, request, *args, **kwargs)
        try:
            result = dispatch(request)
            reply = upstream["format_success_reply"](request.get("version", 6), result)
        except Exception as exc:
            reply = upstream["format_exception_reply"](request.get("version", 6), exc)
        return upstream["completed"](reply)
    cls.handler_async = handler
    cls.__init__ = initialize
    from aqt import gui_hooks
    gui_hooks.profile_will_close.append(EVIDENCE.clear)


install()
