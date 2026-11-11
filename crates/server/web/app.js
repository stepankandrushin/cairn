// Cairn's web interface: a thin client of the server's API (docs/protocol.md, "Web"). The server embeds
// this file as it is, with no build step. Nothing the server sends is ever parsed as HTML: every name and
// message reaches the page as text.

const PART_BYTES = 16 * 1024 * 1024;
const TEXT_PREVIEW_BYTES = 256 * 1024;
const LEVELS = ['read', 'write', 'manage'];
const LEVEL_TEXT = { read: 'Can view', write: 'Can edit', manage: 'Can manage' };
const IS_PUBLIC = location.pathname.startsWith('/s/');
const enc = encodeURIComponent;

// How this browser shows times (Settings), kept in its localStorage: the clock, and the order of a date.
const SETTINGS_KEY = 'cairn.settings';
const CLOCKS = ['24h', '12h'];
const DATE_ORDERS = ['yyyy-mm-dd', 'dd.mm.yyyy', 'dd/mm/yyyy', 'mm/dd/yyyy', 'd mmm yyyy', 'mmm d, yyyy'];

/** What was chosen here, each setting the first of its list where nothing (or nothing known) was. */
function loadSettings() {
  let saved = null;
  try { saved = JSON.parse(localStorage.getItem(SETTINGS_KEY)); } catch { saved = null; }
  const s = saved && typeof saved === 'object' ? saved : {};
  return {
    clock: CLOCKS.includes(s.clock) ? s.clock : CLOCKS[0],
    date: DATE_ORDERS.includes(s.date) ? s.date : DATE_ORDERS[0],
  };
}

const state = {
  me: null, // GET /v1/me
  shared: null, // GET /v1/shared, once asked
  users: null, // GET /v1/users, once asked
  current: null, // {view, listing} of the folder on screen
  openFolder: null, // the folder whose own pages the sidebar lists
  sort: { key: 'name', dir: 1 },
  gen: 0, // the navigation a render belongs to: a stale one never paints
  routing: false,
  publicView: null,
  settings: loadSettings(),
};
let shell = null;

// ---------------------------------------------------------------- errors

class ApiError extends Error {
  constructor(status, text) {
    super(text || (status ? `The server answered ${status}.` : 'The server could not be reached.'));
    this.status = status;
  }
}
class UserError extends Error {}
class Cancelled extends Error {
  constructor() { super('Cancelled'); }
}

function report(e) {
  if (e instanceof Cancelled) return;
  if (e && e.status === 401 && !IS_PUBLIC) return; // the sign-in screen says it
  console.error(e);
  toast(e && e.message ? e.message : String(e), { error: true });
}

/** An event handler whose failure is shown, never dropped. */
const act = (fn) => async (ev) => {
  try { await fn(ev); } catch (e) { report(e); }
};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// ---------------------------------------------------------------- DOM

const PROPS = new Set(['value', 'checked', 'disabled', 'selected', 'indeterminate', 'hidden', 'multiple', 'readOnly']);

/** An element: `attrs` are attributes, `text`, `class`, `on<event>` handlers and a few properties. */
function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (PROPS.has(k)) el[k] = v;
    else el.setAttribute(k, v === true ? '' : String(v));
  }
  return add(el, children);
}

function add(el, children) {
  for (const c of [children].flat(Infinity)) {
    if (c === null || c === undefined || c === false || c === '') continue;
    el.append(c instanceof Node ? c : String(c));
  }
  return el;
}

const $app = () => document.getElementById('app');

const SVG_NS = 'http://www.w3.org/2000/svg';
const ICONS = {
  folder: ['M3 7.5A2.5 2.5 0 0 1 5.5 5h3.8l2.2 2.2h7A2.5 2.5 0 0 1 21 9.7v7.8a2.5 2.5 0 0 1-2.5 2.5h-13A2.5 2.5 0 0 1 3 17.5z'],
  'folder-plus': ['M3 7.5A2.5 2.5 0 0 1 5.5 5h3.8l2.2 2.2h7A2.5 2.5 0 0 1 21 9.7v7.8a2.5 2.5 0 0 1-2.5 2.5h-13A2.5 2.5 0 0 1 3 17.5z', 'M12 10.5v6M9 13.5h6'],
  'folder-upload': ['M3 7.5A2.5 2.5 0 0 1 5.5 5h3.8l2.2 2.2h7A2.5 2.5 0 0 1 21 9.7v7.8a2.5 2.5 0 0 1-2.5 2.5h-13A2.5 2.5 0 0 1 3 17.5z', 'M12 17v-6.3M9.4 13.2l2.6-2.6 2.6 2.6'],
  file: ['M7 3h7l5 5v12a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z', 'M14 3v5h5'],
  link: ['M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1', 'M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1'],
  menu: ['M4 7h16M4 12h16M4 17h16'],
  more: ['M5.5 12h.01M12 12h.01M18.5 12h.01'],
  upload: ['M12 16V4M7 9l5-5 5 5M5 20h14'],
  download: ['M12 4v12M7 11l5 5 5-5M5 20h14'],
  share: ['M9 11a3.2 3.2 0 1 0 0-6.4A3.2 3.2 0 0 0 9 11z', 'M3 20a6 6 0 0 1 12 0', 'M16 4.8a3.2 3.2 0 0 1 0 6.2', 'M21 20a6 6 0 0 0-3.6-5.5'],
  trash: ['M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3'],
  history: ['M3.5 12a8.5 8.5 0 1 0 2.6-6.1L3.5 8.5', 'M3.5 3.5v5h5', 'M12 7.5V12l3 2'],
  close: ['M6 6l12 12M18 6L6 18'],
  chevron: ['M6 9l6 6 6-6'],
  people: ['M9 11a3.2 3.2 0 1 0 0-6.4A3.2 3.2 0 0 0 9 11z', 'M3 20a6 6 0 0 1 12 0', 'M16 4.8a3.2 3.2 0 0 1 0 6.2', 'M21 20a6 6 0 0 0-3.6-5.5'],
  settings: ['M10.29 5.62L10.4 2.94L13.6 2.94L13.71 5.62A6.6 6.6 0 0 1 15.3 6.28L17.28 4.46L19.54 6.72L17.72 8.7A6.6 6.6 0 0 1 18.38 10.29L21.06 10.4L21.06 13.6L18.38 13.71A6.6 6.6 0 0 1 17.72 15.3L19.54 17.28L17.28 19.54L15.3 17.72A6.6 6.6 0 0 1 13.71 18.38L13.6 21.06L10.4 21.06L10.29 18.38A6.6 6.6 0 0 1 8.7 17.72L6.72 19.54L4.46 17.28L6.28 15.3A6.6 6.6 0 0 1 5.62 13.71L2.94 13.6L2.94 10.4L5.62 10.29A6.6 6.6 0 0 1 6.28 8.7L4.46 6.72L6.72 4.46L8.7 6.28A6.6 6.6 0 0 1 10.29 5.62z', 'M12 14.6a2.6 2.6 0 1 0 0-5.2 2.6 2.6 0 0 0 0 5.2z'],
  shared: ['M18 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6z', 'M6 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z', 'M18 22a3 3 0 1 0 0-6 3 3 0 0 0 0 6z', 'M8.6 10.5l6.8-4M8.6 13.5l6.8 4'],
  'sign-out': ['M10 4H6.5A2.5 2.5 0 0 0 4 6.5v11A2.5 2.5 0 0 0 6.5 20H10', 'M15 8l4 4-4 4M19 12H9'],
  globe: ['M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z', 'M3 12h18', 'M12 3c2.5 2.6 3.7 5.6 3.7 9s-1.2 6.4-3.7 9c-2.5-2.6-3.7-5.6-3.7-9S9.5 5.6 12 3z'],
};

function icon(name, cls = '') {
  const s = document.createElementNS(SVG_NS, 'svg');
  s.setAttribute('viewBox', '0 0 24 24');
  s.setAttribute('class', `icon icon-${name} ${cls}`.trim());
  s.setAttribute('aria-hidden', 'true');
  for (const d of ICONS[name]) {
    const p = document.createElementNS(SVG_NS, 'path');
    p.setAttribute('d', d);
    s.append(p);
  }
  return s;
}

const TYPES = {
  image: 'png jpg jpeg jpe gif webp avif bmp svg ico apng',
  video: 'mp4 m4v webm mov ogv',
  audio: 'mp3 wav ogg oga m4a aac flac opus weba',
  pdf: 'pdf',
  text: 'txt text md markdown rst json jsonl ndjson js mjs cjs ts tsx jsx rs py rb go c h cc cpp hpp cxx java kt kts swift sh bash zsh fish ps1 bat cmd toml yaml yml ini cfg conf log csv tsv xml html htm css scss less sql lua pl php r m mm gradle properties env tex srt vtt diff patch lock gitignore gitattributes editorconfig',
  archive: 'zip tar gz tgz bz2 xz zst 7z rar dmg iso',
};
const EXT_TYPE = new Map();
for (const [type, list] of Object.entries(TYPES)) for (const ext of list.split(' ')) EXT_TYPE.set(ext, type);
const TEXT_NAMES = new Set(['makefile', 'dockerfile', 'readme', 'license', 'copying', 'changelog', 'authors']);

/** What a file is, by its name: what kind of preview it gets and its icon's colour. */
function typeOf(display) {
  const lower = display.toLowerCase();
  const dot = lower.lastIndexOf('.');
  const ext = dot >= 0 ? lower.slice(dot + 1) : '';
  return EXT_TYPE.get(ext) || (TEXT_NAMES.has(lower) ? 'text' : 'other');
}

function fileIcon(item, cls = '') {
  if (item.kind === 'dir') return icon('folder', `t-folder ${cls}`);
  if (item.kind === 'symlink') return icon('link', `t-link ${cls}`);
  return icon('file', `t-${typeOf(item.display)} ${cls}`);
}

function loading() {
  return h('div', { class: 'loading', role: 'status' }, h('span', { class: 'spinner' }), 'Loading…');
}

function errorBox(e) {
  const head = e.status === 404 ? 'Not found' : e.status === 403 ? 'Not allowed' : 'Something went wrong';
  return h('div', { class: 'error-box', role: 'alert' },
    h('strong', { text: head }),
    h('p', { text: e.message || String(e) }),
    h('button', { class: 'btn small', onclick: () => refresh() }, 'Try again'));
}

// ---------------------------------------------------------------- formatting

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

function humanSize(n) {
  if (n === null || n === undefined) return '';
  if (n < 1024) return `${n} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  let v = n;
  let i = -1;
  do { v /= 1024; i++; } while (v >= 1024 && i < units.length - 1);
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

// Times are written here, on the clock and in the date order chosen in Settings, rather than by the
// browser's locale: a 24-hour clock, as the tray's, and year-month-day unless changed.
const pad2 = (n) => String(n).padStart(2, '0');
const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** A day in one of DATE_ORDERS. */
function dayText(d, order = state.settings.date) {
  const dd = pad2(d.getDate());
  const mm = pad2(d.getMonth() + 1);
  const y = d.getFullYear();
  switch (order) {
    case 'dd.mm.yyyy': return `${dd}.${mm}.${y}`;
    case 'dd/mm/yyyy': return `${dd}/${mm}/${y}`;
    case 'mm/dd/yyyy': return `${mm}/${dd}/${y}`;
    case 'd mmm yyyy': return `${d.getDate()} ${MONTHS[d.getMonth()]} ${y}`;
    case 'mmm d, yyyy': return `${MONTHS[d.getMonth()]} ${d.getDate()}, ${y}`;
    default: return `${y}-${mm}-${dd}`;
  }
}

/** `16:52`, or `4:52 PM` on a 12-hour clock. */
function clockText(d, clock = state.settings.clock) {
  const m = pad2(d.getMinutes());
  if (clock === '12h') return `${d.getHours() % 12 || 12}:${m} ${d.getHours() < 12 ? 'AM' : 'PM'}`;
  return `${pad2(d.getHours())}:${m}`;
}

/** A time: `{sec, nsec}` as the protocol sends one, or seconds. One no date can hold shows as its seconds. */
function fmtTime(t) {
  const sec = typeof t === 'number' ? t : t && t.sec;
  if (sec === null || sec === undefined) return '';
  const d = new Date(sec * 1000);
  return Number.isNaN(d.getTime()) ? String(sec) : `${dayText(d)} ${clockText(d)}`;
}

function fmtDate(sec) {
  if (sec === null || sec === undefined) return '';
  const d = new Date(sec * 1000);
  return Number.isNaN(d.getTime()) ? String(sec) : dayText(d);
}

function modeString(kind, mode) {
  if (mode === null || mode === undefined) return '';
  let s = kind === 'dir' ? 'd' : kind === 'symlink' ? 'l' : '-';
  const triples = [[0o400, 0o200, 0o100, 0o4000, 's'], [0o40, 0o20, 0o10, 0o2000, 's'], [0o4, 0o2, 0o1, 0o1000, 't']];
  for (const [r, w, x, special, ch] of triples) {
    s += mode & r ? 'r' : '-';
    s += mode & w ? 'w' : '-';
    if (mode & special) s += mode & x ? ch : ch.toUpperCase();
    else s += mode & x ? 'x' : '-';
  }
  return `${s}  ${(mode & 0o7777).toString(8).padStart(4, '0')}`;
}

/** A path as the server gives one: text already, or steps `{display}`. */
function pathText(p) {
  if (p === null || p === undefined) return '';
  if (typeof p === 'string') return p;
  if (Array.isArray(p)) return p.map((s) => (typeof s === 'string' ? s : s.display ?? '')).join(' / ');
  return String(p);
}

/** Text that may break after each `/`, as a path's steps run together: text and `<wbr>`s. */
const slashBreaks = (text) => text.split(/(?<=\/)/).flatMap((part, i) => (i ? [h('wbr'), part] : [part]));

const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;
const byText = (user, device) => (user && device ? `${user} on ${device}` : user || device || '—');
const atLeast = (have, want) => LEVELS.indexOf(have) >= LEVELS.indexOf(want);

function today() {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

/** The end of a day picked in a date input, local time, in seconds: `{expires}` or nothing. */
function expiryOf(input) {
  if (!input.value) return {};
  const [y, m, d] = input.value.split('-').map(Number);
  return { expires: Math.floor(new Date(y, m - 1, d, 23, 59, 59).getTime() / 1000) };
}

// ---------------------------------------------------------------- names

const utf8 = new TextEncoder();

function toBase64(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(s);
}

/** Why a name cannot be one, or null. The server refuses the same, and `.cairn` in a folder's root. */
function nameProblem(name) {
  if (!name) return 'A name cannot be empty.';
  if (name.includes('/')) return 'A name cannot hold “/”.';
  if (name.includes('\0')) return 'A name cannot hold a NUL character.';
  if (name === '.' || name === '..') return `“${name}” cannot be a name.`;
  if (utf8.encode(name).length > 1024) return 'That name is longer than 1024 bytes.';
  return null;
}

/** A name as the wire takes it: NFC, UTF-8, base64. */
function wireName(name) {
  const n = name.normalize('NFC');
  const problem = nameProblem(n);
  if (problem) throw new UserError(problem);
  return toBase64(utf8.encode(n));
}

// ---------------------------------------------------------------- API

async function request(method, path, { json, body, headers = {} } = {}) {
  const hdrs = { ...headers };
  if (method !== 'GET' && method !== 'HEAD') hdrs['X-Cairn-Web'] = '1';
  let payload = body;
  if (json !== undefined) {
    hdrs['Content-Type'] = 'application/json';
    payload = JSON.stringify(json);
  }
  let r;
  try {
    r = await fetch(path, { method, headers: hdrs, body: payload, credentials: 'same-origin', cache: 'no-store' });
  } catch (e) {
    throw new ApiError(0, `The server could not be reached (${e.message}).`);
  }
  if (!r.ok) {
    const text = (await r.text().catch(() => '')).trim();
    if (r.status === 401 && !IS_PUBLIC && state.me) signedOut();
    throw new ApiError(r.status, text);
  }
  if (r.status === 204) return null;
  const text = await r.text();
  if (!text) return null;
  if ((r.headers.get('Content-Type') || '').includes('json')) return JSON.parse(text);
  try { return JSON.parse(text); } catch { return text; }
}

const api = {
  get: async (p) => ownTopName(p, await request('GET', p)),
  post: (p, json) => request('POST', p, json === undefined ? {} : { json }),
  put: (p, json) => request('PUT', p, { json }),
  del: (p) => request('DELETE', p),
};

async function getUsers() {
  const r = await api.get('/v1/users');
  state.users = (r && r.users) || [];
  return state.users;
}

const folderAccess = (ns) => state.me && state.me.folders.find((f) => f.folder === ns)?.access;
/** What the user calls a folder: their own name for it, or else the folder's name. */
const folderName = (ns) => (state.me && state.me.folders.find((f) => f.folder === ns)?.name) || ns;

/**
 * A folder's top, which the server's answers give by the folder's name, shown by the user's own name
 * for it: the listing's or item's own, where it is the top, and the first step of its path.
 */
function ownTopName(path, data) {
  const m = /^\/v1\/ns\/([^/?]+)\/(dir|item)\//.exec(path);
  if (!m || !data || typeof data !== 'object') return data;
  let ns;
  try { ns = decodeURIComponent(m[1]); } catch { return data; }
  const name = folderName(ns);
  if (name === ns) return data;
  const top = (x) => x && typeof x === 'object' && (x.name === undefined || x.name === null);
  if (top(data.dir)) data.dir.display = name;
  if (top(data.item)) data.item.display = name;
  if (Array.isArray(data.path) && data.path[0]) data.path[0].display = name;
  return data;
}

// ---------------------------------------------------------------- views

// A view is one of the three bases of docs/protocol.md: a folder of the user's, a share to the user,
// or a public link. The browser works the same in each, with what its level allows.
function nsView(ns) {
  return { kind: 'ns', ns, base: `/v1/ns/${enc(ns)}`, link: (o) => `#/f/${enc(ns)}/${o}` };
}
function sharedView(id) {
  return { kind: 'shared', id, base: `/v1/shared/${enc(id)}`, link: (o) => `#/s/${enc(id)}/${o}` };
}
function linkView(token) {
  return { kind: 'link', token, base: `/v1/links/${enc(token)}`, link: (o) => `#/o/${o}` };
}

function contentUrl(view, object, { revision, download } = {}) {
  const q = new URLSearchParams();
  if (revision) q.set('revision', revision);
  if (download) q.set('download', '1');
  const s = q.toString();
  return `${view.base}/content/${enc(object)}${s ? `?${s}` : ''}`;
}
const zipUrl = (view, object) => `${view.base}/zip/${enc(object)}`;

function download(url) {
  const a = h('a', { href: url, download: '', hidden: true });
  document.body.append(a);
  a.click();
  a.remove();
}

async function downloadMany(view, items) {
  for (const e of items) {
    if (e.kind === 'file') download(contentUrl(view, e.object, { download: true }));
    else if (e.kind === 'dir') download(zipUrl(view, e.object));
    await sleep(400);
  }
}

// ---------------------------------------------------------------- toasts, menus, dialogs

/** Toasts go into the top dialog when one is open, as a modal dialog covers everything else. */
function toastHost() {
  const dialogs = [...document.querySelectorAll('dialog[open]')];
  const parent = dialogs.length ? dialogs[dialogs.length - 1] : document.body;
  let host = [...parent.children].find((c) => c.classList.contains('toasts'));
  if (!host) {
    host = h('div', { class: 'toasts', 'aria-live': 'polite' });
    parent.append(host);
  }
  return host;
}

function toast(message, { error = false, action = null, timeout } = {}) {
  const el = h('div', { class: `toast${error ? ' error' : ''}`, role: error ? 'alert' : 'status' });
  const remove = () => el.remove();
  add(el, [
    h('span', { class: 'toast-text', text: message }),
    action && h('button', { class: 'btn subtle small', onclick: () => { remove(); act(action.run)(); } }, action.label),
    h('button', { class: 'icon-btn', 'aria-label': 'Dismiss', onclick: remove }, icon('close')),
  ]);
  toastHost().append(el);
  const ms = timeout ?? (error ? 15000 : action ? 10000 : 4500);
  if (ms) setTimeout(remove, ms);
  return el;
}

let openMenu = null;
function closeMenu() {
  if (!openMenu) return;
  openMenu.remove();
  openMenu = null;
  document.removeEventListener('mousedown', menuOutside, true);
  document.removeEventListener('keydown', menuKey, true);
}
function menuOutside(ev) { if (openMenu && !openMenu.contains(ev.target)) closeMenu(); }
function menuKey(ev) {
  if (!openMenu) return;
  if (ev.key === 'Escape') { ev.preventDefault(); closeMenu(); return; }
  if (ev.key === 'ArrowDown' || ev.key === 'ArrowUp') {
    ev.preventDefault();
    const items = [...openMenu.querySelectorAll('.menu-item')];
    const i = items.indexOf(document.activeElement);
    const next = ev.key === 'ArrowDown' ? (i + 1) % items.length : (i - 1 + items.length) % items.length;
    items[next].focus();
  }
}

/** A menu at an element (below it, right-aligned) or at a mouse event. Items: [label, run, class] or '-'. */
function popupMenu(at, items) {
  closeMenu();
  const list = items.filter(Boolean);
  while (list[0] === '-') list.shift();
  while (list[list.length - 1] === '-') list.pop();
  if (!list.length) return;
  const menu = h('div', { class: 'menu', role: 'menu' }, list.map((it) => (it === '-'
    ? h('div', { class: 'menu-sep', role: 'separator' })
    : h('button', { class: `menu-item${it[2] ? ` ${it[2]}` : ''}`, role: 'menuitem', onclick: () => { closeMenu(); act(it[1])(); } }, it[0]))));
  document.body.append(menu);
  const mw = menu.offsetWidth;
  const mh = menu.offsetHeight;
  let x;
  let y;
  if (at instanceof Element) {
    const r = at.getBoundingClientRect();
    x = r.right - mw;
    y = r.bottom + 4;
    if (y + mh > innerHeight - 8) y = r.top - mh - 4;
  } else {
    x = at.clientX;
    y = at.clientY;
    if (y + mh > innerHeight - 8) y -= mh;
  }
  menu.style.left = `${Math.max(8, Math.min(x, innerWidth - mw - 8))}px`;
  menu.style.top = `${Math.max(8, y)}px`;
  openMenu = menu;
  document.addEventListener('mousedown', menuOutside, true);
  document.addEventListener('keydown', menuKey, true);
  menu.querySelector('.menu-item')?.focus();
}

/** A modal dialog, removed once closed. Clicking outside it or Escape closes it. */
function dialog({ title, body, actions = [], wide = false, onClose }) {
  const dlg = h('dialog', { class: `dlg${wide ? ' wide' : ''}` });
  const close = () => { if (dlg.open) dlg.close(); };
  const heading = h('h2', { text: title });
  const bodyEl = h('div', { class: 'dlg-body' }, body);
  const shown = [actions].flat().filter(Boolean);
  const foot = h('div', { class: 'dlg-foot' }, shown);
  add(dlg, h('div', { class: 'dlg-inner' },
    h('div', { class: 'dlg-head' }, heading, h('button', { class: 'icon-btn', type: 'button', 'aria-label': 'Close', onclick: close }, icon('close'))),
    bodyEl,
    shown.length ? foot : null));
  dlg.addEventListener('mousedown', (ev) => { dlg.downOnBackdrop = ev.target === dlg; });
  dlg.addEventListener('click', (ev) => { if (ev.target === dlg && dlg.downOnBackdrop) close(); });
  dlg.addEventListener('close', () => { dlg.remove(); if (onClose) onClose(); });
  document.body.append(dlg);
  dlg.showModal();
  return { dlg, close, body: bodyEl, foot, setTitle: (t) => { heading.textContent = t; } };
}

function confirmDialog(title, message, { ok = 'OK', danger = false } = {}) {
  return new Promise((resolve) => {
    let answer = false;
    const okButton = h('button', { class: `btn ${danger ? 'danger-solid' : 'primary'}`, onclick: () => { answer = true; d.close(); } }, ok);
    const d = dialog({
      title,
      body: h('p', { text: message }),
      actions: [h('button', { class: 'btn', onclick: () => d.close() }, 'Cancel'), okButton],
      onClose: () => resolve(answer),
    });
    okButton.focus();
  });
}

/** Asks for a name; null if cancelled. What it answers is NFC and a name the server takes. */
function promptName(title, initial, okLabel) {
  return new Promise((resolve) => {
    let answer = null;
    const input = h('input', { type: 'text', class: 'text-input', value: initial, spellcheck: 'false', autocomplete: 'off', 'aria-label': 'Name' });
    const err = h('p', { class: 'form-error', role: 'alert', hidden: true });
    const submit = (ev) => {
      ev.preventDefault();
      const v = input.value.normalize('NFC');
      const problem = nameProblem(v);
      if (problem) { err.textContent = problem; err.hidden = false; return; }
      answer = v;
      d.close();
    };
    const d = dialog({
      title,
      body: h('form', { onsubmit: submit }, input, err),
      actions: [h('button', { class: 'btn', onclick: () => d.close() }, 'Cancel'), h('button', { class: 'btn primary', onclick: submit }, okLabel)],
      onClose: () => resolve(answer),
    });
    input.focus();
    const dot = initial.lastIndexOf('.');
    input.setSelectionRange(0, dot > 0 ? dot : initial.length);
  });
}

// ---------------------------------------------------------------- sign-in and the shell

function brand(link = true) {
  return h(link ? 'a' : 'span', { class: 'brand', href: link ? '#/' : undefined },
    h('img', { src: '/web/icon.svg', alt: '' }), 'Cairn');
}

function showSignIn(message) {
  closeMenu();
  state.me = null;
  state.current = null;
  state.openFolder = null;
  shell = null;
  document.title = 'Cairn';
  const input = h('input', { type: 'password', id: 'token', name: 'token', autocomplete: 'current-password', required: true, spellcheck: 'false', autocapitalize: 'off' });
  const err = h('p', { class: 'form-error', role: 'alert', hidden: !message, text: message || '' });
  const button = h('button', { class: 'btn primary wide', type: 'submit' }, 'Sign in');
  const form = h('form', {
    class: 'signin-form',
    onsubmit: async (ev) => {
      ev.preventDefault();
      button.disabled = true;
      err.hidden = true;
      try {
        let r;
        try {
          r = await fetch('/v1/session', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', 'X-Cairn-Web': '1' },
            body: JSON.stringify({ token: input.value.trim() }),
            credentials: 'same-origin',
          });
        } catch (e) {
          throw new ApiError(0, `The server could not be reached (${e.message}).`);
        }
        if (r.status === 401) throw new UserError('That token is not one this server knows.');
        if (!r.ok) throw new ApiError(r.status, (await r.text()).trim());
        input.value = '';
        await start();
      } catch (e) {
        err.textContent = e.message;
        err.hidden = false;
      } finally {
        button.disabled = false;
      }
    },
  }, h('label', { for: 'token', text: 'Token' }), input, err, button);
  $app().replaceChildren(h('div', { class: 'signin' }, h('div', { class: 'signin-card' },
    brand(false),
    h('h1', { text: 'Sign in' }),
    form,
    h('p', { class: 'hint' }, 'Opened from Cairn on a computer that syncs (its Open in browser, or ', h('code', { text: 'cairn web' }), ' in a synced folder), this page signs in by itself.'),
    h('p', { class: 'hint' }, 'Or sign in with a token: ', h('code', { text: 'cairn-server token add <user> --label browser' }), ' on the server makes one.'))));
  input.focus();
}

function signedOut() {
  if (document.querySelector('.signin')) return;
  showSignIn('You are signed out: sign in again to go on.');
}

async function signOut() {
  if (uploads.busy() && !(await confirmDialog('Sign out', 'Uploads are still running. Signing out stops them; what has not finished is not kept.', { ok: 'Sign out', danger: true }))) return;
  uploads.forget();
  await request('DELETE', '/v1/session').catch(() => {});
  // The next to sign in here starts at their own folders, not at the last one's page.
  history.replaceState(null, '', location.pathname);
  showSignIn();
}

function buildShell() {
  const sidebar = h('nav', { class: 'sidebar', 'aria-label': 'Folders' });
  const main = h('main', { class: 'main', id: 'main', tabindex: '-1' });
  const layout = h('div', { class: 'layout' }, sidebar, main);
  const top = h('header', { class: 'topbar' },
    h('button', { class: 'icon-btn menu-btn', 'aria-label': 'Folders', onclick: () => layout.classList.toggle('nav-open') }, icon('menu')),
    brand(),
    h('span', { class: 'spacer' }),
    h('span', { class: 'who', text: state.me.user, title: 'Signed in as this user' }),
    h('a', { class: 'btn subtle settings-link', href: '#/settings', 'aria-label': 'Settings' }, icon('settings'), h('span', { class: 'btn-text', text: 'Settings' })),
    h('button', { class: 'btn subtle', onclick: act(signOut) }, icon('sign-out'), 'Sign out'));
  $app().replaceChildren(top, layout);
  shell = { sidebar, main, layout };
}

function navLink(href, children, active, sub = false) {
  return h('a', { class: `nav-link${sub ? ' sub' : ''}${active ? ' active' : ''}`, href, 'aria-current': active ? 'page' : undefined }, children);
}

function renderSidebar(r) {
  // A folder's own pages stay listed under it while a page of no folder is open (Shared with me,
  // Settings), until another folder is opened; before any is, under the first.
  if (r.ns) state.openFolder = r.ns;
  const folders = state.me.folders;
  const open = folders.some((f) => f.folder === state.openFolder) ? state.openFolder : folders[0]?.folder;
  const items = [h('div', { class: 'nav-title', text: 'Folders' })];
  if (!folders.length) items.push(h('div', { class: 'nav-empty', text: 'No folder is yours yet' }));
  for (const f of folders) {
    const here = r.ns === f.folder;
    items.push(navLink(`#/f/${enc(f.folder)}/root`, [
      icon('folder', 't-folder'), h('span', { class: 'nav-label', text: folderName(f.folder), title: f.name && f.name !== f.folder ? `Your name for the folder ${f.folder}` : undefined }), h('span', { class: `badge ${f.access}`, text: f.access }),
    ], here && r.name === 'files'));
    if (f.folder === open) {
      const sub = (page, glyph, text) => navLink(`#/${page}/${enc(f.folder)}`, [icon(glyph), h('span', { class: 'nav-label', text })], here && r.name === page, true);
      items.push(sub('deleted', 'trash', 'Recently deleted'));
      items.push(sub('members', 'people', 'Members'));
      if (f.access === 'manage') items.push(sub('shares', 'link', 'Shares and links'));
    }
  }
  items.push(h('div', { class: 'nav-title', text: 'Shared' }));
  items.push(navLink('#/shared', [icon('shared'), h('span', { class: 'nav-label', text: 'Shared with me' })], r.name === 'shared' || r.name === 'sharedview'));
  shell.sidebar.replaceChildren(...items);
}

/**
 * Opened by a program with a token, as `#/signin/<ticket>` and the route to show: the ticket leaves the
 * address before anything else runs, and signs this browser in once. What went wrong, if anything.
 */
async function signInByTicket() {
  const m = location.hash.match(/^#\/signin\/([A-Za-z0-9_-]+)(\/.*)?$/);
  if (!m) return null;
  history.replaceState(null, '', location.pathname + (m[2] ? `#${m[2]}` : ''));
  state.signingIn = true;
  try {
    const r = await fetch('/v1/session', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-Cairn-Web': '1' },
      body: JSON.stringify({ ticket: m[1] }),
      credentials: 'same-origin',
    });
    if (r.ok) return null;
    const text = (await r.text()).trim();
    return text ? text.charAt(0).toUpperCase() + text.slice(1) + '.' : `The server answered ${r.status}.`;
  } catch (e) {
    return `The server could not be reached (${e.message}).`;
  } finally {
    state.signingIn = false;
  }
}

async function start(message) {
  try {
    state.me = await api.get('/v1/me');
  } catch (e) {
    if (e.status === 401) { showSignIn(message); return; }
    $app().replaceChildren(h('div', { class: 'signin' }, errorBox(e)));
    return;
  }
  state.shared = null;
  state.users = null;
  buildShell();
  if (!state.routing) {
    window.addEventListener('hashchange', () => route());
    state.routing = true;
  }
  route();
}

function parseRoute() {
  const parts = location.hash.replace(/^#\/?/, '').split('/').map((s) => {
    try { return decodeURIComponent(s); } catch { return s; }
  });
  const [name, a, b] = parts;
  const obj = /^([0-9a-f]{32}|root)$/.test(b || '') ? b : 'root';
  switch (name) {
    case 'f': return a ? { name: 'files', ns: a, obj } : { name: 'home' };
    case 's': return a ? { name: 'sharedview', id: a, obj } : { name: 'shared' };
    case 'deleted':
    case 'members':
    case 'shares': return a ? { name, ns: a } : { name: 'home' };
    case 'shared': return { name: 'shared' };
    case 'settings': return { name: 'settings' };
    default: return { name: 'home' };
  }
}

/** Paints `el` as the page, unless another navigation has begun since `gen`. */
function show(gen, el, title) {
  if (gen !== state.gen || !shell) return false;
  shell.main.replaceChildren(el);
  document.title = title ? `${title} — Cairn` : 'Cairn';
  return true;
}

async function route() {
  if (IS_PUBLIC) return routePublic();
  if (!shell || !state.me || state.signingIn) return;
  closeMenu();
  const r = parseRoute();
  if (r.name === 'home') {
    const first = state.me.folders[0];
    location.replace(first ? `#/f/${enc(first.folder)}/root` : '#/shared');
    return;
  }
  // An address by a folder's former name, as made before it was renamed: the same page by its name now.
  if (r.ns && !folderAccess(r.ns)) {
    const now = await api.get(`/v1/ns/${enc(r.ns)}/info`).then((i) => i && i.folder, () => null);
    if (now && now !== r.ns && folderAccess(now) && parseRoute().ns === r.ns) {
      const parts = location.hash.split('/');
      parts[2] = enc(now);
      location.replace(parts.join('/'));
      return;
    }
  }
  shell.layout.classList.remove('nav-open');
  renderSidebar(r);
  const gen = ++state.gen;
  if (r.name !== 'files' && r.name !== 'sharedview') state.current = null;
  const slow = setTimeout(() => { if (gen === state.gen && shell) shell.main.replaceChildren(loading()); }, 250);
  try {
    switch (r.name) {
      case 'files': await renderBrowser(gen, nsView(r.ns), r.obj); break;
      case 'sharedview': await renderShared(gen, r.id, r.obj); break;
      case 'deleted': await renderDeleted(gen, r.ns); break;
      case 'members': await renderMembers(gen, r.ns); break;
      case 'shares': await renderShares(gen, r.ns); break;
      case 'shared': await renderSharedList(gen); break;
      case 'settings': renderSettings(gen); break;
      default: break;
    }
  } catch (e) {
    if (e.status === 401) return;
    show(gen, errorBox(e), 'Error');
  } finally {
    clearTimeout(slow);
  }
}

function refresh() { return route(); }

let refreshTimer = null;
/** Refreshes the folder on screen soon, if it is one of this view's. */
function refreshSoon(view) {
  if (!state.current || state.current.view.base !== view.base) return;
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => refresh(), 400);
}

// ---------------------------------------------------------------- the browser

async function renderBrowser(gen, view, obj) {
  const listing = await api.get(`${view.base}/dir/${enc(obj)}`);
  if (gen !== state.gen) return;
  const same = state.current && state.current.view.base === view.base && state.current.listing.dir.object === listing.dir.object;
  const scroll = same && shell ? shell.main.scrollTop : 0;
  state.current = { view, listing };
  if (show(gen, browser(view, listing), listing.dir.display)) shell.main.scrollTop = scroll;
}

async function renderShared(gen, id, obj) {
  const view = sharedView(id);
  if (obj === 'root') {
    const info = await api.get(`${view.base}/item/root`);
    if (info.item.kind !== 'dir') {
      state.current = null;
      show(gen, fileView(view, info.item, info.path, info.access), info.item.display);
      return;
    }
  }
  await renderBrowser(gen, view, obj);
}

const keyOf = (e) => `${e.object}:${e.name ?? ''}`;

function browser(view, listing) {
  const access = listing.access || 'read';
  const ctx = {
    view,
    listing,
    access,
    canWrite: atLeast(access, 'write'),
    canShare: view.kind === 'ns' && access === 'manage',
    selected: new Set(),
    entries: listing.entries || [],
  };
  const dir = listing.dir;
  const head = h('div', { class: 'browser-head' },
    breadcrumbs(view, listing.path),
    h('div', { class: 'actions' },
      ctx.canWrite && h('button', { class: 'btn primary', onclick: () => pickFiles(ctx, false) }, icon('upload'), 'Upload'),
      ctx.canWrite && h('button', { class: 'btn', onclick: () => pickFiles(ctx, true) }, icon('folder-upload'), 'Upload folder'),
      ctx.canWrite && h('button', { class: 'btn', onclick: act(() => newFolder(ctx)) }, icon('folder-plus'), 'New folder'),
      h('button', { class: 'btn', title: 'Download this folder and everything in it as a zip', onclick: () => download(zipUrl(view, dir.object)) },
        icon('download'), view.kind === 'link' ? 'Download all (zip)' : 'Download (zip)'),
      ctx.canShare && h('button', { class: 'btn', onclick: () => openShareDialog(view.ns, dir) }, icon('share'), 'Share')));
  ctx.selbar = h('div', { class: 'selbar', hidden: true });
  ctx.tbody = h('tbody');
  ctx.all = h('input', {
    type: 'checkbox',
    'aria-label': 'Select all',
    onchange: (ev) => {
      ctx.selected = new Set(ev.target.checked ? ctx.entries.map(keyOf) : []);
      fillRows(ctx);
    },
  });
  ctx.headers = {};
  const table = h('table', { class: 'files' },
    h('thead', {}, h('tr', {},
      h('th', { class: 'col-check' }, ctx.all),
      sortHeader(ctx, 'name', 'Name', 'col-name'),
      sortHeader(ctx, 'size', 'Size', 'col-size'),
      sortHeader(ctx, 'mtime', 'Modified', 'col-modified'),
      h('th', { class: 'col-menu' }, h('span', { class: 'hidden-input', text: 'Actions' })))),
    ctx.tbody);
  const empty = ctx.entries.length ? null : h('div', { class: 'empty', text: ctx.canWrite ? 'This folder is empty. Drop files here to upload them.' : 'This folder is empty.' });
  const zone = h('div', { class: 'dropzone' }, table, empty);
  if (ctx.canWrite) wireDrop(zone, ctx);
  fillRows(ctx);
  const expires = view.kind === 'link' && view.info && view.info.expires;
  return h('section', { class: 'browser' },
    head,
    expires && h('p', { class: 'muted', text: `This link works until ${fmtTime(expires)}.` }),
    ctx.selbar,
    zone);
}

function breadcrumbs(view, path) {
  const nav = h('nav', { class: 'crumbs', 'aria-label': 'Where' });
  (path || []).forEach((step, i) => {
    if (i) nav.append(h('span', { class: 'sep', text: '/' }));
    const last = i === path.length - 1;
    nav.append(last
      ? h('span', { class: 'crumb current', text: step.display, 'aria-current': 'location' })
      : h('a', { class: 'crumb', href: view.link(i === 0 ? 'root' : step.object), text: step.display }));
  });
  return nav;
}

function sortHeader(ctx, key, label, cls) {
  const arrow = h('span', { class: 'arrow', 'aria-hidden': 'true' });
  const th = h('th', { class: cls }, h('button', {
    class: 'sort',
    onclick: () => {
      state.sort = state.sort.key === key ? { key, dir: -state.sort.dir } : { key, dir: key === 'name' ? 1 : -1 };
      fillRows(ctx);
    },
  }, label, arrow));
  ctx.headers[key] = { th, arrow };
  return th;
}

function sortedEntries(ctx) {
  const { key, dir } = state.sort;
  const value = (e) => (key === 'size' ? (e.kind === 'file' ? e.size ?? 0 : -1) : (e.mtime && e.mtime.sec) || 0);
  return ctx.entries.slice().sort((a, b) => {
    const fa = a.kind === 'dir' ? 0 : 1;
    const fb = b.kind === 'dir' ? 0 : 1;
    if (fa !== fb) return fa - fb;
    if (key === 'name') return dir * collator.compare(a.display, b.display);
    return dir * (value(a) - value(b)) || collator.compare(a.display, b.display);
  });
}

function fillRows(ctx) {
  for (const [key, { th, arrow }] of Object.entries(ctx.headers)) {
    const on = state.sort.key === key;
    th.setAttribute('aria-sort', on ? (state.sort.dir > 0 ? 'ascending' : 'descending') : 'none');
    arrow.textContent = on ? (state.sort.dir > 0 ? '↑' : '↓') : '';
  }
  ctx.tbody.replaceChildren(...sortedEntries(ctx).map((e) => row(ctx, e)));
  updateSelection(ctx);
}

function row(ctx, e) {
  const k = keyOf(e);
  const picked = ctx.selected.has(k);
  let name;
  if (e.kind === 'dir') {
    name = h('a', { class: 'name-link', href: ctx.view.link(e.object), text: e.display });
  } else if (e.kind === 'file') {
    name = h('a', {
      class: 'name-link',
      href: contentUrl(ctx.view, e.object, { revision: e.revision }),
      text: e.display,
      onclick: (ev) => {
        if (ev.ctrlKey || ev.metaKey || ev.shiftKey || ev.button) return; // a new tab opens the file itself
        ev.preventDefault();
        openPreview(ctx, e);
      },
    });
  } else {
    name = h('span', { class: 'name-text', text: e.display });
  }
  const tr = h('tr', { class: picked ? 'selected' : '' },
    h('td', { class: 'col-check' }, h('input', {
      type: 'checkbox',
      checked: picked,
      'aria-label': `Select ${e.display}`,
      onchange: (ev) => {
        if (ev.target.checked) ctx.selected.add(k); else ctx.selected.delete(k);
        tr.classList.toggle('selected', ev.target.checked);
        updateSelection(ctx);
      },
    })),
    h('td', { class: 'col-name link-cell' }, h('div', { class: 'name-cell' },
      fileIcon(e),
      name,
      e.kind === 'symlink' && h('span', { class: 'target', text: `→ ${e.target_display ?? ''}` }))),
    h('td', { class: 'col-size', text: e.kind === 'file' ? humanSize(e.size) : '' }),
    h('td', { class: 'col-modified', text: fmtTime(e.mtime) }),
    h('td', { class: 'col-menu' }, h('button', {
      class: 'icon-btn',
      'aria-label': `Actions for ${e.display}`,
      'aria-haspopup': 'menu',
      onclick: (ev) => rowMenu(ev.currentTarget, ctx, e),
    }, icon('more'))));
  tr.addEventListener('contextmenu', (ev) => { ev.preventDefault(); rowMenu(ev, ctx, e); });
  return tr;
}

function updateSelection(ctx) {
  const n = ctx.selected.size;
  ctx.all.checked = n > 0 && n === ctx.entries.length;
  ctx.all.indeterminate = n > 0 && n < ctx.entries.length;
  ctx.selbar.hidden = n === 0;
  if (!n) { ctx.selbar.replaceChildren(); return; }
  const items = ctx.entries.filter((e) => ctx.selected.has(keyOf(e)));
  ctx.selbar.replaceChildren(
    h('span', { class: 'sel-count', text: `${n} selected` }),
    h('button', { class: 'btn small', onclick: act(() => downloadMany(ctx.view, items)) }, icon('download'), 'Download'),
    ctx.canWrite && h('button', { class: 'btn small', onclick: act(() => moveItems(ctx, items)) }, 'Move to…'),
    ctx.canWrite && h('button', { class: 'btn small danger', onclick: act(() => deleteItems(ctx, items)) }, icon('trash'), 'Delete'),
    h('button', { class: 'btn small subtle', onclick: () => { ctx.selected.clear(); fillRows(ctx); } }, 'Clear'));
}

function rowMenu(at, ctx, e) {
  const { view } = ctx;
  const items = [];
  if (e.kind === 'file') {
    items.push(['Preview', () => openPreview(ctx, e)]);
    items.push(['Download', () => download(contentUrl(view, e.object, { download: true }))]);
    if (view.kind !== 'link') items.push(['History', () => openHistory(view, e, ctx.access, refresh)]);
  } else if (e.kind === 'dir') {
    items.push(['Open', () => { location.hash = view.link(e.object); }]);
    items.push(['Download as zip', () => download(zipUrl(view, e.object))]);
  } else {
    items.push([`Points to ${e.target_display ?? ''}`, () => {}]);
  }
  if (ctx.canShare && e.kind !== 'symlink') items.push(['Share…', () => openShareDialog(view.ns, e)]);
  if (ctx.canWrite) {
    items.push('-');
    items.push(['Rename…', () => renameItem(ctx, e)]);
    items.push(['Move to…', () => moveItems(ctx, [e])]);
    items.push(['Delete', () => deleteItems(ctx, [e]), 'danger']);
  }
  popupMenu(at, items);
}

// ---------------------------------------------------------------- changing the tree

async function newFolder(ctx) {
  const name = await promptName('New folder', '', 'Create');
  if (name === null) return;
  await api.post(`${ctx.view.base}/mkdir`, { parent: ctx.listing.dir.object, name: wireName(name) });
  toast(`Made the folder “${name}”.`);
  refresh();
}

async function renameItem(ctx, e) {
  const name = await promptName(`Rename “${e.display}”`, e.display, 'Rename');
  if (name === null || name === e.display.normalize('NFC')) return;
  const parent = ctx.listing.dir.object;
  await api.post(`${ctx.view.base}/move`, {
    object: e.object,
    from: { parent, name: e.name },
    to: { parent, name: wireName(name) },
  });
  refresh();
}

async function moveItems(ctx, items) {
  const from = ctx.listing.dir.object;
  const moving = new Set(items.filter((e) => e.kind === 'dir').map((e) => e.object));
  const target = await pickFolder(ctx.view, from, moving);
  if (!target) return;
  if (target.object === from) { toast('That is the folder they are in.'); return; }
  let moved = 0;
  for (const e of items) {
    try {
      await api.post(`${ctx.view.base}/move`, {
        object: e.object,
        from: { parent: from, name: e.name },
        to: { parent: target.object, name: e.name },
      });
      moved++;
    } catch (err) {
      if (err.status === 401) return;
      report(new UserError(`${e.display}: ${err.message}`));
    }
  }
  if (moved) toast(`Moved ${moved === 1 ? `“${items[0].display}”` : plural(moved, 'item')} to “${target.display}”.`);
  refresh();
}

/** A folder picked in the view, never one being moved nor anything in it; null if cancelled. */
function pickFolder(view, start, moving) {
  return new Promise((resolve) => {
    let chosen = null;
    let here = null;
    const box = h('div', { class: 'picker' }, loading());
    const moveButton = h('button', { class: 'btn primary', disabled: true, onclick: () => { chosen = here; d.close(); } }, 'Move here');
    const d = dialog({
      title: 'Move to',
      body: box,
      actions: [h('button', { class: 'btn', onclick: () => d.close() }, 'Cancel'), moveButton],
      onClose: () => resolve(chosen),
    });
    const go = async (obj) => {
      moveButton.disabled = true;
      box.replaceChildren(loading());
      try {
        const l = await api.get(`${view.base}/dir/${enc(obj)}`);
        here = l.dir;
        const crumbs = h('div', { class: 'crumbs' }, (l.path || []).map((s, i) => [
          i ? h('span', { class: 'sep', text: '/' }) : null,
          i === l.path.length - 1
            ? h('span', { class: 'crumb current', text: s.display })
            : h('button', { class: 'crumb', onclick: () => go(s.object) }, s.display),
        ]));
        const dirs = l.entries.filter((e) => e.kind === 'dir').sort((a, b) => collator.compare(a.display, b.display));
        const list = h('div', { class: 'picker-list' }, dirs.length
          ? dirs.map((e) => (moving.has(e.object)
            ? h('div', { class: 'picker-row disabled', title: 'This is being moved' }, icon('folder', 't-folder'), h('span', { text: e.display }))
            : h('button', { class: 'picker-row', onclick: () => go(e.object) }, icon('folder', 't-folder'), h('span', { text: e.display }))))
          : h('p', { class: 'muted', text: 'No folders in here.' }));
        box.replaceChildren(crumbs, list);
        moveButton.disabled = !atLeast(l.access || 'read', 'write');
      } catch (e) {
        box.replaceChildren(errorBox(e));
      }
    };
    go(start);
  });
}

async function deleteItems(ctx, items) {
  const one = items.length === 1 ? items[0] : null;
  let message = one
    ? (one.kind === 'dir' ? `“${one.display}” and everything in it will be deleted.` : `“${one.display}” will be deleted.`)
    : `${items.length} items will be deleted, each folder with everything in it.`;
  if (ctx.view.kind === 'ns') message += ' Recently deleted lists it, and brings it back.';
  const ok = await confirmDialog(one ? 'Delete' : `Delete ${items.length} items`, message, { ok: 'Delete', danger: true });
  if (!ok) return;
  const parent = ctx.listing.dir.object;
  const gone = [];
  for (const e of items) {
    try {
      await api.post(`${ctx.view.base}/delete`, { object: e.object, link: { parent, name: e.name }, base: e.revision });
      gone.push(e);
    } catch (err) {
      if (err.status === 401) return;
      report(new UserError(`${e.display}: ${err.message}`));
    }
  }
  refresh();
  if (!gone.length) return;
  const text = gone.length === 1 ? `Deleted “${gone[0].display}”.` : `Deleted ${gone.length} items.`;
  const undo = ctx.view.kind === 'ns'
    ? { label: 'Undo', run: () => undelete(ctx.view.ns, [...new Set(gone.map((e) => e.object))]) }
    : null;
  toast(text, { action: undo });
}

async function undelete(ns, objects) {
  const r = await api.post(`/v1/ns/${enc(ns)}/undelete`, { objects });
  toast(restoredText((r && r.items) || []));
  refresh();
}

function restoredText(items) {
  if (!items.length) return 'Nothing was brought back.';
  if (items.length === 1) {
    const it = items[0];
    const where = pathText(it.path);
    return `Brought back “${it.display}”${where ? ` as ${where}` : ''}${it.renamed ? ', under a new name as its own was taken' : ''}.`;
  }
  const renamed = items.filter((it) => it.renamed).length;
  return `Brought back ${plural(items.length, 'item')}${renamed ? `, ${renamed} under a new name as theirs was taken` : ''}.`;
}

// ---------------------------------------------------------------- preview, details, history

function previewEl(view, item) {
  const none = (text) => h('div', { class: 'pv-none', text });
  if (item.kind !== 'file') return none(item.kind === 'symlink' ? `A link to ${item.target_display ?? ''}` : 'No preview.');
  const url = contentUrl(view, item.object, { revision: item.revision });
  switch (typeOf(item.display)) {
    case 'image': {
      const img = h('img', { class: 'pv-img', src: url, alt: item.display });
      img.addEventListener('error', () => img.replaceWith(none('This image cannot be shown here: download it to open it.')));
      return img;
    }
    case 'video': return h('video', { class: 'pv-media', src: url, controls: true, preload: 'metadata' });
    case 'audio': return h('audio', { class: 'pv-audio', src: url, controls: true, preload: 'metadata' });
    case 'pdf': return h('iframe', { class: 'pv-pdf', src: url, title: item.display });
    case 'text': return textPreview(url, item.size);
    default: return none('No preview for this kind of file: download it to open it.');
  }
}

/** The first 256 KiB of a text file, as text. Read as a stream, so a server that sends all of it costs no more. */
function textPreview(url, size) {
  const pre = h('pre', { class: 'pv-text', text: 'Loading…' });
  if (!size) { pre.textContent = '(empty file)'; return pre; }
  (async () => {
    const r = await fetch(url, { headers: { Range: `bytes=0-${TEXT_PREVIEW_BYTES - 1}` }, credentials: 'same-origin' });
    if (!r.ok) throw new ApiError(r.status, (await r.text().catch(() => '')).trim());
    const reader = r.body.getReader();
    const parts = [];
    let got = 0;
    while (got < TEXT_PREVIEW_BYTES) {
      const { done, value } = await reader.read();
      if (done) break;
      parts.push(value);
      got += value.length;
    }
    reader.cancel().catch(() => {});
    const bytes = new Uint8Array(Math.min(got, TEXT_PREVIEW_BYTES));
    let at = 0;
    for (const p of parts) {
      const take = Math.min(p.length, bytes.length - at);
      bytes.set(p.subarray(0, take), at);
      at += take;
      if (at >= bytes.length) break;
    }
    const cut = size > bytes.length;
    pre.textContent = new TextDecoder('utf-8').decode(bytes)
      + (cut ? `\n\n… the first ${humanSize(bytes.length)} of ${humanSize(size)}: download the file for all of it.` : '');
  })().catch((e) => { pre.textContent = `It could not be read: ${e.message}`; });
  return pre;
}

function details(item, path) {
  const rows = [
    ['Size', item.kind === 'file' ? `${humanSize(item.size)} (${(item.size ?? 0).toLocaleString()} bytes)` : '—'],
    ['Modified', fmtTime(item.mtime)],
    ['Mode', modeString(item.kind, item.mode)],
    ['Where', pathText(path)],
  ];
  if (item.kind === 'symlink') rows.push(['Points to', item.target_display ?? '']);
  return h('dl', { class: 'details' }, rows.map(([k, v]) => [h('dt', { text: k }), h('dd', { text: v, class: k === 'Mode' ? 'mono' : '' })]));
}

function openPreview(ctx, e) {
  const { view, listing } = ctx;
  const path = [...(listing.path || []), { display: e.display }];
  const d = dialog({
    title: e.display,
    wide: true,
    body: h('div', { class: 'preview' }, h('div', { class: 'pv-main' }, previewEl(view, e)), details(e, path)),
    actions: [
      view.kind !== 'link' && h('button', { class: 'btn', onclick: () => { d.close(); openHistory(view, e, ctx.access, refresh); } }, icon('history'), 'History'),
      ctx.canShare && h('button', { class: 'btn', onclick: () => { d.close(); openShareDialog(view.ns, e); } }, icon('share'), 'Share'),
      h('button', { class: 'btn primary', onclick: () => download(contentUrl(view, e.object, { download: true })) }, icon('download'), 'Download'),
    ],
  });
}

/** A view's root that is a file: a share of one, or a link to one. */
function fileView(view, item, path, access, expires) {
  return h('section', { class: 'fileview' },
    h('div', { class: 'fileview-head' },
      fileIcon(item, 'big'),
      h('div', {},
        h('h1', { text: item.display }),
        h('p', { class: 'muted', text: [item.kind === 'file' ? humanSize(item.size) : '', fmtTime(item.mtime)].filter(Boolean).join(' · ') }))),
    h('div', { class: 'actions' },
      item.kind === 'file' && h('button', { class: 'btn primary big', onclick: () => download(contentUrl(view, item.object, { download: true })) }, icon('download'), 'Download'),
      view.kind !== 'link' && item.kind === 'file' && h('button', { class: 'btn', onclick: () => openHistory(view, item, access, refresh) }, icon('history'), 'History')),
    expires && h('p', { class: 'muted', text: `This link works until ${fmtTime(expires)}.` }),
    h('div', { class: 'pv-main inline' }, previewEl(view, item)),
    view.kind !== 'link' && h('div', { class: 'panel' }, details(item, path)));
}

async function openHistory(view, item, access, onChange) {
  const box = h('div', { class: 'history' }, loading());
  dialog({ title: `History of “${item.display}”`, body: box, wide: true });
  const canWrite = atLeast(access || 'read', 'write');
  const load = async () => {
    const r = await api.get(`${view.base}/history/${enc(item.object)}`);
    const revisions = (r && r.revisions) || [];
    if (!revisions.length) { box.replaceChildren(h('p', { class: 'muted', text: 'No history.' })); return; }
    const current = revisions[0].revision;
    const rows = revisions.map((rev) => h('tr', { class: rev.revision === current ? 'current' : '' },
      h('td', { class: 'nowrap' }, fmtTime(rev.time), rev.revision === current && h('span', { class: 'badge', text: 'now' })),
      h('td', { text: byText(rev.user, rev.device_name) }),
      h('td', { class: 'col-size', text: rev.kind === 'file' ? humanSize(rev.size) : rev.kind }),
      h('td', { class: 'mono nowrap', text: modeString(rev.kind, rev.mode) }),
      h('td', { class: 'row-actions' },
        rev.kind === 'file' && h('button', { class: 'btn small', onclick: () => download(contentUrl(view, item.object, { revision: rev.revision, download: true })) }, 'Download'),
        canWrite && rev.revision !== current && h('button', {
          class: 'btn small',
          onclick: act(async (ev) => {
            const button = ev.currentTarget;
            button.disabled = true;
            try {
              await api.post(`${view.base}/restore`, { object: item.object, revision: rev.revision });
              toast(`Restored the version of ${fmtTime(rev.time)}.`);
              await load();
              if (onChange) onChange();
            } finally {
              button.disabled = false;
            }
          }),
        }, 'Restore'))));
    box.replaceChildren(
      h('p', { class: 'muted', text: 'Restoring a version makes a new one with its content and metadata, so a restore can be undone too.' }),
      h('table', { class: 'list' },
        h('thead', {}, h('tr', {}, ['When', 'By', 'Size', 'Mode', ''].map((t) => h('th', { text: t })))),
        h('tbody', {}, rows)));
  };
  try { await load(); } catch (e) { box.replaceChildren(errorBox(e)); }
}

// ---------------------------------------------------------------- uploads

const STATUS = {
  waiting: 'Waiting', preparing: 'Preparing', sending: 'Sending', storing: 'Storing',
  done: 'Done', failed: 'Failed', cancelled: 'Cancelled', skipped: 'Skipped',
};
const FINISHED = new Set(['done', 'failed', 'cancelled', 'skipped']);

const uploads = {
  tasks: [],
  running: false,
  el: null,
  list: null,
  title: null,
  panel() {
    if (this.el) return this.el;
    this.title = h('span', { class: 'up-title' });
    this.list = h('div', { class: 'up-list' });
    this.el = h('div', { class: 'uploads', role: 'region', 'aria-label': 'Uploads' },
      h('div', { class: 'up-head' },
        this.title,
        h('button', { class: 'btn subtle small', onclick: () => this.clear() }, 'Clear finished'),
        h('button', { class: 'icon-btn', 'aria-label': 'Fold the list', onclick: () => this.el.classList.toggle('collapsed') }, icon('chevron'))),
      this.list);
    document.body.append(this.el);
    return this.el;
  },
  add(tasks) {
    this.panel().hidden = false;
    for (const t of tasks) {
      this.tasks.push(t);
      this.list.append(t.el);
    }
    this.summary();
    this.pump();
  },
  summary() {
    if (!this.title) return;
    const left = this.tasks.filter((t) => !FINISHED.has(t.state)).length;
    const failed = this.tasks.filter((t) => t.state === 'failed').length;
    if (left) this.title.textContent = `Uploading: ${left} of ${this.tasks.length} left`;
    else this.title.textContent = failed ? `Uploads: ${failed} failed` : 'Uploads done';
  },
  clear() {
    this.tasks = this.tasks.filter((t) => {
      if (!FINISHED.has(t.state)) return true;
      t.el.remove();
      return false;
    });
    if (!this.tasks.length) this.el.hidden = true;
    this.summary();
  },
  busy() { return this.tasks.some((t) => !FINISHED.has(t.state)); },
  /** Cancel what is left and forget every upload: what the last person to sign in here sent is theirs. */
  forget() {
    for (const t of this.tasks) t.cancel();
    this.tasks = [];
    if (this.el) {
      this.list.replaceChildren();
      this.el.hidden = true;
    }
  },
  async pump() {
    if (this.running) return;
    this.running = true;
    try {
      for (;;) {
        const t = this.tasks.find((x) => x.state === 'waiting');
        if (!t) break;
        await runUpload(t);
        this.summary();
      }
    } finally {
      this.running = false;
      this.summary();
    }
  },
};

function makeTask(view, parent, item, batch) {
  const label = [...item.dirs, item.file ? item.file.name : ''].filter(Boolean).join('/');
  const bar = h('progress', { max: '1', value: 0 });
  const status = h('span', { class: 'up-status', text: STATUS.waiting });
  const cancelButton = h('button', { class: 'icon-btn', 'aria-label': `Cancel ${label}`, onclick: () => t.cancel() }, icon('close'));
  const t = {
    view,
    parent,
    dirs: item.dirs,
    file: item.file,
    name: item.file ? item.file.name : null,
    batch,
    id: null,
    xhr: null,
    cancelled: false,
    state: 'waiting',
    el: h('div', { class: 'up-item', 'data-state': 'waiting' },
      h('div', { class: 'up-name', text: label, title: label }), bar, h('div', { class: 'up-row' }, status, cancelButton)),
    set(st, text) {
      this.state = st;
      this.el.dataset.state = st;
      status.textContent = text || STATUS[st];
      if (FINISHED.has(st)) cancelButton.hidden = true;
      if (st === 'done') bar.value = 1;
      uploads.summary();
    },
    progress(sent) {
      const size = this.file ? this.file.size : 0;
      bar.value = size ? sent / size : 1;
      status.textContent = `${humanSize(sent)} of ${humanSize(size)}`;
    },
    storing(stored, size) {
      bar.value = size ? (stored || 0) / size : 1;
      status.textContent = size ? `Storing: ${humanSize(stored || 0)} of ${humanSize(size)}` : 'Storing';
    },
    cancel() {
      if (FINISHED.has(this.state)) return;
      this.cancelled = true;
      if (this.xhr) this.xhr.abort();
      if (this.state === 'waiting') this.set('cancelled');
    },
  };
  return t;
}

function startBatch(ctx, items) {
  if (!items.length) return;
  const batch = { policy: null, cache: { lists: new Map(), dirs: new Map() }, tasks: [] };
  batch.resolve = async (name, existing) => {
    if (batch.policy) return batch.policy;
    const left = batch.tasks.filter((t) => t.state === 'waiting').length;
    const { choice, all } = await conflictDialog(name, existing, left);
    if (all) batch.policy = choice;
    return choice;
  };
  const parent = ctx.listing.dir.object;
  batch.tasks = items.map((it) => makeTask(ctx.view, parent, it, batch));
  uploads.add(batch.tasks);
}

function pickFiles(ctx, folder) {
  const input = h('input', { type: 'file', multiple: true, class: 'hidden-input' });
  if (folder) {
    input.setAttribute('webkitdirectory', '');
    input.webkitdirectory = true;
  }
  input.addEventListener('change', () => {
    const items = [...input.files].map((file) => ({
      file,
      dirs: folder && file.webkitRelativePath ? file.webkitRelativePath.split('/').slice(0, -1) : [],
    }));
    input.remove();
    startBatch(ctx, items);
  });
  input.addEventListener('cancel', () => input.remove());
  document.body.append(input);
  input.click();
}

function wireDrop(zone, ctx) {
  let depth = 0;
  const hasFiles = (ev) => [...((ev.dataTransfer && ev.dataTransfer.types) || [])].includes('Files');
  zone.addEventListener('dragenter', (ev) => {
    if (!hasFiles(ev)) return;
    ev.preventDefault();
    depth++;
    zone.classList.add('dragging');
  });
  zone.addEventListener('dragover', (ev) => {
    if (!hasFiles(ev)) return;
    ev.preventDefault();
    ev.dataTransfer.dropEffect = 'copy';
  });
  zone.addEventListener('dragleave', () => {
    depth = Math.max(0, depth - 1);
    if (!depth) zone.classList.remove('dragging');
  });
  zone.addEventListener('drop', (ev) => {
    if (!hasFiles(ev)) return;
    ev.preventDefault();
    ev.stopPropagation();
    depth = 0;
    zone.classList.remove('dragging');
    dropped(ctx, ev.dataTransfer);
  });
}

/** What was dropped, folders walked: their entries must be taken before the drop event ends. */
function dropped(ctx, dt) {
  const entries = [];
  for (const it of dt.items || []) {
    if (it.kind !== 'file') continue;
    const entry = it.webkitGetAsEntry ? it.webkitGetAsEntry() : null;
    if (entry) entries.push(entry);
  }
  if (!entries.length) {
    startBatch(ctx, [...dt.files].map((file) => ({ file, dirs: [] })));
    return;
  }
  walkEntries(entries).then((items) => startBatch(ctx, items), report);
}

async function walkEntries(entries) {
  const out = [];
  const readAll = (reader) => new Promise((resolve, reject) => {
    const all = [];
    const next = () => reader.readEntries((got) => {
      if (!got.length) resolve(all);
      else { all.push(...got); next(); }
    }, reject);
    next();
  });
  const walk = async (entry, dirs) => {
    if (entry.isFile) {
      const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
      out.push({ file, dirs });
    } else if (entry.isDirectory) {
      const here = [...dirs, entry.name];
      const children = await readAll(entry.createReader());
      if (!children.length) out.push({ file: null, dirs: here }); // an empty folder is made too
      for (const c of children) await walk(c, here);
    }
  };
  for (const e of entries) await walk(e, []);
  return out;
}

async function listEntries(view, parent, cache, fresh = false) {
  if (!fresh && cache.lists.has(parent)) return cache.lists.get(parent);
  const l = await api.get(`${view.base}/dir/${enc(parent)}`);
  cache.lists.set(parent, l.entries || []);
  return l.entries || [];
}

async function findEntry(view, parent, name, cache) {
  const wire = wireName(name);
  return (await listEntries(view, parent, cache, true)).find((e) => e.name === wire) || null;
}

/** The folder `name` in `parent`: the one there, or a new one. */
async function ensureDir(view, parent, name, cache) {
  const key = `${parent}/${name}`;
  if (cache.dirs.has(key)) return cache.dirs.get(key);
  const wire = wireName(name);
  let found = (await listEntries(view, parent, cache)).find((e) => e.name === wire);
  if (!found) {
    try {
      const r = await api.post(`${view.base}/mkdir`, { parent, name: wire });
      cache.dirs.set(key, r.object);
      cache.lists.delete(parent);
      return r.object;
    } catch (e) {
      if (e.status !== 409) throw e;
      found = (await listEntries(view, parent, cache, true)).find((x) => x.name === wire);
      if (!found) throw e;
    }
  }
  if (found.kind !== 'dir') throw new UserError(`“${name}” is already here, and is not a folder.`);
  cache.dirs.set(key, found.object);
  return found.object;
}

async function ensurePath(view, parent, dirs, cache) {
  let at = parent;
  for (const d of dirs) at = await ensureDir(view, at, d, cache);
  return at;
}

async function freeName(view, parent, name, cache) {
  const entries = await listEntries(view, parent, cache, true);
  const taken = new Set(entries.map((e) => e.display.normalize('NFC')));
  const dot = name.lastIndexOf('.');
  const [stem, ext] = dot > 0 ? [name.slice(0, dot), name.slice(dot)] : [name, ''];
  for (let n = 2; n < 10000; n++) {
    const candidate = `${stem} (${n})${ext}`;
    if (!taken.has(candidate.normalize('NFC'))) return candidate;
  }
  throw new UserError(`No free name like “${name}” is left.`);
}

function conflictDialog(name, existing, left) {
  return new Promise((resolve) => {
    let choice = 'skip';
    const all = h('input', { type: 'checkbox' });
    const pick = (c) => { choice = c; d.close(); };
    const isFile = existing.kind === 'file';
    const d = dialog({
      title: 'Already here',
      body: [
        h('p', {}, '“', h('strong', { text: name }), isFile
          ? '” is already in this folder. Replace it with the one being uploaded, which keeps what is there now in its history, or keep both?'
          : '” is already in this folder, and is not a file, so it cannot be replaced: keep both, or skip this one?'),
        left > 0 && h('label', { class: 'check' }, all, `Do the same for every other name taken in this upload (${left} left)`),
      ],
      actions: [
        h('button', { class: 'btn', onclick: () => pick('skip') }, 'Skip'),
        h('button', { class: 'btn', onclick: () => pick('keep') }, 'Keep both'),
        isFile && h('button', { class: 'btn primary', onclick: () => pick('replace') }, 'Replace'),
      ],
      onClose: () => resolve({ choice, all: all.checked }),
    });
  });
}

function fileMtime(file) {
  const ms = Math.max(0, Math.round(file.lastModified || Date.now()));
  return { sec: Math.floor(ms / 1000), nsec: (ms % 1000) * 1e6 };
}

async function runUpload(t) {
  const { view, batch } = t;
  try {
    t.set('preparing');
    const parent = await ensurePath(view, t.parent, t.dirs, batch.cache);
    if (t.cancelled) throw new Cancelled();
    if (!t.file) { t.set('done', 'Folder made'); refreshSoon(view); return; }
    let name = t.name;
    let replace = null;
    for (let attempt = 0; ; attempt++) {
      const body = { parent, name: wireName(name), size: t.file.size, mtime: fileMtime(t.file) };
      if (replace) body.replace = replace;
      try {
        const r = await api.post(`${view.base}/uploads`, body);
        t.id = r.id;
        break;
      } catch (e) {
        if (e.status !== 409 || replace || attempt > 50) throw e;
        const existing = await findEntry(view, parent, name, batch.cache);
        if (!existing) throw e;
        const choice = await batch.resolve(name, existing);
        if (t.cancelled) throw new Cancelled();
        if (choice === 'skip') { t.set('skipped'); return; }
        if (choice === 'replace') {
          if (existing.kind !== 'file') throw new UserError(`“${name}” is a folder here, so a file cannot replace it.`);
          replace = existing.revision;
        } else {
          name = await freeName(view, parent, name, batch.cache);
        }
      }
    }
    if (name !== t.name) t.el.querySelector('.up-name').textContent = [...t.dirs, name].join('/');
    t.set('sending');
    t.progress(0);
    await sendParts(t);
    await api.post(`${view.base}/uploads/${enc(t.id)}/commit`);
    t.set('storing');
    await waitStored(t);
    t.set('done', replace ? 'Replaced' : 'Done');
    batch.cache.lists.delete(parent);
    refreshSoon(view);
  } catch (e) {
    if (e instanceof Cancelled || t.cancelled) {
      t.set('cancelled');
      if (t.id) request('DELETE', `${view.base}/uploads/${enc(t.id)}`).catch(() => {});
      return;
    }
    if (t.id && t.state === 'sending') request('DELETE', `${view.base}/uploads/${enc(t.id)}`).catch(() => {});
    t.set('failed', e.message || String(e));
  }
}

/** One part, by XMLHttpRequest, which fetch cannot replace while it shows no upload progress. */
function putPart(t, url, blob, onProgress) {
  return new Promise((resolve, reject) => {
    const x = new XMLHttpRequest();
    t.xhr = x;
    x.open('PUT', url);
    x.setRequestHeader('X-Cairn-Web', '1');
    x.setRequestHeader('Content-Type', 'application/octet-stream');
    x.upload.onprogress = (e) => onProgress(e.loaded);
    x.onload = () => { t.xhr = null; resolve({ status: x.status, text: x.responseText || '' }); };
    x.onerror = () => { t.xhr = null; reject(new ApiError(0, 'The connection was cut off.')); };
    x.onabort = () => { t.xhr = null; reject(new Cancelled()); };
    x.send(blob);
  });
}

function receivedOf(text) {
  try {
    const j = JSON.parse(text);
    return typeof j.received === 'number' ? j.received : null;
  } catch {
    return null;
  }
}

async function sendParts(t) {
  const base = `${t.view.base}/uploads/${enc(t.id)}`;
  const size = t.file.size;
  let offset = 0;
  let failures = 0;
  while (offset < size) {
    if (t.cancelled) throw new Cancelled();
    const end = Math.min(size, offset + PART_BYTES);
    const from = offset;
    let res;
    try {
      res = await putPart(t, `${base}?offset=${offset}`, t.file.slice(offset, end), (loaded) => t.progress(from + loaded));
    } catch (e) {
      if (e instanceof Cancelled) throw e;
      failures++;
      if (failures > 6) throw e;
      t.set('sending', `Connection lost: trying again (${failures})`);
      await sleep(1000 * failures);
      const st = await api.get(base);
      offset = st.received;
      continue;
    }
    if (res.status === 401) { signedOut(); throw new ApiError(401, res.text.trim()); }
    if (res.status === 409) {
      const got = receivedOf(res.text);
      if (got === null || ++failures > 6) throw new ApiError(409, res.text.trim());
      offset = got;
      continue;
    }
    if (res.status < 200 || res.status >= 300) throw new ApiError(res.status, res.text.trim());
    const got = receivedOf(res.text);
    offset = got === null ? end : got;
    failures = 0;
    t.progress(offset);
  }
}

async function waitStored(t) {
  const url = `${t.view.base}/uploads/${enc(t.id)}`;
  for (let i = 0; ; i++) {
    await sleep(i === 0 ? 150 : 500);
    if (t.cancelled) throw new Cancelled();
    const st = await api.get(url);
    if (st.state === 'done') return st.done;
    if (st.state === 'failed') throw new UserError(st.error || 'The server could not store it.');
    t.storing(st.stored, st.size);
  }
}

// ---------------------------------------------------------------- sharing

async function openShareDialog(ns, item) {
  const base = `/v1/ns/${enc(ns)}`;
  const box = h('div', { class: 'share' }, loading());
  const isRoot = item.name === undefined || item.name === null;
  dialog({ title: isRoot ? `Share the folder ${item.display}` : `Share “${item.display}”`, body: box, wide: true });
  const load = async () => {
    const [r, users] = await Promise.all([api.get(`${base}/shares?object=${enc(item.object)}`), getUsers()]);
    box.replaceChildren(...shareSections(base, item, (r && r.shares) || [], users, load));
  };
  try { await load(); } catch (e) { box.replaceChildren(errorBox(e)); }
}

function madeText(s) {
  return `${s.by ? `by ${s.by}` : ''}${s.created ? `${s.by ? ', ' : ''}${fmtDate(s.created)}` : ''}${s.expires ? ` · until ${fmtDate(s.expires)}` : ''}`;
}

// The server names a link's address when links open somewhere other than here, such as a door open to anyone.
const linkUrl = (s) => s.url || `${location.origin}/s/${s.link}`;

async function copyText(text, input) {
  try {
    await navigator.clipboard.writeText(text);
    toast('Link copied.');
  } catch {
    if (input) { input.focus(); input.select(); }
    toast('Copy the selected link with your keyboard.');
  }
}

async function removeShare(base, s, reload) {
  const what = s.user ? `Stop sharing with ${s.user}?` : 'Delete this link? Anyone who has it can no longer open it.';
  if (!(await confirmDialog(s.user ? 'Stop sharing' : 'Delete the link', what, { ok: s.user ? 'Stop sharing' : 'Delete', danger: true }))) return;
  await api.del(`${base}/shares/${enc(s.id)}`);
  await reload();
}

function shareSections(base, item, shares, users, reload) {
  const people = shares.filter((s) => s.user);
  const links = shares.filter((s) => !s.user);

  const others = users.filter((u) => u !== state.me.user && !people.some((s) => s.user === u));
  const userSelect = h('select', { 'aria-label': 'User', class: 'grow' }, others.length
    ? others.map((u) => h('option', { value: u, text: u }))
    : h('option', { value: '', text: 'No one else to share with' }));
  const accessSelect = h('select', { 'aria-label': 'Access' },
    h('option', { value: 'read', text: LEVEL_TEXT.read }),
    h('option', { value: 'write', text: LEVEL_TEXT.write }));
  const userExpiry = h('input', { type: 'date', min: today(), 'aria-label': 'Until (optional)', title: 'Until (optional)' });
  const shareButton = h('button', {
    class: 'btn primary',
    disabled: !others.length,
    onclick: act(async () => {
      shareButton.disabled = true;
      try {
        await api.post(`${base}/shares`, { object: item.object, user: userSelect.value, access: accessSelect.value, ...expiryOf(userExpiry) });
        toast(`Shared with ${userSelect.value}.`);
        await reload();
      } finally {
        shareButton.disabled = false;
      }
    }),
  }, 'Share');

  const linkExpiry = h('input', { type: 'date', min: today(), 'aria-label': 'Until (optional)', title: 'Until (optional)' });
  const linkButton = h('button', {
    class: 'btn primary',
    onclick: act(async () => {
      linkButton.disabled = true;
      try {
        const s = await api.post(`${base}/shares`, { object: item.object, ...expiryOf(linkExpiry) });
        await reload();
        if (s && s.link) {
          const url = linkUrl(s);
          copyText(url, [...document.querySelectorAll('.link-input')].find((i) => i.value === url));
        }
      } finally {
        linkButton.disabled = false;
      }
    }),
  }, icon('globe'), 'Make a link');

  return [
    h('div', { class: 'panel' },
      h('h2', {}, 'People'),
      h('p', { class: 'muted', text: 'Users of this server who may open it while signed in. Someone given the whole folder is in Members instead.' }),
      people.length
        ? people.map((s) => h('div', { class: 'share-row' },
          icon('people'),
          h('span', { class: 'who-line' }, h('strong', { text: s.user }), h('span', { class: `badge ${s.access}`, text: LEVEL_TEXT[s.access] || s.access })),
          h('span', { class: 'share-meta', text: madeText(s) }),
          h('button', { class: 'btn small danger', onclick: act(() => removeShare(base, s, reload)) }, 'Remove')))
        : h('p', { class: 'muted', text: 'Shared with no one.' }),
      h('div', { class: 'form-row' }, userSelect, accessSelect, h('label', { class: 'check muted' }, 'until', userExpiry), shareButton)),
    h('div', { class: 'panel' },
      h('h2', {}, 'Public links'),
      h('p', { class: 'muted', text: 'Anyone with a link can view and download it, without signing in.' }),
      links.length
        ? links.map((s) => {
          const input = h('input', { type: 'text', class: 'link-input', value: linkUrl(s), readOnly: true, 'aria-label': 'Link', onfocus: (ev) => ev.target.select() });
          return h('div', { class: 'share-row' },
            icon('globe'),
            input,
            h('button', { class: 'btn small', onclick: () => copyText(linkUrl(s), input) }, 'Copy'),
            h('span', { class: 'share-meta', text: madeText(s) }),
            h('button', { class: 'btn small danger', onclick: act(() => removeShare(base, s, reload)) }, 'Delete'));
        })
        : h('p', { class: 'muted', text: 'No link yet.' }),
      h('div', { class: 'form-row' }, h('label', { class: 'check muted' }, 'until', linkExpiry), linkButton)),
  ];
}

// ---------------------------------------------------------------- folder screens

async function renderMembers(gen, ns) {
  const base = `/v1/ns/${enc(ns)}`;
  const manager = folderAccess(ns) === 'manage';
  const [answer, users] = await Promise.all([api.get(`${base}/access`), manager ? getUsers() : Promise.resolve([])]);
  const members = Array.isArray(answer) ? answer : (answer && answer.members) || [];
  const changed = async (user) => {
    if (user === state.me.user) {
      state.me = await api.get('/v1/me');
      if (!folderAccess(ns)) { location.hash = '#/'; return; }
    }
    refresh();
  };
  const rows = members.map((m) => {
    let level = h('span', { class: `badge ${m.access}`, text: m.access });
    if (manager) {
      level = h('select', {
        'aria-label': `Level of ${m.user}`,
        onchange: act(async (ev) => {
          const sel = ev.target;
          try {
            await api.put(`${base}/access/${enc(m.user)}`, { access: sel.value });
            toast(`${m.user} now has ${sel.value} access.`);
            await changed(m.user);
          } catch (e) {
            sel.value = m.access;
            throw e;
          }
        }),
      }, LEVELS.map((l) => h('option', { value: l, text: l })));
      level.value = m.access;
    }
    return h('tr', {},
      h('td', {}, h('strong', { text: m.user }), m.user === state.me.user && h('span', { class: 'muted', text: ' (you)' })),
      h('td', {}, level),
      h('td', { class: 'row-actions' }, manager && h('button', {
        class: 'btn small danger',
        onclick: act(async () => {
          const ok = await confirmDialog('Remove access', `${m.user} will no longer reach ${folderName(ns)}. What their devices hold of it stays on them.`, { ok: 'Remove', danger: true });
          if (!ok) return;
          await api.del(`${base}/access/${enc(m.user)}`);
          toast(`${m.user} no longer has access.`);
          await changed(m.user);
        }),
      }, 'Remove')));
  });
  let addForm = null;
  if (manager) {
    const candidates = users.filter((u) => !members.some((m) => m.user === u));
    const userSelect = h('select', { 'aria-label': 'User', class: 'grow' }, candidates.length
      ? candidates.map((u) => h('option', { value: u, text: u }))
      : h('option', { value: '', text: 'Every user has access already' }));
    const levelSelect = h('select', { 'aria-label': 'Level' }, LEVELS.map((l) => h('option', { value: l, text: l })));
    levelSelect.value = 'write';
    addForm = h('div', { class: 'panel' },
      h('h2', {}, 'Give someone access'),
      h('div', { class: 'form-row' }, userSelect, levelSelect, h('button', {
        class: 'btn primary',
        disabled: !candidates.length,
        onclick: act(async () => {
          await api.put(`${base}/access/${enc(userSelect.value)}`, { access: levelSelect.value });
          toast(`${userSelect.value} has ${levelSelect.value} access now.`);
          refresh();
        }),
      }, 'Add')),
      h('p', { class: 'hint', text: 'A new user is made on the server, with cairn-server user add <name>.' }));
  }
  show(gen, h('section', {},
    h('div', { class: 'screen-head' }, h('h1', { text: `Members of ${folderName(ns)}` })),
    h('p', { class: 'muted', text: 'read: see the files and their history. write: also add, change and delete. manage: also say who has access, and share. A folder always keeps one manager.' }),
    h('table', { class: 'list' },
      h('thead', {}, h('tr', {}, h('th', { text: 'User' }), h('th', { text: 'Level' }), h('th', {}))),
      h('tbody', {}, rows)),
    manager ? addForm : h('p', { class: 'hint', text: 'Only a manager of this folder changes who has access.' })), `Members of ${folderName(ns)}`);
}

async function renderShares(gen, ns) {
  const base = `/v1/ns/${enc(ns)}`;
  const r = await api.get(`${base}/shares`);
  const shares = (r && r.shares) || [];
  const reload = () => refresh();
  const rows = shares.map((s) => {
    const where = pathText(s.path);
    const input = s.link ? h('input', { type: 'text', class: 'link-input', value: linkUrl(s), readOnly: true, 'aria-label': 'Link', onfocus: (ev) => ev.target.select() }) : null;
    return h('tr', {},
      h('td', {}, h('div', { class: 'name-cell' },
        fileIcon({ kind: s.kind, display: where || '' }),
        h('span', { text: s.path === undefined || s.path === null ? '(deleted)' : where || folderName(ns) }))),
      h('td', {}, s.user ? h('span', {}, icon('people'), ' ', s.user) : h('span', {}, icon('globe'), ' public link')),
      h('td', {}, h('span', { class: `badge ${s.access}`, text: LEVEL_TEXT[s.access] || s.access })),
      h('td', { class: 'share-meta', text: madeText(s) }),
      h('td', { class: 'row-actions' },
        input && h('button', { class: 'btn small', onclick: () => copyText(linkUrl(s), input) }, 'Copy link'),
        h('button', { class: 'btn small danger', onclick: act(() => removeShare(base, s, reload)) }, s.user ? 'Remove' : 'Delete')));
  });
  show(gen, h('section', {},
    h('div', { class: 'screen-head' }, h('h1', { text: `Shares and links in ${folderName(ns)}` })),
    h('p', { class: 'muted', text: 'Everything in this folder shared with a user, or by a public link. Share something from its menu in the folder.' }),
    shares.length
      ? h('table', { class: 'list' },
        h('thead', {}, h('tr', {}, ['What', 'With', 'Access', 'Made', ''].map((t) => h('th', { text: t })))),
        h('tbody', {}, rows))
      : h('p', { class: 'empty', text: 'Nothing here is shared.' })), `Shares in ${folderName(ns)}`);
}

async function renderDeleted(gen, ns) {
  const base = `/v1/ns/${enc(ns)}`;
  const canWrite = atLeast(folderAccess(ns) || 'read', 'write');
  const r = await api.get(`${base}/deleted?days=30`);
  const items = (r && r.items) || [];
  const selected = new Set();
  const restoreButton = h('button', {
    class: 'btn primary',
    disabled: true,
    onclick: act(async () => {
      restoreButton.disabled = true;
      try {
        await undelete(ns, [...selected]);
      } finally {
        restoreButton.disabled = !selected.size;
      }
    }),
  }, 'Bring back');
  const count = h('span', { class: 'muted' });
  const update = () => {
    restoreButton.disabled = !selected.size;
    count.textContent = selected.size ? `${selected.size} selected` : '';
  };
  const rows = items.map((it) => {
    const size = it.kind === 'dir'
      ? [it.files ? plural(it.files, 'file') : 'empty', it.size ? humanSize(it.size) : ''].filter(Boolean).join(', ')
      : humanSize(it.size);
    const where = pathText(it.path) || folderName(ns);
    const when = fmtTime(it.time);
    const by = byText(it.user, it.device_name);
    // The columns a narrow page leaves out are told under the name instead (app.css, "Recently deleted").
    const tr = h('tr', {},
      h('td', { class: 'col-check' }, canWrite && h('input', {
        type: 'checkbox',
        'aria-label': `Select ${it.display}`,
        onchange: (ev) => {
          if (ev.target.checked) selected.add(it.object); else selected.delete(it.object);
          tr.classList.toggle('selected', ev.target.checked);
          update();
        },
      })),
      h('td', { class: 'col-name' }, h('div', { class: 'name-cell' }, fileIcon(it), h('div', { class: 'name-stack' },
        h('span', { class: 'name-text', text: it.display }),
        h('div', { class: 'row-meta' },
          h('span', { class: 'm-in' }, 'in ', slashBreaks(where)),
          h('span', { class: 'm-sizewhen' }, h('span', { class: 'm-size', text: size }), h('wbr'), h('span', { class: 'm-when', text: when })),
          by !== '—' && h('span', { class: 'm-by', text: `by ${by}` }))))),
      h('td', { class: 'col-in' }, slashBreaks(where)),
      h('td', { class: 'col-size', text: size }),
      h('td', { class: 'col-when', text: when }),
      h('td', { class: 'col-by', text: by }));
    return tr;
  });
  show(gen, h('section', {},
    h('div', { class: 'screen-head' }, h('h1', { text: `Recently deleted in ${folderName(ns)}` }), count, canWrite && restoreButton),
    h('p', { class: 'muted', text: 'What was deleted in the last 30 days and is still deleted, from any device. Each comes back to where it was, as it was.' }),
    r && r.complete === false && h('p', { class: 'hint', text: 'Only the most recent deletes are listed here: older ones are further back in the history than this list reads.' }),
    items.length
      ? h('table', { class: 'list deleted' },
        h('thead', {}, h('tr', {}, h('th', { class: 'col-check' }),
          h('th', { class: 'col-name', text: 'Name' }),
          h('th', { class: 'col-in', text: 'Was in' }),
          h('th', { class: 'col-size', text: 'Size' }),
          h('th', { class: 'col-when', text: 'Deleted' }),
          h('th', { class: 'col-by', text: 'By' }))),
        h('tbody', {}, rows))
      : h('p', { class: 'empty', text: 'Nothing was deleted in the last 30 days.' })), `Deleted in ${folderName(ns)}`);
}

async function renderSharedList(gen) {
  const r = await api.get('/v1/shared');
  const shares = (r && r.shares) || [];
  state.shared = shares;
  const rows = shares.map((s) => h('tr', {},
    h('td', { class: 'link-cell' }, h('div', { class: 'name-cell' }, fileIcon(s.item), h('a', { class: 'name-link', href: `#/s/${enc(s.id)}/root`, text: s.item.display }))),
    h('td', { text: s.by || '' }),
    h('td', {}, h('span', { class: `badge ${s.access}`, text: LEVEL_TEXT[s.access] || s.access })),
    h('td', { class: 'muted', text: s.expires ? `until ${fmtDate(s.expires)}` : '' }),
    h('td', { class: 'col-size', text: s.item.kind === 'file' ? humanSize(s.item.size) : '' })));
  show(gen, h('section', {},
    h('div', { class: 'screen-head' }, h('h1', { text: 'Shared with me' })),
    h('p', { class: 'muted', text: 'Files and folders other users share with you. Folders given to you whole are under Folders.' }),
    shares.length
      ? h('table', { class: 'list' },
        h('thead', {}, h('tr', {}, ['Name', 'Shared by', 'Access', 'Expires', ''].map((t) => h('th', { text: t })))),
        h('tbody', {}, rows))
      : h('p', { class: 'empty', text: 'Nothing is shared with you.' })), 'Shared with me');
}

// ---------------------------------------------------------------- settings

/** Chosen here, kept in this browser, it applies at once. A browser that will not keep it is said so. */
function saveSetting(key, value) {
  state.settings = { ...state.settings, [key]: value };
  try {
    localStorage.setItem(SETTINGS_KEY, JSON.stringify(state.settings));
  } catch {
    throw new UserError('This browser would not keep the setting: it holds on this page until it is closed or reloaded.');
  }
}

const DATE_WORDS = {
  'yyyy-mm-dd': 'year-month-day',
  'dd.mm.yyyy': 'day.month.year',
  'dd/mm/yyyy': 'day/month/year',
  'mm/dd/yyyy': 'month/day/year',
  'd mmm yyyy': 'day month year',
  'mmm d, yyyy': 'month day, year',
};

/**
 * The user's own name for each folder of theirs, kept on the server, so it holds wherever they sign
 * in: emptied, or made the folder's name, they see the folder's name again.
 */
function folderNames() {
  const rows = state.me.folders.map((f) => {
    const input = h('input', {
      type: 'text', class: 'grow', value: f.name || '', placeholder: f.folder, maxlength: 64,
      'aria-label': `Your name for the folder ${f.folder}`,
    });
    const save = act(async (ev) => {
      ev.preventDefault();
      const name = input.value.trim();
      if (name === (f.name || '')) return;
      if (!name || name === f.folder) {
        await api.del(`/v1/ns/${enc(f.folder)}/name`);
        f.name = undefined;
      } else {
        f.name = (await api.put(`/v1/ns/${enc(f.folder)}/name`, { name })).name;
      }
      input.value = f.name || '';
      renderSidebar(parseRoute());
      toast(f.name ? `You see ${f.folder} as “${f.name}” now.` : `You see ${f.folder} by its own name now.`);
    });
    return h('form', { class: 'form-row', onsubmit: save },
      h('span', { class: 'folder-id mono', title: 'The folder\'s name, which devices, links and other people use', text: f.folder }),
      input,
      h('button', { class: 'btn', type: 'submit' }, 'Save'));
  });
  return h('div', { class: 'panel' },
    h('h2', { text: 'Folder names' }),
    h('p', { class: 'muted', text: 'What you see each folder called, wherever you sign in. Yours alone: the folder keeps its name, which devices, links and other people use. Empty, you see that name.' }),
    rows.length ? rows : h('p', { class: 'empty', text: 'No folder is yours yet.' }));
}

function renderSettings(gen) {
  // A day past the 12th, so that the order of each date shows.
  const sample = new Date(new Date().getFullYear(), 2, 25, 16, 52);
  const now = h('strong');
  const showNow = () => { now.textContent = fmtTime(Date.now() / 1000); };
  const group = (title, key, values, label, about) => h('div', { class: 'panel', role: 'radiogroup', 'aria-label': title },
    h('h2', { text: title }),
    values.map((v) => h('label', { class: 'choice' },
      h('input', {
        type: 'radio',
        name: key,
        value: v,
        checked: state.settings[key] === v,
        onchange: act(() => {
          try { saveSetting(key, v); } finally { showNow(); }
        }),
      }),
      h('span', { class: 'choice-label', text: label(v) }),
      h('span', { class: 'muted', text: about(v) }))));
  showNow();
  show(gen, h('section', { class: 'settings' },
    h('div', { class: 'screen-head' }, h('h1', { text: 'Settings' })),
    folderNames(),
    h('p', { class: 'muted', text: 'How times are shown. Kept in this browser, for whoever signs in on it.' }),
    group('Clock', 'clock', CLOCKS, (v) => (v === '12h' ? '12-hour' : '24-hour'), (v) => clockText(sample, v)),
    group('Dates', 'date', DATE_ORDERS, (v) => dayText(sample, v), (v) => DATE_WORDS[v]),
    h('p', {}, 'A time now shows as ', now, '.')), 'Settings');
}

// Settings changed in another tab of this browser: the page on screen shows its times the new way.
window.addEventListener('storage', (ev) => {
  if (ev.key !== SETTINGS_KEY && ev.key !== null) return;
  state.settings = loadSettings();
  refresh();
});

// ---------------------------------------------------------------- public links

async function startPublic() {
  const token = decodeURIComponent(location.pathname.slice(3).split('/')[0] || '');
  const view = linkView(token);
  state.publicView = view;
  const main = h('main', { class: 'main public', id: 'main' });
  $app().replaceChildren(
    h('header', { class: 'topbar' }, brand(false), h('span', { class: 'spacer' }), h('span', { class: 'muted', text: 'Shared with you' })),
    main);
  shell = { main, layout: null, sidebar: null };
  try {
    view.info = await api.get(view.base);
  } catch (e) {
    main.replaceChildren(e.status === 404
      ? h('div', { class: 'gone' }, h('h1', { text: 'This link does not exist or has expired.' }), h('p', { class: 'muted', text: 'Ask whoever sent it for a new one.' }))
      : errorBox(e));
    return;
  }
  window.addEventListener('hashchange', () => routePublic());
  routePublic();
}

async function routePublic() {
  const view = state.publicView;
  if (!view || !view.info) return;
  closeMenu();
  const gen = ++state.gen;
  const m = location.hash.match(/^#\/o\/([0-9a-f]{32}|root)$/);
  const obj = m ? m[1] : 'root';
  const { item, expires } = view.info;
  try {
    if (item.kind !== 'dir') {
      show(gen, fileView(view, item, [{ display: item.display }], 'read', expires), item.display);
      return;
    }
    await renderBrowser(gen, view, obj);
  } catch (e) {
    show(gen, e.status === 404
      ? h('div', { class: 'gone' }, h('h1', { text: 'Not found' }), h('p', { class: 'muted', text: 'This link does not reach it, or it is gone.' }), h('a', { href: '#/o/root', text: 'Back to the shared folder' }))
      : errorBox(e), 'Error');
  }
}

// ---------------------------------------------------------------- start

window.addEventListener('beforeunload', (ev) => {
  if (!uploads.busy()) return;
  ev.preventDefault();
  ev.returnValue = '';
});
window.addEventListener('scroll', () => closeMenu(), true);
window.addEventListener('resize', () => closeMenu());
// A file dropped beside the list must not make the browser leave the page to open it.
window.addEventListener('dragover', (ev) => { if ([...(ev.dataTransfer?.types || [])].includes('Files')) ev.preventDefault(); });
window.addEventListener('drop', (ev) => { if ([...(ev.dataTransfer?.types || [])].includes('Files')) ev.preventDefault(); });

if (IS_PUBLIC) {
  startPublic();
} else {
  // A page already open in this tab gets a sign-in address as a change of its fragment alone.
  window.addEventListener('hashchange', () => {
    if (location.hash.startsWith('#/signin/')) signInByTicket().then(start);
  });
  signInByTicket().then(start);
}
