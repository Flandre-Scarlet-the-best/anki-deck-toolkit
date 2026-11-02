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
    const data = {own: {tabId: 1, groupId: 2}};
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
        tabs: {onRemoved: event(), get: async () => tab},
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
            removeFile: async id => {calls.push(['removeFile', id]); if (failures.has('removeFile')) throw Error(); fs.unlinkSync(items.find(i => i.id === id).filename);},
            erase: async ({id}) => {calls.push(['erase', id]); if (failures.has('erase')) throw Error(); const index = items.findIndex(i => i.id === id); return index < 0 ? [] : [items.splice(index, 1)[0].id];},
        },
    };
    function worker() {
        onRequest.listeners.clear();
        const context = vm.createContext({chrome, forvoDom() {}, URL, Date: clockDate, TextEncoder, crypto: webcrypto,
            fetch: async () => {pairingFetches += 1; return {json: async () => ({schema_version: 1, port: 47613, secret: pairingSecret})};},
            WebSocket: WebSocketStub, console: {info() {}, warn() {}}, setTimeout: () => ++nextTimerId, clearTimeout() {},
            setInterval: () => ++nextTimerId, clearInterval() {}});
        vm.runInContext(source.replace(/^import .*;$/m, '').split('const OPERATIONS =')[0] +
            '\nglobalThis.api = {downloadBegin, downloadStatus, downloadRelease, connect, closeCurrent: () => socket?.onclose?.(), setPhase: value => {phase = value;}, hasPairing: () => pairing !== null};', context);
        return context.api;
    }
    let api = worker();
    return {
        data, items, failures, calls, tab,
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
    h.data.own.download_used = false;
    await h.begin(); h.request(); const own = h.add(2); await h.status();
    assert.equal((await h.release('keep')).released, true);
    assert.equal(fs.existsSync(own.filename), true);
    assert.deepEqual(h.calls, []);
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
