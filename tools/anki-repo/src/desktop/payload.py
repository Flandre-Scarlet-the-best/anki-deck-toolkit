"""Версионированный адаптер, загружаемый проверенным bootstrap AnkiConnect.

Принятие задания по HTTP выполняет upstream с дополнительным локальным capability token.
"""
import hashlib
import hmac
import importlib
import json
import os
from pathlib import Path
import re
import sys
import stat
import time
import uuid

VERSION = "1.2.0"
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
    if params.get("profile") is not None and aqt.mw.pm.name is None:
        raise ValueError("profile_not_ready")
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
                capabilities=["toolkitStatus", "toolkitExport", "toolkitImport", "toolkitJob", "toolkitShutdown", "toolkitCatalog", "toolkitBindProfile"],
                addons=addons, addons_ready=ready,
                functional_evidence=EVIDENCE.get(mw.pm.name, {}), busy=BUSY)


def profile_identity(mw, bind=False):
    # prefs21.db принадлежит ProfileManager: имя меняется отдельно от данных
    # профиля. UUID базы + UUID профиля не зависят от имени и WSL-пути.
    key = "anki_deck_toolkit_identity"
    base_id = mw.pm.meta.get(key)
    profile_id = mw.pm.profile.get(key)
    base_path = os.path.normcase(str(safe_path(Path(mw.pm.base)).resolve()))
    binding = mw.pm.meta.get(key + "_base")
    if binding is not None and binding != base_path:
        raise ValueError("profile_base_binding_mismatch: используйте исходную базу Anki; перенос или копия prefs21.db требует отдельной явной привязки")
    records = [(name, mw.pm._unpickle(data)) for name, data in
               mw.pm.db.all("select name, cast(data as blob) from profiles where name != '_global'")]
    registry_key, bound_key = key + "_profiles", key + "_bound"
    registry = mw.pm.meta.get(registry_key)
    def valid_uuid(value):
        return isinstance(value, str) and re.fullmatch("[0-9a-f]{32}", value)
    def corrupt():
        raise ValueError("profile_identity_corrupt: восстановите согласованные prefs21.db из резервной копии")
    clean = (key not in mw.pm.meta and key + "_base" not in mw.pm.meta
             and registry_key not in mw.pm.meta and key not in mw.pm.profile
             and bound_key not in mw.pm.profile
             and not any(key in data or bound_key in data for _, data in records))
    if clean:
        if not bind:
            return None
        base_id, profile_id = uuid.uuid4().hex, uuid.uuid4().hex
        registry = {}
    else:
        if not valid_uuid(base_id):
            corrupt()
        if binding is None:
            raise ValueError("profile_base_binding_missing: восстановите согласованные prefs21.db из резервной копии")
        if registry_key in mw.pm.meta:
            if (not isinstance(registry, dict)
                    or any(not isinstance(name, str) or not valid_uuid(value) for name, value in registry.items())
                    or len(set(registry.values())) != len(registry)):
                corrupt()
        if bound_key in mw.pm.profile and mw.pm.profile[bound_key] is not True:
            corrupt()
        if key not in mw.pm.profile:
            # Только запись, которая никогда не была привязана. Реестр имён и
            # отдельная метка переносятся/сохраняются при переименовании профиля.
            if (bound_key in mw.pm.profile or registry is None or mw.pm.name in registry
                    or any(name == mw.pm.name and key in data for name, data in records)):
                corrupt()
            if not bind:
                return None
            profile_id = uuid.uuid4().hex
        elif not valid_uuid(profile_id):
            corrupt()
        elif registry is not None:
            if (profile_id not in registry.values()
                    or (mw.pm.name in registry and registry[mw.pm.name] != profile_id)):
                corrupt()
        for name, data in records:
            if name != mw.pm.name and data.get(key) == profile_id:
                raise ValueError("profile_identity_ambiguous: дублирован UUID профиля в prefs21.db")
        if registry is None and bind:
            # Прежний формат сначала явно подтверждает существующий UUID;
            # потерянный UUID не превращается в первичную привязку.
            registry = {name: data[key] for name, data in records if key in data}
            if any(not valid_uuid(value) for value in registry.values()) or len(set(registry.values())) != len(registry):
                corrupt()
    if bind:
        registry = {name: value for name, value in registry.items() if value != profile_id}
        registry[mw.pm.name] = profile_id
        mw.pm.meta[key], mw.pm.meta[key + "_base"] = base_id, base_path
        mw.pm.meta[registry_key] = registry
        mw.pm.profile[key], mw.pm.profile[bound_key] = profile_id, True
        mw.pm.save()
    return hashlib.sha256((base_id + ":" + profile_id).encode("ascii")).hexdigest()


def ticket_profile_matches(ticket, mw):
    # Устойчивый ID всегда проверяется, даже если текстовое имя совпало.
    # Старый билет без ID сохраняет прежнюю строгую проверку имени.
    if "profile_id" in ticket:
        return bool(ticket["profile_id"]) and ticket["profile_id"] == profile_identity(mw)
    return ticket.get("profile") == mw.pm.name


def catalog(mw, bind=False):
    from aqt.qt import QApplication
    if BUSY or not mw.col or mw.state not in ("deckBrowser", "overview") or QApplication.activeModalWidget() or getattr(mw, "_background_op_count", 0):
        raise ValueError("anki_busy: завершите занятие или ответьте на диалог Anki")
    upstream["ac"].runtime.gui.guard_lifecycle()
    if not health(mw)[1]:
        raise ValueError("addons_not_ready")
    identity = profile_identity(mw, bind)
    decks = [dict(id=str(d["id"]), name=d["name"], crowdanki_uuid=d.get("crowdanki_uuid"))
             for d in mw.col.decks.all() if not d.get("dyn")]
    uuids = [d["crowdanki_uuid"] for d in decks if d["crowdanki_uuid"] is not None]
    if any(not isinstance(value, str) or not value for value in uuids) or len(set(uuids)) != len(uuids):
        raise ValueError("ambiguous_deck_uuid: устраните повторяющиеся или повреждённые CrowdAnki UUID в Anki")
    return dict(profile=mw.pm.name, profile_id=identity, process=PROCESS, decks=decks)


def export_inventory(output):
    # Свидетельство связывает полный набор байтов с живым завершением задания.
    result = {}
    files = [output / "deck.json"]
    if (output / "import_config.yaml").exists():
        files.append(output / "import_config.yaml")
    media = output / "media"
    if media.exists():
        files.extend(media.iterdir())
    for path in files:
        safe_path(path)
        metadata = path.stat()
        if not path.is_file() or metadata.st_nlink != 1:
            raise ValueError("unsafe_export_file")
        if path.parent == media:
            media_name(path.name)
        with path.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        after = path.stat()
        if (metadata.st_size, metadata.st_mtime_ns, metadata.st_ino) != (after.st_size, after.st_mtime_ns, after.st_ino):
            raise ValueError("export_changed_during_read")
        result[path.relative_to(output).as_posix()] = digest
    return result


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
    if not ticket_profile_matches(ticket, mw) or ticket.get("approved") is not True:
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
        if os.name != "nt":
            descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
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


# Поля, которые меняет повторение карточек или синхронизация без изменения
# содержимого; в отпечаток не входят, иначе обычная учёба блокировала бы импорт.
VOLATILE = ("usn", "mod", "newToday", "revToday", "lrnToday", "timeToday", "collapsed", "browserCollapsed")


def stable(value):
    return {k: v for k, v in value.items() if k not in VOLATILE}


def deck_state(mw, deck_id, scoped=None):
    # Отпечаток Anki → Anki для обнаружения изменений колоды между свежим
    # экспортом и записью CrowdAnki. Содержимое C он не описывает и не заменяет.
    col = mw.col
    ids = sorted(int(i) for i in col.decks.deck_and_child_ids(int(deck_id)))
    inside = ",".join(str(i) for i in ids)
    digest = hashlib.sha256()
    def add(*item):
        digest.update(json.dumps(item, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n")
    for did in ids:
        deck = col.decks.get(did, default=False)
        if not deck:
            raise ValueError("deck_not_found")
        add("deck", stable(deck))
        if not deck.get("dyn"):
            add("config", stable(col.decks.config_dict_for_deck_id(did)))
    for row in col.db.all("select id, nid, did, odid, ord from cards where did in (%s) or odid in (%s) order by id" % (inside, inside)):
        add("card", list(row))
    models = set()
    for row in col.db.all("select id, guid, mid, tags, flds, flags, data from notes where id in "
                          "(select nid from cards where did in (%s) or odid in (%s)) order by id" % (inside, inside)):
        add("note", list(row))
        models.add(row[2])
        if scoped is not None:
            scoped.add(row[0])
    for mid in sorted(models):
        add("model", stable(col.models.get(mid)))
    return digest.hexdigest()


def import_guard(ticket):
    # Билет со стабильным profile_id выдаёт только публикация обмена
    # (publish-output): для него охрана обязательна. Обычный `anki import`
    # произвольного каталога C не имеет и работает по прежнему контракту jobs.
    if "profile_id" not in ticket and "expected" not in ticket:
        return None
    guard = ticket.get("expected")
    if (not isinstance(guard, dict) or not isinstance(guard.get("collection_state"), str)
            or not isinstance(guard.get("deck_id"), str) or not isinstance(guard.get("deck_uuid"), str)
            or not isinstance(guard.get("media"), dict)):
        raise ValueError("freshness_guard_missing: импорт без проверенного состояния C не выполняется")
    return guard


def note_guids(deck):
    guids = {note.get("guid") for note in deck.get("notes", [])}
    for child in deck.get("children", []):
        guids.update(note_guids(child))
    guids.discard(None)
    return guids


def file_sha(path):
    with safe_path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify_unchanged(mw, guard, payload, source):
    # Fail-closed: любое расхождение с состоянием, снятым при свежем экспорте C,
    # отменяет импорт до резервной копии и записи CrowdAnki.
    deck = mw.col.decks.get(int(guard["deck_id"]), default=False)
    if not deck or deck.get("crowdanki_uuid") != guard["deck_uuid"] or payload.get("crowdanki_uuid") != guard["deck_uuid"]:
        raise ValueError("anki_changed_before_import: колода с проверенным UUID изменилась")
    scoped = set()
    if deck_state(mw, guard["deck_id"], scoped) != guard["collection_state"]:
        raise ValueError("anki_changed_before_import: колода изменилась после свежего экспорта")
    guids = sorted(note_guids(payload))
    for start in range(0, len(guids), 500):
        chunk = guids[start:start + 500]
        rows = mw.col.db.all("select id from notes where guid in (%s)" % ",".join("?" * len(chunk)), *chunk)
        if any(row[0] not in scoped for row in rows):
            raise ValueError("import_scope_conflict: GUID из B принадлежит заметке вне проверенной колоды")
    incoming = source / "media"
    if incoming.exists():
        target = Path(mw.col.media.dir())
        for file in safe_path(incoming).iterdir():
            name = media_name(file.name)
            current = safe_path(target / name)
            expected = guard["media"].get(name)
            if not current.exists():
                if expected is not None:
                    raise ValueError("anki_changed_before_import: media удалено после свежего экспорта")
                continue
            if file_sha(current) not in (expected, file_sha(file)):
                raise ValueError("anki_changed_before_import: media изменено после свежего экспорта")


def execute_job(identifier, mw, action, path, ticket):
    global BUSY
    value = dict(id=identifier, state="running", process=PROCESS, profile=ticket["profile"], action=action)
    JOBS[identifier] = value
    partial_changes_possible = False
    try:
        save_result(path, value)
        if not ticket_profile_matches(ticket, mw) or not mw.col or mw.state not in ("deckBrowser", "overview"):
            raise ValueError("profile_changed")
        if not health(mw)[1]:
            raise ValueError("addons_not_ready")
        if action == "export":
            module = importlib.import_module(CROWD + ".export.anki_exporter")
            adapters = importlib.import_module(CROWD + ".anki.adapters.anki_deck")
            config = importlib.import_module(CROWD + ".config.config_settings")
            class CheckedExporter(module.AnkiJsonExporter):
                def _copy_media(self, deck, directory):
                    originals = {}
                    for name in deck.get_media_file_list():
                        media_name(name)
                        source = safe_path(Path(mw.col.media.dir()) / name)
                        metadata = source.stat()
                        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                            raise ValueError("unsafe_source_media")
                        originals[name] = (metadata.st_ino, metadata.st_size, metadata.st_mtime_ns, metadata.st_ctime_ns)
                    super()._copy_media(deck, directory)
                    for name, before in originals.items():
                        metadata = safe_path(Path(mw.col.media.dir()) / name).stat()
                        if before != (metadata.st_ino, metadata.st_size, metadata.st_mtime_ns, metadata.st_ctime_ns) or metadata.st_nlink != 1:
                            raise ValueError("source_media_changed_during_export")
            deck = mw.col.decks.by_name(ticket["deck"])
            if deck is None:
                raise ValueError("deck_not_found")
            if ticket.get("profile_id") is not None:
                if profile_identity(mw) != ticket["profile_id"] or str(deck["id"]) != ticket["deck_id"] or PROCESS != ticket["process"]:
                    raise ValueError("export_source_changed")
                if ticket.get("deck_uuid") and deck.get("crowdanki_uuid") != ticket["deck_uuid"]:
                    raise ValueError("export_identity_changed")
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
            if ticket.get("profile_id") is not None:
                if payload["crowdanki_uuid"] != mw.col.decks.by_name(ticket["deck"])["crowdanki_uuid"]:
                    raise ValueError("export_uuid_mismatch")
                # Снимается после экспорта в том же потоке GUI: CrowdAnki при
                # экспорте сам дописывает crowdanki_uuid в колоду, модели и
                # настройки, и C описывает именно это состояние.
                value.update(profile_id=ticket["profile_id"], deck_id=ticket["deck_id"],
                             deck_name=ticket["deck"], deck_uuid=payload["crowdanki_uuid"],
                             inventory=export_inventory(output),
                             collection_state=deck_state(mw, deck["id"]))
        else:
            source = safe_path(path / "source")
            payload = json.loads(safe_path(source / "deck.json").read_text("utf-8"))
            import_preflight(mw, payload, source)
            guard = import_guard(ticket)
            if guard is not None:
                verify_unchanged(mw, guard, payload, source)
            module = importlib.import_module(CROWD + ".importer.anki_importer")
            # Оригинальный загрузчик с диалогом конфигурации и резервным копированием.
            # Выполняется в потоке GUI после завершения принятия задания по HTTP.
            importer = module.AnkiJsonImporter(mw.col)
            dialog = importer.read_import_config
            def confirmed(directory, deck):
                nonlocal partial_changes_possible
                config = dialog(directory, deck)
                if config is not None:
                    # load_deck вызывает это сразу после модального диалога и до
                    # create_backup_now/save_to_collection, без возврата в цикл событий:
                    # последний момент отказа без изменения коллекции. Повторная
                    # проверка ловит правки, сохранённые во время диалога.
                    if guard is not None:
                        verify_unchanged(mw, guard, payload, source)
                    partial_changes_possible = True
                return config
            importer.read_import_config = confirmed
            ok = importer.load_deck(source)
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
    if action in ("toolkitCatalog", "toolkitBindProfile"):
        if action == "toolkitBindProfile" and params.get("approved") is not True:
            raise ValueError("profile_binding_requires_approval")
        return catalog(mw, action == "toolkitBindProfile")
    if action == "toolkitJob":
        identifier = params.get("id")
        path = job_path(identifier, mw)
        ticket = json.loads((path / "request.json").read_text("utf-8"))
        if not ticket_profile_matches(ticket, mw):
            raise ValueError("wrong_profile")
        if identifier in JOBS:
            return dict(JOBS[identifier], observer_process=PROCESS)
        persisted = path / "result.json"
        if persisted.exists():
            v = json.loads(safe_path(persisted).read_text("utf-8"))
            if v.get("state") in ("running", "queued"):
                v["state"] = "interrupted"
            return dict(v, observer_process=PROCESS)
        return dict(id=identifier, state="prepared", observer_process=PROCESS)
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
    value = dict(id=identifier, state="queued", process=PROCESS, profile=ticket["profile"], action=kind)
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
