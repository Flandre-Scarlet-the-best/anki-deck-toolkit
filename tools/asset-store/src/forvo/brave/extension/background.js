// Тонкий транспорт Forvo для pronunciation-assets. Расширение выполняет только
// типизированные команды локального Rust bridge над собственной вкладкой;
// выбор кандидата, проверка URL, байтов и MP3 остаются в Rust.
import {forvoAuth, forvoDom} from './forvo_page.js';

const PROTOCOL = 'forvo-adapter/v1';
const WS_PATH = '/forvo-adapter/v1';
const GROUP_TITLE = 'Anki Deck Toolkit';
const FORVO_ORIGIN = 'https://forvo.com';
const MAX_INBOUND_CHARS = 64 * 1024;
const KEEPALIVE_MS = 20000;
const RETRY_MS = 3000;
const QUICK_RETRIES = 5;
const HANDSHAKE_MS = 10000;
const WINDOW_LAYOUT_MS = 3000;
const NAVIGATION_VISIBLE_MS = 5000;
const VERSION = chrome.runtime.getManifest().version;

let socket = null;
let phase = 'idle';
let pairing = null;
let keepalive = null;
let retry = null;
let handshakeTimer = null;
let operation = null;
let ownNonce = null;
let bridgeNonce = null;

class Failure extends Error {
    constructor(code, message) {
        super(message || code);
        this.code = code;
    }
}

const hex = bytes => Array.from(bytes, b => b.toString(16).padStart(2, '0')).join('');
const fromHex = text => new Uint8Array(text.match(/../g).map(h => parseInt(h, 16)));
const randomHex = () => hex(crypto.getRandomValues(new Uint8Array(32)));
const encoder = new TextEncoder();

async function loadPairing() {
    if (pairing) return pairing;
    const response = await fetch(chrome.runtime.getURL('pairing.json'), {cache: 'no-store'});
    const value = await response.json();
    if (value?.schema_version !== 1 || !Number.isInteger(value.port) || value.port < 1024 || value.port > 65535 ||
        typeof value.secret !== 'string' || !/^[0-9a-f]{64}$/.test(value.secret)) {
        throw new Failure('pairing_invalid');
    }
    const key = await crypto.subtle.importKey('raw', fromHex(value.secret), {name: 'HMAC', hash: 'SHA-256'}, false, ['sign', 'verify']);
    pairing = {port: value.port, key};
    return pairing;
}

// Экземпляр расширения в этом профиле: случайный ID в собственном storage.local.
// Bridge закрепляет его при первом подключении и не принимает экземпляр из
// другого профиля, даже если туда загружен тот же каталог (тот же ID расширения).
const INSTANCE_KEY = 'instance';
async function instanceId() {
    const {[INSTANCE_KEY]: stored} = await chrome.storage.local.get(INSTANCE_KEY);
    if (typeof stored === 'string' && /^[0-9a-f]{64}$/.test(stored)) return stored;
    const created = randomHex();
    await chrome.storage.local.set({[INSTANCE_KEY]: created});
    return created;
}

const fn = value => typeof value === 'function';

// Сведения о браузере только из собственных возможностей расширения: бренды
// userAgentData и наличие нужных API. Настройки, профили и другие расширения не читаются.
async function browserFacts() {
    const data = self.navigator?.userAgentData;
    const brands = (data?.brands || []).map(b => ({brand: String(b.brand), version: String(b.version)}));
    let chromium = null;
    try {
        const high = await data?.getHighEntropyValues(['fullVersionList']);
        chromium = high?.fullVersionList?.find(b => b.brand === 'Chromium')?.version || null;
    } catch {
        chromium = null;
    }
    chromium ??= brands.find(b => b.brand === 'Chromium')?.version || null;
    return {
        brands, chromium_version: chromium, platform: data?.platform || null,
        manifest_version: chrome.runtime.getManifest().manifest_version,
        capabilities: {
            service_worker: typeof ServiceWorkerGlobalScope !== 'undefined' && self instanceof ServiceWorkerGlobalScope,
            downloads: fn(chrome.downloads?.cancel) && fn(chrome.downloads?.erase) && fn(chrome.downloads?.removeFile),
            downloads_search: fn(chrome.downloads?.search),
            web_request_observe: fn(chrome.webRequest?.onBeforeRequest?.addListener),
            tabs: fn(chrome.tabs?.create) && fn(chrome.tabs?.group) && fn(chrome.tabs?.query),
            tab_groups: fn(chrome.tabGroups?.update) && fn(chrome.tabGroups?.query),
            scripting: fn(chrome.scripting?.executeScript),
            storage_local: fn(chrome.storage?.local?.get),
            storage_session: fn(chrome.storage?.session?.get),
            alarms: fn(chrome.alarms?.create),
            websocket_hmac: fn(self.WebSocket) && fn(crypto?.subtle?.sign),
        },
    };
}

const macText = (role, first, second) => `${PROTOCOL}|${role}|${first}|${second}|${chrome.runtime.id}`;

function send(message) {
    if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
}

// После разрыва несколько быстрых попыток покрывают перезапуск bridge; дальше
// подключение ждёт alarm раз в 30 секунд, чтобы не засорять журнал ошибок расширения.
let quickRetries = 0;
function scheduleRetry() {
    clearTimeout(retry);
    if (quickRetries >= QUICK_RETRIES) return;
    quickRetries += 1;
    retry = setTimeout(connect, RETRY_MS);
}

function teardown() {
    clearInterval(keepalive);
    clearTimeout(handshakeTimer);
    keepalive = null;
    operation?.abort();
    operation = null;
    socket = null;
    phase = 'idle';
    ownNonce = bridgeNonce = null;
}

async function connect() {
    if (socket) return;
    let current;
    try {
        const {port} = await loadPairing();
        if (socket) return;
        current = new WebSocket(`ws://127.0.0.1:${port}${WS_PATH}`);
    } catch (error) {
        console.warn('forvo-adapter: сопряжение недоступно', error?.code || error?.name);
        return;
    }
    socket = current;
    phase = 'handshake';
    handshakeTimer = setTimeout(() => current.close(4001, 'handshake_timeout'), HANDSHAKE_MS);
    current.onmessage = event => {
        if (current !== socket) return;
        handle(event.data).catch(() => current.close(4002, 'protocol_error'));
    };
    current.onclose = () => {
        if (current !== socket) return;
        teardown();
        scheduleRetry();
    };
    current.onerror = () => {};
}

async function handle(data) {
    if (typeof data !== 'string' || data.length > MAX_INBOUND_CHARS) throw new Failure('message_rejected');
    const message = JSON.parse(data);
    if (phase === 'handshake' && message?.type === 'hello' && message.v === 1 && !bridgeNonce &&
        typeof message.nonce === 'string' && /^[0-9a-f]{64}$/.test(message.nonce)) {
        bridgeNonce = message.nonce;
        ownNonce = randomHex();
        const signature = await crypto.subtle.sign('HMAC', pairing.key, encoder.encode(macText('extension', bridgeNonce, ownNonce)));
        send({type: 'auth', v: 1, nonce: ownNonce, extension_id: chrome.runtime.id, version: VERSION, mac: hex(new Uint8Array(signature))});
        return;
    }
    if (phase === 'handshake' && message?.type === 'auth_ok' && ownNonce && typeof message.mac === 'string' && /^[0-9a-f]{64}$/.test(message.mac)) {
        const valid = await crypto.subtle.verify('HMAC', pairing.key, fromHex(message.mac), encoder.encode(macText('bridge', ownNonce, bridgeNonce)));
        if (!valid) throw new Failure('bridge_not_paired');
        clearTimeout(handshakeTimer);
        phase = 'ready';
        quickRetries = 0;
        keepalive = setInterval(() => send({type: 'keepalive'}), KEEPALIVE_MS);
        send({type: 'ready', version: VERSION, instance: await instanceId(), browser: await browserFacts()});
        console.info('forvo-adapter: bridge подключён');
        return;
    }
    if (phase === 'ready' && message?.type === 'cmd' && Number.isSafeInteger(message.id) && typeof message.op === 'string') {
        await command(message.id, message.op, message.args && typeof message.args === 'object' ? message.args : {});
        return;
    }
    throw new Failure('message_rejected');
}

async function command(id, op, args) {
    const handler = OPERATIONS[op];
    if (!handler) return send({type: 'res', id, ok: false, code: 'unknown_operation'});
    if (operation) return send({type: 'res', id, ok: false, code: 'busy'});
    const controller = new AbortController();
    operation = controller;
    try {
        await cleanupRestoredGroups();
        const result = await handler(args, controller.signal);
        send({type: 'res', id, ok: true, result});
    } catch (error) {
        send({type: 'res', id, ok: false, code: error instanceof Failure ? error.code : 'extension_failure',
            message: error instanceof Failure ? error.message : String(error?.name || 'Error')});
    } finally {
        if (operation === controller) operation = null;
    }
}

// --- Проверка аргументов -------------------------------------------------

function text(value, max) {
    if (typeof value !== 'string' || value.length === 0 || value.length > max || /[\u0000-\u001f]/.test(value)) throw new Failure('invalid_arguments');
    return value;
}
function recordId(value) {
    if (typeof value !== 'string' || !/^[1-9][0-9]{0,19}$/.test(value)) throw new Failure('invalid_arguments');
    return value;
}
function duration(value, min, max) {
    if (!Number.isInteger(value) || value < min || value > max) throw new Failure('invalid_arguments');
    return value;
}
function forvoUrl(value) {
    const url = new URL(text(value, 2048));
    if (url.origin !== FORVO_ORIGIN || url.username || url.password) throw new Failure('url_not_allowed');
    return url.href;
}
const wordPath = surface => '/word/' + encodeURIComponent(surface) + '/';

function delay(ms) {
    return new Promise(resolve => setTimeout(resolve, ms));
}

// --- Собственная вкладка --------------------------------------------------

async function ownTab() {
    const {own} = await chrome.storage.local.get('own');
    if (!own) return null;
    try {
        const tab = await chrome.tabs.get(own.tabId);
        // Владение доказывает сохранённый tab ID в текущей сессии браузера и наша группа.
        if (tab.groupId === own.groupId && new URL(tab.url || tab.pendingUrl || 'about:blank').origin === FORVO_ORIGIN) return tab;
    } catch {
        // Вкладка закрыта пользователем: состояние сбрасывается, чужие вкладки не усыновляются.
    }
    await chrome.storage.local.remove('own');
    return null;
}

async function requireTab() {
    const tab = await ownTab();
    if (!tab) throw new Failure('own_tab_missing');
    return tab;
}

// Chromium разрешает вкладке без жеста пользователя только первую загрузку
// (DownloadRequestLimiter); следующие блокируются или вызывают панель «Скачать
// несколько файлов». Поэтому вкладка, уже начавшая загрузку, заменяется свежей
// в той же группе, а настройки браузера не меняются.
async function ensureTab() {
    const existing = await ownTab();
    const {own} = await chrome.storage.local.get('own');
    if (existing && !own?.download_used) return {tab: existing, created: false, replaced: false};
    const window = existing ? {id: existing.windowId} : await chrome.windows.getLastFocused({windowTypes: ['normal']});
    await windowLaidOut(window.id);
    const tab = await chrome.tabs.create({windowId: window.id, url: FORVO_ORIGIN + '/', active: false});
    try {
        await navigationVisible(tab.id);
        const groupId = existing
            ? await chrome.tabs.group({tabIds: [tab.id], groupId: existing.groupId})
            : await chrome.tabs.group({tabIds: [tab.id], createProperties: {windowId: window.id}});
        await chrome.tabGroups.update(groupId, {title: GROUP_TITLE, color: 'blue', collapsed: true});
        await chrome.tabs.update(tab.id, {autoDiscardable: false});
        await chrome.storage.local.set({own: {tabId: tab.id, groupId, windowId: window.id, download_used: false}});
        if (existing) await chrome.tabs.remove(existing.id).catch(() => {});
        return {tab: await chrome.tabs.get(tab.id), created: true, replaced: !!existing};
    } catch (error) {
        await chrome.tabs.remove(tab.id).catch(() => {});
        throw new Failure('tab_group_failed', String(error?.message || error));
    }
}

// Сразу после запуска браузера окно может быть ещё не размечено: у Vivaldi интерфейс
// сам является веб-страницей, и вкладка, созданная до его разметки, навсегда остаётся
// с нулевым viewport. Ждём, пока у активной вкладки окна появится размер; дольше
// WINDOW_LAYOUT_MS не ждём, тогда проверка кнопки честно сообщит о недоступности.
async function windowLaidOut(windowId) {
    const deadline = Date.now() + WINDOW_LAYOUT_MS;
    while (Date.now() < deadline) {
        const [front] = await chrome.tabs.query({windowId, active: true});
        if (!front || front.width > 0) return;
        await new Promise(resolve => setTimeout(resolve, 100));
    }
}

// Новая вкладка проходит через начальную about:blank. Vivaldi фиксирует её уже после
// начала навигации: на миг вкладка становится status=complete без url и pendingUrl.
// Такая вкладка не доказуемо наша (адрес не виден), и следующая команда создала бы
// вторую. Поэтому вкладка возвращается только после того, как на forvo.com
// зафиксирован её url; потерянная навигация собственной вкладки повторяется один раз.
async function navigationVisible(tabId) {
    for (const attempt of [0, 1]) {
        if (attempt) await chrome.tabs.update(tabId, {url: FORVO_ORIGIN + '/'});
        const deadline = Date.now() + NAVIGATION_VISIBLE_MS;
        while (Date.now() < deadline) {
            const tab = await chrome.tabs.get(tabId);
            if (committedForvo(tab)) return;
            if (tab.status === 'complete' && !tab.url && !tab.pendingUrl && Date.now() > deadline - NAVIGATION_VISIBLE_MS / 2) break;
            await new Promise(resolve => setTimeout(resolve, 50));
        }
    }
    throw new Failure('tab_group_failed', 'navigation_not_visible');
}

function committedForvo(tab) {
    try {
        return new URL(tab.url || 'about:blank').origin === FORVO_ORIGIN;
    } catch {
        return false;
    }
}

// После перезапуска браузера восстановление сессии возвращает вкладку прошлой
// группы, а tab ID и group ID уже новые. Закрываются только вкладки групп,
// которые выглядят собственными по всем признакам: наш заголовок, группа свёрнута,
// каждая вкладка неактивна, не закреплена и открыта на forvo.com. Адреса других
// сайтов расширению не видны, поэтому любая посторонняя вкладка исключает группу.
// Проверка идёт один раз за сессию браузера: storage.session очищается при
// перезапуске браузера и перезагрузке расширения. Действующая собственная группа
// исключается.
async function cleanupRestoredGroups() {
    const {[SESSION_CLEANUP_KEY]: done} = await chrome.storage.session.get(SESSION_CLEANUP_KEY);
    if (done) return;
    await chrome.storage.session.set({[SESSION_CLEANUP_KEY]: true});
    const current = await ownTab();
    let groups = 0, tabs = 0, kept = 0;
    for (const group of await chrome.tabGroups.query({title: GROUP_TITLE})) {
        if (group.id === current?.groupId) continue;
        const members = await chrome.tabs.query({groupId: group.id});
        const restored = group.collapsed && members.length > 0 && members.every(t => !t.active && !t.pinned && isForvo(t));
        if (!restored) {
            kept += 1;
            continue;
        }
        await chrome.tabs.remove(members.map(t => t.id)).catch(() => {});
        groups += 1;
        tabs += members.length;
    }
    await chrome.storage.local.set({[CLEANUP_KEY]: {groups_closed: groups, tabs_closed: tabs, groups_kept: kept}});
}

function isForvo(tab) {
    try {
        return new URL(tab.url || tab.pendingUrl || 'about:blank').origin === FORVO_ORIGIN;
    } catch {
        return false;
    }
}

async function markTabUsed(tabId) {
    const {own} = await chrome.storage.local.get('own');
    if (own?.tabId === tabId) await chrome.storage.local.set({own: {...own, download_used: true}});
}

async function tabSummary(tab) {
    if (!tab) return {exists: false};
    const group = await chrome.tabGroups.get(tab.groupId).catch(() => null);
    const url = new URL(tab.url || tab.pendingUrl || 'about:blank');
    return {exists: true, tab_id: tab.id, window_id: tab.windowId, group_id: tab.groupId, group_title: group?.title ?? null,
        group_collapsed: group?.collapsed ?? null, active: tab.active, status: tab.status,
        origin: url.origin, path: url.pathname, has_query: url.search !== ''};
}

chrome.tabs.onRemoved.addListener(async tabId => {
    const {own} = await chrome.storage.local.get('own');
    if (own?.tabId === tabId) await chrome.storage.local.remove('own');
});

async function inject(tabId, func, args = []) {
    try {
        const [result] = await chrome.scripting.executeScript({target: {tabId, frameIds: [0]}, func, args});
        return result?.result;
    } catch (error) {
        throw new Failure('injection_failed', String(error?.message || error).slice(0, 200));
    }
}

function waitComplete(tabId, ms, signal) {
    return new Promise(resolve => {
        const finish = status => {
            clearTimeout(timer);
            chrome.tabs.onUpdated.removeListener(listener);
            resolve(status);
        };
        const listener = (id, info) => {
            if (id === tabId && info.status === 'complete') finish('complete');
        };
        const timer = setTimeout(() => finish('timeout'), ms);
        chrome.tabs.onUpdated.addListener(listener);
        signal.addEventListener('abort', () => finish('aborted'), {once: true});
    });
}

// --- Загрузка выбранной записи ----------------------------------------------
//
// Один штатный download: обычная кнопка выбранной записи в собственной вкладке.
// Расширение не регистрирует onDeterminingFilename и не отменяет, не
// переименовывает и не удаляет загрузки, владение которыми не доказано.
// У DownloadItem нет tabId, поэтому владение доказывает связка:
//  1) запрос ожидаемого адреса выбранной записи пришёл из собственной вкладки
//     (webRequest.onBeforeRequest без блокировки сообщает tabId);
//  2) в окне операции ровно одна загрузка с этим адресом и ни одного такого же
//     запроса из других вкладок;
//  3) связывание выполняется, пока наблюдатель операции жив. После рестарта
//     service worker несвязанная операция загрузку не усыновляет (fail closed).
// Состояние операции хранится в chrome.storage.local: рестарт service worker и
// переподключение bridge его не теряют, а таймеры service worker не считаются
// гарантией — ход операции опрашивает Rust.

const OP_KEY = 'operation';
const API_KEY = 'apiDownloads';
const SESSION_CLEANUP_KEY = 'restoredCleanupDone';
const CLEANUP_KEY = 'restoredCleanup';
const DOWNLOAD_FILTER = {urls: [FORVO_ORIGIN + '/download/*']};
const START_MARGIN_MS = 1000;
const observers = new Map();

function describeUrl(raw) {
    try {
        const url = new URL(raw);
        return {origin: url.origin, path: url.pathname, has_query: url.search !== '', has_fragment: url.hash !== '', has_credentials: !!(url.username || url.password)};
    } catch {
        return null;
    }
}

function operationId(value) {
    if (typeof value !== 'string' || !/^[0-9a-f]{32}$/.test(value)) throw new Failure('invalid_arguments');
    return value;
}

// Тот же контракт адреса, что matches_download в Rust: /download/mp3/<слово>/ja/<record_id>.
function expectedDownload(raw, surface, id) {
    let url;
    try {
        url = new URL(raw);
    } catch {
        return false;
    }
    if (url.origin !== FORVO_ORIGIN || url.username || url.password || url.search || url.hash) return false;
    let parts;
    try {
        parts = url.pathname.split('/').filter(Boolean).map(decodeURIComponent);
    } catch {
        return false;
    }
    return parts.length === 5 && parts[0] === 'download' && parts[1] === 'mp3' && parts[2] === surface && parts[3] === 'ja' && parts[4] === id;
}

async function loadOp() {
    return (await chrome.storage.local.get(OP_KEY))[OP_KEY] || null;
}

// Изменения операции сериализуются: события webRequest и опрос Rust не теряют друг друга.
let opQueue = Promise.resolve();
function mutateOp(opId, change) {
    const run = opQueue.then(async () => {
        const op = await loadOp();
        if (!op || op.op_id !== opId) return null;
        if (change(op) !== false) await chrome.storage.local.set({[OP_KEY]: op});
        return op;
    });
    opQueue = run.catch(() => {});
    return run;
}

function trace(opId, event, fields = {}) {
    console.info('forvo-adapter', JSON.stringify({operation_id: opId, event, ...fields}));
}

function observe(op) {
    const onRequest = details => {
        if (!expectedDownload(details.url, op.surface, op.record_id)) return;
        const own = details.tabId === op.tab_id;
        mutateOp(op.op_id, current => {
            if (own) {
                current.own_requests += 1;
                current.first_request_at ??= Math.round(details.timeStamp);
            } else {
                current.foreign_requests += 1;
            }
        });
        trace(op.op_id, own ? 'own_request' : 'foreign_request');
    };
    chrome.webRequest.onBeforeRequest.addListener(onRequest, DOWNLOAD_FILTER);
    observers.set(op.op_id, onRequest);
}

function unobserve(opId) {
    const listener = observers.get(opId);
    if (listener) chrome.webRequest.onBeforeRequest.removeListener(listener);
    observers.delete(opId);
}

function sanitizeItem(item, op) {
    const name = item.filename ? item.filename.split(/[\\/]/).pop() : null;
    return {id: item.id, state: item.state, error: item.error || null, danger: item.danger, exists: item.exists, paused: item.paused,
        has_filename: !!item.filename, filename: item.filename || null, file_name: name, bytes_received: item.bytesReceived,
        total_bytes: item.totalBytes, file_size: item.fileSize, mime: item.mime || null, url: describeUrl(item.url),
        final_url: describeUrl(item.finalUrl), referrer_is_own_page: item.referrer === op.page_url,
        referrer_origin_is_forvo: describeUrl(item.referrer)?.origin === FORVO_ORIGIN, by_extension: !!item.byExtensionId,
        start_time: item.startTime || null, end_time: item.endTime || null};
}

async function downloadBegin(args) {
    const opId = operationId(args.op_id);
    const surface = text(args.surface, 128);
    const id = recordId(args.record_id);
    if (await loadOp()) throw new Failure('operation_pending');
    const tab = await requireTab();
    const {own} = await chrome.storage.local.get('own');
    if (own?.download_used) throw new Failure('own_tab_used');
    const page = new URL(tab.url);
    if (page.origin !== FORVO_ORIGIN || decodeURIComponent(page.pathname) !== decodeURIComponent(wordPath(surface))) throw new Failure('page_identity_mismatch');
    // Уже идущая загрузка того же адреса сделала бы владение неразличимым: кнопка не нажимается.
    const active = await chrome.downloads.search({state: 'in_progress'});
    if (active.some(item => expectedDownload(item.url, surface, id))) {
        trace(opId, 'conflict_before_click');
        return {code: 'download_conflict', phase: 'before_click'};
    }
    const op = {op_id: opId, surface, record_id: id, tab_id: tab.id, page_url: tab.url, began_at: Date.now(), own_requests: 0,
        foreign_requests: 0, first_request_at: null, bound: null, conflict: false, observer_lost: false};
    await chrome.storage.local.set({[OP_KEY]: op});
    await markTabUsed(tab.id);
    observe(op);
    trace(opId, 'begin', {tab_id: tab.id});
    let activated;
    try {
        activated = await inject(tab.id, forvoDom, [surface, 'activate', id]);
    } catch (error) {
        await dropOp(opId);
        throw error;
    }
    if (activated?.code !== 'activated') {
        await dropOp(opId);
        return {code: activated?.code || 'dom_changed', candidates: activated?.candidates || []};
    }
    trace(opId, 'activated');
    return {code: 'activated', op_id: opId, began_at: op.began_at};
}

async function dropOp(opId) {
    unobserve(opId);
    const op = await loadOp();
    if (op?.op_id === opId) await chrome.storage.local.remove(OP_KEY);
}

async function downloadStatus(args) {
    const opId = operationId(args.op_id);
    const op = await loadOp();
    if (!op || op.op_id !== opId) return {code: 'operation_unknown', op_id: opId};
    const alive = observers.has(opId);
    const tab = await ownTab();
    const tabPresent = !!tab && tab.id === op.tab_id;
    const items = (await chrome.downloads.search({startedAfter: new Date(op.began_at - START_MARGIN_MS).toISOString()}))
        .filter(item => expectedDownload(item.url, op.surface, op.record_id));
    const current = await mutateOp(opId, state => {
        if (state.bound !== null) return false;
        if (state.foreign_requests > 0 || items.length > 1) {
            if (state.conflict) return false;
            state.conflict = true;
            trace(opId, 'conflict', {matching_downloads: items.length});
            return;
        }
        if (!alive) {
            if (state.observer_lost) return false;
            state.observer_lost = true;
            trace(opId, 'observer_lost');
            return;
        }
        if (state.own_requests > 0 && items.length === 1 && !state.conflict && !state.observer_lost) {
            state.bound = items[0].id;
            state.bound_at = Date.now();
            trace(opId, 'bound', {download_id: state.bound});
            return;
        }
        return false;
    });
    if (!current) return {code: 'operation_unknown', op_id: opId};
    const [item] = current.bound === null ? [] : await chrome.downloads.search({id: current.bound});
    return {code: 'status', op_id: opId, observer_alive: alive, own_requests: current.own_requests,
        foreign_requests: current.foreign_requests, matching_downloads: items.length, conflict: current.conflict,
        observer_lost: current.observer_lost, bound_id: current.bound, bound_missing: current.bound !== null && !item,
        item: item ? sanitizeItem(item, current) : null, tab_present: tabPresent,
        page_unchanged: tabPresent && tab.url === current.page_url, elapsed_ms: Date.now() - current.began_at};
}

// Освобождение операции. Действия применяются только к загрузке, владение
// которой доказано связыванием; несвязанная операция ничего не трогает.
async function downloadRelease(args) {
    const opId = operationId(args.op_id);
    const action = args.action;
    if (action !== 'remove' && action !== 'keep') throw new Failure('invalid_arguments');
    unobserve(opId);
    const op = await loadOp();
    if (!op || op.op_id !== opId) return {released: false, reason: 'operation_unknown'};
    const result = {released: true, bound: op.bound !== null, canceled: false, removed_file: false, erased: false};
    if (op.bound !== null && action === 'remove') {
        const [item] = await chrome.downloads.search({id: op.bound});
        if (item && expectedDownload(item.url, op.surface, op.record_id)) {
            if (item.state === 'in_progress') {
                await chrome.downloads.cancel(item.id).catch(() => {});
                result.canceled = true;
            }
            if (item.state === 'complete' && item.exists) {
                await chrome.downloads.removeFile(item.id).then(() => {
                    result.removed_file = true;
                }).catch(() => {});
            }
            await chrome.downloads.erase({id: item.id});
            result.erased = true;
        }
    }
    await chrome.storage.local.remove(OP_KEY);
    trace(opId, 'released', {action, ...result});
    return result;
}

async function operationSummary() {
    const op = await loadOp();
    return op ? {op_id: op.op_id, bound: op.bound !== null, conflict: op.conflict, observer_alive: observers.has(op.op_id),
        age_ms: Date.now() - op.began_at} : null;
}

// Эксперимент официального API для приёмки: повторная загрузка адреса уже
// связанной собственной загрузки через chrome.downloads.download.
async function downloadsApi(args, signal) {
    const opId = operationId(args.op_id);
    const op = await loadOp();
    if (!op || op.op_id !== opId || op.bound === null) throw new Failure('download_not_owned');
    const [source] = await chrome.downloads.search({id: op.bound});
    if (!source || source.state !== 'complete' || !expectedDownload(source.url, op.surface, op.record_id)) throw new Failure('download_not_owned');
    const stem = text(args.filename_stem, 80);
    if (!/^[a-z0-9-]+$/.test(stem)) throw new Failure('invalid_arguments');
    const promptMs = duration(args.prompt_ms, 1000, 60000);
    const timeout = duration(args.timeout_ms, 1000, 120000);
    const created = await chrome.downloads.download({url: source.url, filename: `AnkiDeckToolkit/${stem}.mp3`, saveAs: false, conflictAction: 'uniquify'});
    const {[API_KEY]: owned = []} = await chrome.storage.local.get(API_KEY);
    await chrome.storage.local.set({[API_KEY]: [...owned, created].slice(-32)});
    const started = Date.now();
    for (;;) {
        if (signal.aborted) throw new Failure('aborted');
        const [item] = await chrome.downloads.search({id: created});
        if (!item) return {code: 'download_vanished', download_id: created};
        if (item.state === 'complete') {
            return {code: 'complete', download_id: created, filename: item.filename, file_size: item.fileSize, bytes_received: item.bytesReceived,
                total_bytes: item.totalBytes, mime: item.mime || null, exists: item.exists, danger: item.danger, final_url: describeUrl(item.finalUrl)};
        }
        if (item.state === 'interrupted') return {code: 'interrupted', download_id: created, error: item.error || null, has_filename: !!item.filename};
        const elapsed = Date.now() - started;
        // Пока имя пусто, Chromium ждёт выбора пути пользователем.
        if (!item.filename && elapsed >= promptMs) {
            await chrome.downloads.cancel(created).catch(() => {});
            await chrome.downloads.erase({id: created}).catch(() => {});
            return {code: 'save_as_prompt_suspected', download_id: created, waited_ms: elapsed};
        }
        if (elapsed >= timeout) {
            await chrome.downloads.cancel(created).catch(() => {});
            return {code: 'download_timeout', download_id: created};
        }
        await delay(250);
    }
}

async function downloadCleanup(args) {
    if (!Number.isSafeInteger(args.download_id)) throw new Failure('invalid_arguments');
    const {[API_KEY]: owned = []} = await chrome.storage.local.get(API_KEY);
    if (!owned.includes(args.download_id)) throw new Failure('download_not_owned');
    const [item] = await chrome.downloads.search({id: args.download_id});
    let removed = false;
    if (item?.state === 'complete' && item.exists) {
        await chrome.downloads.removeFile(args.download_id);
        removed = true;
    }
    await chrome.downloads.erase({id: args.download_id});
    await chrome.storage.local.set({[API_KEY]: owned.filter(id => id !== args.download_id)});
    return {removed_file: removed, erased: true};
}

// Снимок без URL и заголовков: только структура вкладок и групп вне собственной.
async function snapshot() {
    const {own} = await chrome.storage.local.get('own');
    const tabs = await chrome.tabs.query({});
    const groups = await chrome.tabGroups.query({});
    return {
        tabs: tabs.filter(t => t.id !== own?.tabId).map(t => ({id: t.id, window_id: t.windowId, index: t.index, group_id: t.groupId,
            active: t.active, pinned: t.pinned})),
        groups: groups.filter(g => g.id !== own?.groupId).map(g => ({id: g.id, window_id: g.windowId, collapsed: g.collapsed, color: g.color})),
        own_tab_id: own?.tabId ?? null,
    };
}

const OPERATIONS = {
    async ping() {
        // hasListeners() — фактическое состояние API: глобального перехватчика имён загрузок нет.
        return {protocol: PROTOCOL, version: VERSION, extension_id: chrome.runtime.id, instance: await instanceId(),
            browser: await browserFacts(), own_tab: await tabSummary(await ownTab()),
            listeners: {determining_filename: chrome.downloads.onDeterminingFilename.hasListeners(),
                web_request: chrome.webRequest.onBeforeRequest.hasListeners(), active_observers: observers.size},
            operation: await operationSummary(),
            restored_cleanup: (await chrome.storage.local.get(CLEANUP_KEY))[CLEANUP_KEY] ?? null};
    },
    async tab_status() {
        return await tabSummary(await ownTab());
    },
    async open() {
        const {tab, created, replaced} = await ensureTab();
        return {created, replaced, ...(await tabSummary(tab))};
    },
    async navigate(args, signal) {
        const url = forvoUrl(args.url);
        const timeout = duration(args.timeout_ms, 1000, 60000);
        const {tab} = await ensureTab();
        const done = waitComplete(tab.id, timeout, signal);
        await chrome.tabs.update(tab.id, {url});
        const status = await done;
        return {load: status, ...(await tabSummary(await requireTab()))};
    },
    async auth() {
        const tab = await requireTab();
        return await inject(tab.id, forvoAuth);
    },
    async dom(args) {
        const surface = text(args.surface, 128);
        const action = args.action;
        if (action !== 'inspect' && action !== 'expand') throw new Failure('invalid_arguments');
        const tab = await requireTab();
        return await inject(tab.id, forvoDom, [surface, action, '']);
    },
    async download_begin(args) {
        return await downloadBegin(args);
    },
    async download_status(args) {
        return await downloadStatus(args);
    },
    async download_release(args) {
        return await downloadRelease(args);
    },
    async downloads_api(args, signal) {
        return await downloadsApi(args, signal);
    },
    async download_cleanup(args) {
        return await downloadCleanup(args);
    },
    async snapshot() {
        return await snapshot();
    },
    async close() {
        const tab = await ownTab();
        if (tab) await chrome.tabs.remove(tab.id);
        await chrome.storage.local.remove('own');
        return {closed: !!tab};
    },
    async reload() {
        setTimeout(() => chrome.runtime.reload(), 100);
        return {reloading: true};
    },
};

chrome.runtime.onStartup.addListener(async () => {
    // Tab ID и download ID прошлой сессии браузера не доказывают владения.
    await chrome.storage.local.remove(['own', OP_KEY, API_KEY]);
    connect();
});
chrome.runtime.onInstalled.addListener(() => connect());
chrome.alarms.onAlarm.addListener(alarm => {
    if (alarm.name === 'connect') connect();
});
chrome.alarms.create('connect', {periodInMinutes: 0.5});
connect();
