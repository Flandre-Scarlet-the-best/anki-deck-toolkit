// Тонкий транспорт Forvo для pronunciation-assets. Расширение выполняет только
// типизированные команды локального Rust bridge над собственной вкладкой;
// выбор кандидата, проверка URL, байтов и MP3 остаются в Rust.
import {forvoAuth, forvoDom} from './forvo_page.js';

const PROTOCOL = 'forvo-adapter/v1';
const WS_PATH = '/forvo-adapter/v1';
const GROUP_TITLE = 'Anki Deck Toolkit';
const FORVO_ORIGIN = 'https://forvo.com';
const MAX_INBOUND_CHARS = 64 * 1024;
const MAX_AUDIO_BYTES = 8 * 1024 * 1024;
const KEEPALIVE_MS = 20000;
const RETRY_MS = 3000;
const QUICK_RETRIES = 5;
const HANDSHAKE_MS = 10000;
const OBSERVED_TTL_MS = 120000;
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
// Наблюдённые штатные загрузки: токен → URL. Rust не может передать сюда произвольный адрес.
const observed = new Map();

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
        send({type: 'ready', version: VERSION});
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

function aborted(signal) {
    return new Promise((_, reject) => signal.addEventListener('abort', () => reject(new Failure('aborted')), {once: true}));
}
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

async function ensureTab() {
    const existing = await ownTab();
    if (existing) return {tab: existing, created: false};
    const window = await chrome.windows.getLastFocused({windowTypes: ['normal']});
    const tab = await chrome.tabs.create({windowId: window.id, url: FORVO_ORIGIN + '/', active: false});
    try {
        const groupId = await chrome.tabs.group({tabIds: [tab.id], createProperties: {windowId: window.id}});
        await chrome.tabGroups.update(groupId, {title: GROUP_TITLE, color: 'blue', collapsed: true});
        await chrome.tabs.update(tab.id, {autoDiscardable: false});
        await chrome.storage.local.set({own: {tabId: tab.id, groupId, windowId: window.id}});
        return {tab: await chrome.tabs.get(tab.id), created: true};
    } catch (error) {
        await chrome.tabs.remove(tab.id).catch(() => {});
        throw new Failure('tab_group_failed', String(error?.message || error));
    }
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

// --- Штатная загрузка Forvo ------------------------------------------------

function describeUrl(raw) {
    try {
        const url = new URL(raw);
        return {origin: url.origin, path: url.pathname, has_query: url.search !== '', has_fragment: url.hash !== '', has_credentials: !!(url.username || url.password)};
    } catch {
        return null;
    }
}

// Единый перехватчик штатных загрузок Forvo. Он включается на время операции и
// ещё на GUARD_MS после неё: запоздавшая загрузка сайта иначе дошла бы до диалога
// «Сохранить как», который не закрывается ни отменой, ни закрытием вкладки.
const GUARD_MS = 180000;
const interceptor = {pending: null, guardUntil: 0, since: 0, lateCancelled: 0, foreignSeen: 0, timer: null, installed: false};

function forvoRelated(item) {
    const origin = raw => {
        try {
            return new URL(raw).origin;
        } catch {
            return null;
        }
    };
    return !item.byExtensionId && [item.referrer, item.url, item.finalUrl].some(raw => origin(raw) === FORVO_ORIGIN);
}

function onDetermining(item, suggest) {
    const started = Date.parse(item.startTime || '') || Date.now();
    if (!forvoRelated(item) || started < interceptor.since) {
        // Чужие загрузки: пустой suggest() немедленно, без изменения имени и диалога.
        interceptor.foreignSeen += 1;
        suggest();
        return false;
    }
    const pending = interceptor.pending;
    interceptor.pending = null;
    // Отмена до подтверждения: диалог не открывается, URL взят из реального download flow.
    chrome.downloads.cancel(item.id).catch(() => {}).finally(() => {
        suggest();
        chrome.downloads.erase({id: item.id}).catch(() => {});
        if (pending) pending(item);
        else interceptor.lateCancelled += 1;
    });
    return true;
}

function armInterceptor() {
    clearTimeout(interceptor.timer);
    interceptor.guardUntil = Date.now() + GUARD_MS;
    if (!interceptor.installed) {
        chrome.downloads.onDeterminingFilename.addListener(onDetermining);
        interceptor.installed = true;
    }
}

function releaseInterceptorLater() {
    clearTimeout(interceptor.timer);
    interceptor.timer = setTimeout(() => {
        if (interceptor.pending || Date.now() < interceptor.guardUntil) return releaseInterceptorLater();
        chrome.downloads.onDeterminingFilename.removeListener(onDetermining);
        interceptor.installed = false;
    }, Math.max(1000, interceptor.guardUntil - Date.now()));
}

async function observeSiteDownload(args, signal) {
    const surface = text(args.surface, 128);
    const id = recordId(args.record_id);
    const timeout = duration(args.timeout_ms, 1000, 120000);
    const tab = await requireTab();
    const page = new URL(tab.url);
    if (page.origin !== FORVO_ORIGIN || decodeURIComponent(page.pathname) !== decodeURIComponent(wordPath(surface))) throw new Failure('page_identity_mismatch');
    const pageUrl = tab.url;
    const popups = [];
    const onCreatedTab = created => {
        if (created.openerTabId === tab.id) popups.push(created.id);
    };
    const match = new Promise(resolve => {
        interceptor.pending = resolve;
    });
    interceptor.since = Date.now() - 1000;
    const lateBefore = interceptor.lateCancelled;
    armInterceptor();
    chrome.tabs.onCreated.addListener(onCreatedTab);
    try {
        const activated = await inject(tab.id, forvoDom, [surface, 'activate', id]);
        if (activated?.code !== 'activated') return {code: activated?.code || 'dom_changed', candidates: activated?.candidates || []};
        const timer = delay(timeout).then(() => null);
        const item = await Promise.race([match, timer, aborted(signal)]);
        if (!item) return {code: 'download_not_started', guard_ms: GUARD_MS, late_cancelled_before: interceptor.lateCancelled - lateBefore};
        const token = randomHex();
        observed.set(token, {url: item.finalUrl || item.url, expires: Date.now() + OBSERVED_TTL_MS, tabId: tab.id});
        return {code: 'observed', token, download_id: item.id, url: describeUrl(item.url), final_url: describeUrl(item.finalUrl),
            mime: item.mime || null, referrer_is_own_page: item.referrer === pageUrl,
            referrer_origin_is_forvo: describeUrl(item.referrer)?.origin === FORVO_ORIGIN, cancelled_before_prompt: true,
            new_tabs_from_own_page: popups.length};
    } finally {
        interceptor.pending = null;
        interceptor.guardUntil = Date.now() + GUARD_MS;
        releaseInterceptorLater();
        chrome.tabs.onCreated.removeListener(onCreatedTab);
        for (const popup of popups) await chrome.tabs.remove(popup).catch(() => {});
    }
}

function takeObserved(token) {
    if (typeof token !== 'string' || !/^[0-9a-f]{64}$/.test(token)) throw new Failure('invalid_arguments');
    const entry = observed.get(token);
    if (!entry || entry.expires < Date.now()) {
        observed.delete(token);
        throw new Failure('observation_expired');
    }
    return entry;
}

// Выполняется внутри собственной вкладки forvo.com: тот же origin и сессия,
// cookies не покидают браузер. Возвращаются только байты и безопасные метаданные.
async function pageFetch(url, max) {
    try {
        const response = await fetch(url, {credentials: 'same-origin', cache: 'no-store', redirect: 'follow'});
        const final = new URL(response.url);
        const declared = Number(response.headers.get('content-length') || 0);
        if (declared > max) return {code: 'too_large', status: response.status};
        const bytes = new Uint8Array(await response.arrayBuffer());
        if (bytes.length > max) return {code: 'too_large', status: response.status};
        let binary = '';
        for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
        return {code: 'ok', status: response.status, redirected: response.redirected, final_origin: final.origin, final_path: final.pathname,
            final_has_query: final.search !== '', content_type: response.headers.get('content-type') || null, length: bytes.length, base64: btoa(binary)};
    } catch (error) {
        return {code: 'fetch_failed', name: String(error?.name || 'Error')};
    }
}

async function fetchObserved(args) {
    const entry = takeObserved(args.token);
    const tab = await requireTab();
    if (tab.id !== entry.tabId) throw new Failure('own_tab_changed');
    return await inject(tab.id, pageFetch, [entry.url, MAX_AUDIO_BYTES]);
}

// Эксперимент официального API: та же наблюдённая ссылка через chrome.downloads.
async function downloadsApi(args, signal) {
    const entry = takeObserved(args.token);
    const stem = text(args.filename_stem, 80);
    if (!/^[a-z0-9-]+$/.test(stem)) throw new Failure('invalid_arguments');
    const promptMs = duration(args.prompt_ms, 1000, 60000);
    const timeout = duration(args.timeout_ms, 1000, 120000);
    let created = null;
    const events = [];
    const onCreated = item => {
        if (item.byExtensionId === chrome.runtime.id && item.url === entry.url) events.push('created');
    };
    chrome.downloads.onCreated.addListener(onCreated);
    try {
        created = await chrome.downloads.download({url: entry.url, filename: `AnkiDeckToolkit/${stem}.mp3`, saveAs: false, conflictAction: 'uniquify'});
        const {downloadIds = []} = await chrome.storage.local.get('downloadIds');
        await chrome.storage.local.set({downloadIds: [...downloadIds, created].slice(-32)});
        const started = Date.now();
        for (;;) {
            if (signal.aborted) throw new Failure('aborted');
            const [item] = await chrome.downloads.search({id: created});
            if (!item) return {code: 'download_vanished', download_id: created};
            if (item.state === 'complete') {
                return {code: 'complete', download_id: created, filename: item.filename, file_size: item.fileSize, bytes_received: item.bytesReceived,
                    total_bytes: item.totalBytes, mime: item.mime || null, exists: item.exists, danger: item.danger, final_url: describeUrl(item.finalUrl), events};
            }
            if (item.state === 'interrupted') return {code: 'interrupted', download_id: created, error: item.error || null, events};
            const elapsed = Date.now() - started;
            // Пока имя пусто, Chromium ждёт выбора пути пользователем.
            if (!item.filename && elapsed >= promptMs) {
                await chrome.downloads.cancel(created).catch(() => {});
                await chrome.downloads.erase({id: created}).catch(() => {});
                return {code: 'save_as_prompt_suspected', download_id: created, waited_ms: elapsed, events};
            }
            if (elapsed >= timeout) {
                await chrome.downloads.cancel(created).catch(() => {});
                return {code: 'download_timeout', download_id: created, events};
            }
            await delay(250);
        }
    } finally {
        chrome.downloads.onCreated.removeListener(onCreated);
    }
}

async function downloadCleanup(args) {
    if (!Number.isSafeInteger(args.download_id)) throw new Failure('invalid_arguments');
    const {downloadIds = []} = await chrome.storage.local.get('downloadIds');
    if (!downloadIds.includes(args.download_id)) throw new Failure('download_not_owned');
    const [item] = await chrome.downloads.search({id: args.download_id});
    let removed = false;
    if (item?.state === 'complete' && item.exists) {
        await chrome.downloads.removeFile(args.download_id);
        removed = true;
    }
    await chrome.downloads.erase({id: args.download_id});
    await chrome.storage.local.set({downloadIds: downloadIds.filter(id => id !== args.download_id)});
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
        return {protocol: PROTOCOL, version: VERSION, extension_id: chrome.runtime.id, own_tab: await tabSummary(await ownTab()),
            interceptor: {installed: interceptor.installed, late_cancelled: interceptor.lateCancelled}};
    },
    async tab_status() {
        return await tabSummary(await ownTab());
    },
    async open() {
        const {tab, created} = await ensureTab();
        return {created, ...(await tabSummary(tab))};
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
    async download(args, signal) {
        return await observeSiteDownload(args, signal);
    },
    async fetch_observed(args) {
        return await fetchObserved(args);
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
    // Tab ID прошлой сессии браузера мог достаться чужой вкладке.
    await chrome.storage.local.remove(['own', 'downloadIds']);
    connect();
});
chrome.runtime.onInstalled.addListener(() => connect());
chrome.alarms.onAlarm.addListener(alarm => {
    if (alarm.name === 'connect') connect();
});
chrome.alarms.create('connect', {periodInMinutes: 0.5});
connect();
