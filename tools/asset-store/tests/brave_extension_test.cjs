// Управляемая граница Chrome API: исполняется настоящий background.js.
// Запуск: node --test tools/asset-store/tests/brave_extension_test.cjs
const {test} = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {webcrypto} = require('node:crypto');
const source = fs.readFileSync(process.env.BRAVE_BACKGROUND || path.join(__dirname, '../src/forvo/brave/extension/background.js'), 'utf8');
const op_id = 'a'.repeat(32);
const url = 'https://forvo.com/download/mp3/元気/ja/3661006';
const args = {op_id, surface: '元気', record_id: '3661006'};
const event = () => ({listeners: new Set(), addListener(fn) {this.listeners.add(fn);}, removeListener(fn) {this.listeners.delete(fn);}});
function harness(t) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'brave-extension-'));
    t.after(() => fs.rmSync(dir, {recursive: true, force: true}));
    const clock = {now: Date.now()};
    const clockDate = class extends Date { static now() { return clock.now; } };
    let pairingSecret = 'a'.repeat(64);
    let pairingFetches = 0;
    let nextTimerId = 0;
    class WebSocketStub {
        static OPEN = 1;
        constructor(url) { this.url = url; this.readyState = 0; }
        send() {}
    }
    const data = {};
    const session = {own: {tabId: 1, groupId: 2}};
    const tab = {id: 1, groupId: 2, url: 'https://forvo.com/word/%E5%85%83%E6%B0%97/'};
    const items = [];
    const failures = new Set();
    const calls = [];
    const onRequest = event();
    let beforeSet = async () => {};
    const chrome = {
        runtime: {id: 'test-extension', getURL: path => `chrome-extension://test/${path}`, getManifest: () => ({version: 'test'})},
        storage: {local: {
            get: async key => ({[key]: structuredClone(data[key])}),
            set: async value => {await beforeSet(value); Object.assign(data, structuredClone(value));},
            remove: async key => {if (failures.has('storage_remove')) throw Error(); delete data[key];},
        }},
        tabs: {onRemoved: event(), get: async () => tab, query: async () => [tab], remove: async () => {}},
        tabGroups: {query: async () => []},
        webRequest: {onBeforeRequest: onRequest},
        scripting: {executeScript: async ({args}) => {
            if (args.length === 1) tab.url = args[0];
            return [{result: {code: 'activated'}}];
        }},
        downloads: {
            search: async query => structuredClone(items.filter(i => (query.id === undefined || i.id === query.id) && (!query.state || i.state === query.state))),
            cancel: async id => {
                calls.push(['cancel', id]);
                if (failures.has('cancel')) throw Error();
                const item = items.find(i => i.id === id);
                item.state = failures.has('cancel_complete') ? 'complete' : 'interrupted';
                item.error = item.state === 'interrupted' ? 'USER_CANCELED' : null;
            },
            removeFile: async id => {calls.push(['removeFile', id]); if (failures.has('removeFile')) throw Error(); const item = items.find(i => i.id === id); fs.unlinkSync(item.filename); item.exists = false;},
            erase: async ({id}) => {calls.push(['erase', id]); if (failures.has('erase')) throw Error(); if (failures.has('erase_empty')) return []; const index = items.findIndex(i => i.id === id); return index < 0 ? [] : [items.splice(index, 1)[0].id];},
        },
    };
    function worker() {
        onRequest.listeners.clear();
        const context = vm.createContext({chrome, forvoDom() {}, URL, Date: clockDate, TextEncoder, crypto: webcrypto,
            AbortController,
            fetch: async () => {pairingFetches += 1; return {json: async () => ({schema_version: 1, port: 47613, secret: pairingSecret})};},
            WebSocket: WebSocketStub, console: {info() {}, warn() {}}, setTimeout: () => ++nextTimerId, clearTimeout() {},
            setInterval: () => ++nextTimerId, clearInterval() {}});
        vm.runInContext(source.replace(/^import .*;$/m, '').split('chrome.runtime.onStartup.addListener')[0] +
            '\nglobalThis.api = {downloadBegin, downloadStatus, downloadRelease, downloadCleanup, cleanupRestoredGroups, close: OPERATIONS.close, open: OPERATIONS.open, command, ownTab, connect, closeCurrent: () => socket?.onclose?.(), setPhase: value => {phase = value;}, hasPairing: () => pairing !== null};', context);
        return context.api;
    }
    chrome.storage.session = {
        get: async key => ({[key]: structuredClone(session[key])}),
        set: async value => Object.assign(session, structuredClone(value)),
        remove: async key => {delete session[key];},
    };
    let api = worker();
    return {
        data, session, items, failures, calls, tab, chrome,
        restoredCleanup: () => api.cleanupRestoredGroups(),
        command: (id, op, commandArgs = {}) => api.command(id, op, commandArgs),
        close: () => api.close(),
        open: () => api.open(),
        ownTab: () => api.ownTab(),
        restart() {api = worker();},
        advance(ms) {clock.now += ms;},
        connect: () => api.connect(),
        closeSocket: () => api.closeCurrent(),
        setPhase: value => api.setPhase(value),
        hasPairing: () => api.hasPairing(),
        pairingFetches: () => pairingFetches,
        rotatePairing() {pairingSecret = 'b'.repeat(64);},
        beforeSet(fn) {beforeSet = fn;},
        begin: () => api.downloadBegin(args),
        status: () => api.downloadStatus({op_id}),
        release: (action = 'remove') => api.downloadRelease({op_id, action}),
        cleanup: id => api.downloadCleanup({download_id: id}),
        request(tabId = 1) {for (const fn of onRequest.listeners) fn({url, tabId, timeStamp: Date.now()});},
        add(id, referrer = tab.url, state = 'complete') {
            const filename = path.join(dir, `${id}.mp3`);
            fs.writeFileSync(filename, `bytes-${id}`);
            const item = {id, url, referrer, filename, state, exists: true};
            items.push(item);
            return item;
        },
    };
}

test('недавний чужой файл не связывается до появления своего и сохраняет байты', async t => {
    const h = harness(t);
    const foreign = h.add(1);
    await h.begin(); h.request();
    assert.equal((await h.status()).bound_id, null);
    const own = h.add(2);
    assert.equal((await h.status()).bound_id, 2);
    assert.equal((await h.release()).released, true);
    assert.equal(fs.readFileSync(foreign.filename, 'utf8'), 'bytes-1');
    assert.equal(fs.existsSync(own.filename), false);
    assert.deepEqual(h.items.map(i => i.id), [1]);
});

test('поздний конфликт пересматривается, чужой файл не удаляется', async t => {
    const h = harness(t); await h.begin(); h.request();
    h.add(1); assert.equal((await h.status()).bound_id, 1);
    h.request(9); const foreign = h.add(2, 'https://forvo.com/word/元気/');
    assert.equal((await h.status()).conflict, true);
    await h.release();
    assert.equal(fs.readFileSync(foreign.filename, 'utf8'), 'bytes-2');
});

test('время ожидания имени файла отсчитывается от связывания', async t => {
    const h = harness(t); await h.begin();
    h.advance(15_000); h.request();
    const item = h.add(1, h.tab.url, 'in_progress'); delete item.filename;
    const bound = await h.status();
    assert.equal(bound.elapsed_ms, 15_000);
    assert.equal(bound.bound_elapsed_ms, 0);
    h.advance(7_999);
    assert.equal((await h.status()).bound_elapsed_ms, 7_999);
    h.advance(1);
    assert.equal((await h.status()).bound_elapsed_ms, 8_000);
});

test('после закрытия WebSocket pairing перечитывается даже после ready', async t => {
    const h = harness(t);
    await h.connect();
    assert.equal(h.pairingFetches(), 1);
    assert.equal(h.hasPairing(), true);
    h.rotatePairing(); h.setPhase('ready'); h.closeSocket();
    assert.equal(h.hasPairing(), false);
    await h.connect();
    assert.equal(h.pairingFetches(), 2);
});

for (const failure of ['cancel', 'removeFile', 'erase']) {
    test(`отказ ${failure}: правдивый результат и повтор очистки`, async t => {
        const h = harness(t); await h.begin(); h.request();
        const item = h.add(1, h.tab.url, failure === 'cancel' ? 'in_progress' : 'complete');
        await h.status(); h.failures.add(failure);
        const result = await h.release();
        assert.equal(result.released, false);
        assert.equal(result.erased, false);
        if (failure === 'cancel') assert.equal(result.canceled, false);
        if (failure === 'removeFile') assert.equal(result.removed_file, false);
        assert.ok(h.data.operation);
        assert.equal(fs.existsSync(item.filename), failure !== 'erase');
        assert.ok(h.items.some(i => i.id === 1));
        h.failures.clear(); h.restart();
        assert.equal((await h.release()).released, true);
        assert.equal(h.data.operation, undefined);
    });
}

test('keep, неизвестная и несвязанная операции сохраняют файл', async t => {
    const h = harness(t); assert.equal((await h.release()).reason, 'operation_unknown');
    await h.begin(); const item = h.add(1, 'https://forvo.com/');
    assert.equal((await h.release()).released, true);
    assert.equal(fs.existsSync(item.filename), true);
    h.session.own.download_used = false;
    await h.begin(); h.request(); const own = h.add(2); await h.status();
    assert.equal((await h.release('keep')).released, true);
    assert.equal(fs.existsSync(own.filename), true);
    assert.deepEqual(h.calls, []);
});

test('неподтверждённый erase сохраняет владение файлом для повтора cleanup', async t => {
    const h = harness(t);
    const item = h.add(1);
    h.data.apiDownloads = [1];
    h.failures.add('erase_empty');
    const first = await h.cleanup(1);
    assert.equal(first.removed_file, true);
    assert.equal(first.erased, false);
    assert.equal(first.reason, 'erase_unconfirmed');
    assert.deepEqual(h.data.apiDownloads, [1]);
    assert.equal(fs.existsSync(item.filename), false);
    assert.ok(h.items.some(i => i.id === 1));

    h.failures.clear();
    const retry = await h.cleanup(1);
    assert.equal(retry.removed_file, false);
    assert.equal(retry.erased, true);
    assert.deepEqual(h.data.apiDownloads, []);
    assert.equal(h.items.length, 0);
});

test('cleanup удаляет запись interrupted и снимает владение только после подтверждения', async t => {
    const h = harness(t);
    h.add(1, h.tab.url, 'interrupted');
    h.data.apiDownloads = [1];
    const result = await h.cleanup(1);
    assert.equal(result.removed_file, false);
    assert.equal(result.erased, true);
    assert.deepEqual(h.data.apiDownloads, []);
    assert.equal(h.items.length, 0);
});

test('рестарт до связывания fail closed, после связывания сохраняет доказательство', async t => {
    for (const bound of [false, true]) {
        const h = harness(t); await h.begin(); h.request(); h.add(1);
        if (bound) await h.status();
        h.restart(); const status = await h.status();
        assert.equal(status.observer_lost, !bound);
        assert.equal(status.bound_id, bound ? 1 : null);
        await h.release();
        assert.equal(h.calls.length > 0, bound);
    }
});

test('сокращённый referrer и прежнее состояние не дают владения', async t => {
    const h = harness(t); await h.begin(); h.request(); const item = h.add(1, 'https://forvo.com/');
    assert.equal((await h.status()).bound_id, null);
    // Состояние предыдущей версии не содержит нового доказательства.
    delete h.data.operation.ownership_version; h.data.operation.bound = 1;
    assert.equal((await h.status()).ownership_proven, false);
    assert.equal((await h.release()).released, false);
    assert.equal(fs.existsSync(item.filename), true);
    assert.deepEqual(h.calls, []);
});

test('задержанная запись наблюдателя не воскрешает освобождённую операцию', async t => {
    const h = harness(t); await h.begin();
    let resume, entered;
    const gate = new Promise(resolve => {resume = resolve;});
    const started = new Promise(resolve => {entered = resolve;});
    h.beforeSet(async value => {if (value.operation?.own_requests === 1) {entered(); await gate;}});
    h.request(); await started;
    const releasing = h.release();
    // Управляемое завершение отложенного storage.set, без sleep.
    await Promise.resolve(); await Promise.resolve();
    resume(); await releasing;
    assert.equal(h.data.operation, undefined);
    assert.equal((await h.status()).code, 'operation_unknown');
});

test('разрыв после erase до удаления состояния допускает безопасный повтор', async t => {
    const h = harness(t); await h.begin(); h.request();
    const own = h.add(1); await h.status();
    h.failures.add('storage_remove');
    const first = await h.release();
    assert.equal(first.released, false);
    assert.equal(first.erased, true);
    assert.equal(fs.existsSync(own.filename), false);
    assert.ok(h.data.operation);
    h.failures.clear(); h.restart();
    assert.equal((await h.release()).released, true);
    assert.equal(h.data.operation, undefined);
});

test('новая вторая загрузка из своей страницы делает связывание неоднозначным', async t => {
    const h = harness(t); await h.begin(); h.request(); h.add(1);
    assert.equal((await h.status()).bound_id, 1);
    h.add(2);
    assert.equal((await h.status()).conflict, true);
});

test('завершение между search и cancel не выдаётся за отмену', async t => {
    const h = harness(t); await h.begin(); h.request();
    const item = h.add(1, h.tab.url, 'in_progress'); await h.status();
    h.failures.add('cancel_complete');
    const result = await h.release();
    assert.equal(result.canceled, false);
    assert.equal(result.removed_file, true);
    assert.equal(result.released, true);
    assert.equal(fs.existsSync(item.filename), false);
});

test('похожие восстановленные группы сохраняются независимо от URL и отказа удаления', async t => {
    const h = harness(t);
    delete h.session.own;
    const groups = [{id: 20, title: 'Anki Deck Toolkit', collapsed: true}, {id: 30, title: 'Anki Deck Toolkit', collapsed: true}];
    const tabs = [{id: 21, groupId: 20, active: false, pinned: false, url: 'https://forvo.com/'},
        {id: 31, groupId: 30, active: false, pinned: false, url: 'https://example.org/'}];
    h.chrome.tabGroups = {query: async () => structuredClone(groups)};
    h.chrome.tabs.query = async ({groupId}) => tabs.filter(t => t.groupId === groupId);
    h.chrome.tabs.remove = async () => {assert.fail('неподтверждённая вкладка не удаляется');};
    for (let i = 0; i < 2; i++) {
        await h.restoredCleanup();
        assert.deepEqual(h.data.restoredCleanup, {groups_closed: 0, tabs_closed: 0, groups_kept: 2, ownership_unproven: 2, status: 'ownership_unproven'});
        assert.equal(tabs.length, 2);
    }
});

test('ошибка запроса групп видна и не препятствует повторной диагностике', async t => {
    const h = harness(t);
    h.chrome.tabGroups = {query: async () => {throw Error('unavailable');}};
    await assert.rejects(h.restoredCleanup());
    assert.equal(h.data.restoredCleanup.status, 'error');
    assert.equal(h.data.restoredCleanup.groups_closed, 0);
    h.chrome.tabGroups.query = async () => [];
    await h.restoredCleanup();
    assert.equal(h.data.restoredCleanup.status, 'complete');
});

test('ошибка диагностики допускает попытку закрытия, но не подтверждение очистки', async t => {
    const h = harness(t);
    h.chrome.tabGroups = {query: async () => {throw Error('unavailable');}};
    await h.command(1, 'close');
    assert.equal(h.data.restoredCleanup.status, 'error');
    assert.equal(h.session.own.tabId, 1); // подтверждение отсутствия группы не получено
});

test('отказ close сохраняет доказанную собственную вкладку для повтора', async t => {
    const h = harness(t);
    h.chrome.tabs.remove = async () => {throw Error('remove denied');};
    await assert.rejects(h.close());
    assert.equal(h.session.own.tabId, 1);
    const removed = [];
    h.chrome.tabs.remove = async id => removed.push(id);
    assert.equal((await h.close()).closed, true);
    assert.deepEqual(removed, [1]);
    assert.equal(h.session.own, undefined);
});

test('локальные ID предыдущей сессии не усыновляют вкладку', async t => {
    const h = harness(t);
    h.data.own = h.session.own;
    h.chrome.storage.session = {get: async () => ({}), remove: async () => {}};
    assert.equal(await h.ownTab(), null);
    assert.equal((await h.close()).closed, false);
    assert.equal(h.session.own.tabId, 1);
});

test('перезапуск worker сохраняет сессионное владение, живая группа исключена из диагностики', async t => {
    const h = harness(t);
    h.chrome.tabGroups = {query: async () => [{id: 2}, {id: 3}]};
    h.restart();
    assert.equal((await h.ownTab()).id, 1);
    await h.restoredCleanup();
    assert.equal(h.data.restoredCleanup.groups_kept, 1);
    assert.equal(h.data.restoredCleanup.groups_closed, 0);
    assert.equal(h.session.own.tabId, 1);
});

test('close отказывается от чужой вкладки и неподтверждённых групп', async t => {
    const h = harness(t);
    const removed = [];
    h.chrome.tabs.remove = async id => removed.push(id);
    h.chrome.tabs.query = async () => [{id: 1, groupId: 99, url: 'https://example.org/'}];
    await h.ownTab(); // предварительная диагностика не стирает утраченное доказательство
    await assert.rejects(h.close(), e => e.code === 'ownership_unproven');
    assert.deepEqual(removed, []);
    // Без идентичности этой сессии одноимённая группа не доказана своей: она не
    // закрывается и не блокирует close, а только учитывается.
    delete h.session.own;
    h.chrome.tabGroups.query = async () => [{id: 99, title: 'Anki Deck Toolkit'}];
    const result = await h.close();
    assert.equal(result.closed, false);
    assert.equal(result.unproven_groups_kept, 1);
    assert.deepEqual(removed, []);
});

test('close подтверждает отсутствие группы только после удаления собственной вкладки', async t => {
    const h = harness(t);
    const tabs = [{id: 1, groupId: 2, url: 'https://forvo.com/'}, {id: 4, groupId: 5, url: 'https://example.org/'}];
    h.chrome.tabs.query = async () => tabs;
    h.chrome.tabs.remove = async id => tabs.splice(tabs.findIndex(t => t.id === id), 1);
    h.chrome.tabGroups.query = async () => tabs.map(t => ({id: t.groupId}));
    const result = await h.close();
    assert.equal(result.own_group_absent, true);
    assert.equal(result.own_tab_absent, true);
    assert.deepEqual(tabs.map(t => t.id), [4]);
});

// Управляемая модель вкладок и групп профиля с отказами отдельных вызовов API.
// Несколько harness могут делить одну модель: это две копии расширения (две цели)
// в одном профиле со своими storage.session.
function profile(h, failures = new Set()) {
    let nextTab = 100;
    let nextGroup = 200;
    const tabs = [];
    const groups = [];
    const fail = name => {if (failures.has(name)) throw Error(name);};
    const api = {
        windows: {getLastFocused: async () => ({id: 7})},
        tabs: {
            onRemoved: h.chrome.tabs.onRemoved,
            query: async (q = {}) => structuredClone(tabs.filter(t => (q.windowId === undefined || t.windowId === q.windowId)
                && (q.active === undefined || t.active === q.active) && (q.groupId === undefined || t.groupId === q.groupId))),
            get: async id => {
                fail('get');
                const tab = tabs.find(t => t.id === id);
                if (!tab) throw Error('No tab with id');
                return structuredClone(tab);
            },
            create: async ({windowId, url}) => {
                const tab = {id: nextTab++, windowId, groupId: -1, url, status: 'complete', active: false, pinned: false, width: 800};
                tabs.push(tab);
                return structuredClone(tab);
            },
            group: async ({tabIds, groupId, createProperties}) => {
                fail('group');
                const id = groupId ?? nextGroup++;
                if (!groups.some(g => g.id === id)) groups.push({id, title: '', windowId: createProperties?.windowId});
                for (const tabId of tabIds) tabs.find(t => t.id === tabId).groupId = id;
                return id;
            },
            update: async (id, props) => {
                if ('autoDiscardable' in props) fail('update');
                Object.assign(tabs.find(t => t.id === id), props);
            },
            remove: async id => {
                fail('remove');
                const index = tabs.findIndex(t => t.id === id);
                if (index < 0) throw Error('No tab with id');
                const [tab] = tabs.splice(index, 1);
                const empty = groups.findIndex(g => g.id === tab.groupId && !tabs.some(t => t.groupId === g.id));
                if (empty >= 0) groups.splice(empty, 1);
            },
        },
        tabGroups: {
            query: async (q = {}) => structuredClone(groups.filter(g => q.title === undefined || g.title === q.title)),
            get: async id => structuredClone(groups.find(g => g.id === id)),
            update: async (id, props) => {fail('groupUpdate'); Object.assign(groups.find(g => g.id === id), props);},
        },
    };
    const attach = target => {
        Object.assign(target.chrome, api);
        const session = target.chrome.storage.session;
        const set = session.set;
        session.set = async value => {
            if ('own' in value) fail('own_set');
            else if ('leftoverTabs' in value) fail('leftover_set');
            return set(value);
        };
        delete target.session.own;
    };
    attach(h);
    return {tabs, groups, failures, attach};
}

const STAGES = ['get', 'group', 'groupUpdate', 'update', 'own_set'];

for (const stage of STAGES) {
    test(`отказ ${stage} после создания вкладки: откат подтверждён, ошибка информативна`, async t => {
        const h = harness(t);
        const p = profile(h, new Set([stage]));
        await assert.rejects(h.open(), e => e.code === 'tab_group_failed' && e.message.includes(stage));
        assert.deepEqual(p.tabs, []);
        assert.deepEqual(p.groups, []);
        assert.deepEqual(h.session.leftoverTabs, []);
        assert.equal(h.session.own, undefined);
    });

    test(`двойной отказ ${stage} + remove: остаток сохранён и удаляется повтором`, async t => {
        const h = harness(t);
        const p = profile(h, new Set([stage, 'remove']));
        // Вкладка другой цели того же профиля с тем же названием группы.
        p.tabs.push({id: 50, windowId: 7, groupId: 60, url: 'https://forvo.com/', active: false});
        p.groups.push({id: 60, title: 'Anki Deck Toolkit', windowId: 7});
        await assert.rejects(h.open(), e => e.code === 'tab_cleanup_failed' && e.message.includes(stage));
        const leftover = p.tabs.find(t => t.id !== 50);
        assert.ok(leftover, 'вкладка, которую не удалось удалить, существует');
        assert.deepEqual(h.session.leftoverTabs, [leftover.id]);
        assert.equal(h.session.own, undefined);
        // Остаток мешает открыть ещё одну вкладку: накопления нет.
        p.failures.delete(stage);
        await assert.rejects(h.open(), e => e.code === 'tab_cleanup_failed');
        assert.equal(p.tabs.length, 2);
        // Перезапуск service worker: доказательство живёт в storage.session.
        p.failures.clear(); h.restart();
        const opened = await h.open();
        assert.equal(opened.created, true);
        assert.deepEqual(p.tabs.map(t => t.id).sort(), [50, h.session.own.tabId].sort());
        assert.deepEqual(h.session.leftoverTabs, []);
        assert.ok(p.groups.some(g => g.id === 60), 'группа другой цели не тронута');
    });
}

test('несохранённое доказательство: вкладка удаляется сразу, двойной отказ виден', async t => {
    const h = harness(t);
    const p = profile(h, new Set(['leftover_set']));
    await assert.rejects(h.open(), e => e.code === 'tab_group_failed');
    assert.deepEqual(p.tabs, []);
    p.failures.add('remove');
    await assert.rejects(h.open(), e => e.code === 'tab_cleanup_failed' && e.message.includes('leftover_not_recorded'));
    assert.equal(p.tabs.length, 1);
});

test('отказ удаления прежней вкладки при замене не выдаётся за полную замену', async t => {
    const h = harness(t);
    const p = profile(h);
    await h.open();
    const first = h.session.own;
    h.session.own = {...first, download_used: true};
    p.failures.add('remove');
    await assert.rejects(h.open(), e => e.code === 'tab_cleanup_failed' && e.message.includes('previous_tab_not_removed'));
    // Новая вкладка — собственная в той же группе, прежняя — остаток для повтора.
    assert.notEqual(h.session.own.tabId, first.tabId);
    assert.equal(h.session.own.groupId, first.groupId);
    assert.equal(h.session.own.download_used, false);
    assert.deepEqual(h.session.leftoverTabs, [first.tabId]);
    assert.equal(p.tabs.length, 2);
    p.failures.clear();
    const again = await h.open();
    assert.equal(again.created, false);
    assert.deepEqual(p.tabs.map(t => t.id), [h.session.own.tabId]);
    assert.deepEqual(h.session.leftoverTabs, []);
    // close повторяет очистку остатков и честно сообщает об их сохранении.
    h.session.own = {...h.session.own, download_used: true};
    await h.open();
    assert.equal(p.tabs.length, 1);
    h.session.leftoverTabs = [999, ...(h.session.leftoverTabs || [])];
    p.tabs.push({id: 999, windowId: 7, groupId: -1, url: 'https://forvo.com/', active: false});
    p.failures.add('remove');
    await assert.rejects(h.close());
    assert.deepEqual(h.session.leftoverTabs, [999]);
    assert.ok(h.session.own, 'собственная вкладка не удалена — идентичность сохранена');
    p.failures.clear();
    const closed = await h.close();
    assert.equal(closed.own_tab_absent, true);
    assert.deepEqual(p.tabs, []);
    assert.deepEqual(h.session.leftoverTabs, []);
});

test('остаток, уведённый пользователем на другой сайт, не закрывается', async t => {
    const h = harness(t);
    const p = profile(h);
    p.tabs.push({id: 40, windowId: 7, groupId: -1, url: 'https://example.org/', active: true, width: 800});
    h.session.leftoverTabs = [40];
    await h.open();
    assert.ok(p.tabs.some(t => t.id === 40));
    assert.deepEqual(h.session.leftoverTabs, []);
});

test('две цели в одном профиле: close каждой закрывает только своё', async t => {
    const a = harness(t);
    const b = harness(t);
    const p = profile(a);
    p.attach(b);
    await a.open();
    await b.open();
    assert.equal(p.groups.length, 2);
    assert.ok(p.groups.every(g => g.title === 'Anki Deck Toolkit'));
    const bOwn = b.session.own;
    const closedA = await a.close();
    assert.equal(closedA.closed, true);
    assert.equal(closedA.own_group_absent, true);
    assert.equal(closedA.unproven_groups_kept, 1);
    assert.deepEqual(p.tabs.map(t => t.id), [bOwn.tabId]);
    assert.deepEqual(p.groups.map(g => g.id), [bOwn.groupId]);
    // Повторный close A идемпотентен и по-прежнему не трогает B.
    assert.equal((await a.close()).closed, false);
    assert.equal(p.tabs.length, 1);
    const closedB = await b.close();
    assert.equal(closedB.closed, true);
    assert.equal(closedB.unproven_groups_kept, 0);
    assert.deepEqual(p.tabs, []);
    assert.deepEqual(p.groups, []);
});
