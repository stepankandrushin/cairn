// The tray's window: draws what `snapshot` says each second, and passes a person's choices back.
// Everything shown is set as text, never as markup, since paths and reasons come from outside.
'use strict';

const invoke = window.__TAURI__.core.invoke;
let state = null;
let tab = 'status';
let seenTab = 0;
let seenNotice = 0;
let drawn = '';
// Conflicts awaiting a confirmation: "rootId/seq/action".
const confirming = new Set();
// Conflicts with an action under way, so their buttons wait.
const busy = new Set();
let conflictsAskedAt = 0;

function el(tag, attrs, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === 'class') e.className = v;
    else if (k === 'onclick') e.addEventListener('click', v);
    else if (k === 'disabled') e.disabled = !!v;
    else e.setAttribute(k, v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    e.append(typeof c === 'string' || typeof c === 'number' ? document.createTextNode(String(c)) : c);
  }
  return e;
}

function when(secs) {
  if (!secs) return 'never';
  const d = new Date(secs * 1000);
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (d.toDateString() === new Date().toDateString()) return time;
  return d.toLocaleDateString([], { day: 'numeric', month: 'short' }) + ' ' + time;
}

function notice(text) {
  document.getElementById('notice-text').textContent = text;
  document.getElementById('notice').hidden = !text;
}

document.getElementById('notice-close').addEventListener('click', () => notice(''));
for (const b of document.querySelectorAll('#tabs button')) {
  b.addEventListener('click', () => setTab(b.dataset.tab));
}

function setTab(t) {
  tab = t;
  if (t === 'conflicts') askConflicts();
  draw(true);
}

// Each answer has the server replay its journal, so only when a person opens the tab or acts, and not
// twice within a few seconds of clicking.
async function askConflicts() {
  if (Date.now() - conflictsAskedAt < 3000) return;
  conflictsAskedAt = Date.now();
  if (!state) return;
  for (const r of state.roots.filter((r) => r.up)) {
    invoke('refresh_conflicts', { rootId: r.root_id }).catch(() => {});
  }
}

async function call(cmd, args) {
  try {
    const msg = await invoke(cmd, args);
    if (msg) notice(msg);
    return true;
  } catch (e) {
    notice(String(e));
    return false;
  }
}

async function tick() {
  try {
    state = await invoke('snapshot');
  } catch (e) {
    notice('The tray cannot read its own state: ' + e);
    return;
  }
  if (state.tab[0] !== seenTab) {
    seenTab = state.tab[0];
    if (state.tab[1]) setTab(state.tab[1]);
  }
  if (state.notice[0] !== seenNotice) {
    seenNotice = state.notice[0];
    if (state.notice[1]) notice(state.notice[1]);
  }
  draw(false);
}

function counts() {
  let problems = 0;
  let conflicts = 0;
  for (const r of state.roots) {
    problems += r.problems.length;
    if (r.conflicts) conflicts += r.conflicts.open.length;
  }
  return { problems, conflicts };
}

function draw(force) {
  if (!state) return;
  const key = JSON.stringify([tab, { ...state, now: 0 }, [...confirming], [...busy]]);
  if (!force && key === drawn) return;
  drawn = key;
  const { problems, conflicts } = counts();
  document.getElementById('overall-dot').className = 'dot ' + state.light;
  document.getElementById('overall-text').textContent =
    state.roots.length === 0 ? 'Cairn: no roots' : 'Cairn: ' + state.light_word;
  for (const b of document.querySelectorAll('#tabs button')) {
    b.classList.toggle('on', b.dataset.tab === tab);
    const n = b.dataset.tab === 'problems' ? problems : b.dataset.tab === 'conflicts' ? conflicts : 0;
    const label = { status: 'Status', recent: 'Recent activity', problems: 'Problems', conflicts: 'Conflicts' }[b.dataset.tab];
    b.textContent = n ? `${label} (${n})` : label;
  }
  const main = document.getElementById('content');
  const body = { status: drawStatus, recent: drawRecent, problems: drawProblems, conflicts: drawConflicts }[tab]();
  main.replaceChildren(...body);
}

function noRoots() {
  return [el('div', { class: 'card' },
    el('div', {}, state.registry_error || 'No roots on this device yet.'),
    el('div', { class: 'todo' }, 'A root appears here once `cairn init` or `cairn clone` made it, or its daemon started.'))];
}

function title(r) {
  return [el('h2', {}, el('span', { class: 'dot ' + r.light }), r.name), el('div', { class: 'path' }, r.path)];
}

function drawStatus() {
  if (state.roots.length === 0) return noRoots();
  return state.roots.map((r) => el('div', { class: 'card' },
    title(r),
    el('div', { class: 'headline' }, r.headline),
    el('div', { class: 'meta' },
      [r.server && 'server ' + r.server, r.version && 'daemon ' + r.version].filter(Boolean).join(' · ')),
    el('div', { class: 'buttons' },
      el('button', { class: 'act', disabled: !r.up, onclick: () => call('act', { rootId: r.root_id, what: 'sync_now' }) }, 'Sync now'),
      r.paused_by_you
        ? el('button', { class: 'act', disabled: !r.up, onclick: () => call('act', { rootId: r.root_id, what: 'resume' }) }, 'Resume')
        : el('button', { class: 'act', disabled: !r.up, onclick: () => call('act', { rootId: r.root_id, what: 'pause' }) }, 'Pause'),
      el('button', { class: 'act', onclick: () => call('open_folder', { rootId: r.root_id }) }, 'Open folder'),
      el('button', { class: 'act', disabled: !r.server, onclick: () => call('open_server', { rootId: r.root_id }) }, 'Open in browser'))));
}

const CHANGE = {
  added: 'added', changed: 'changed', metadata: 'metadata changed', moved: 'moved', removed: 'removed',
  conflict: 'conflict copy made',
};

function drawRecent() {
  if (state.roots.length === 0) return noRoots();
  return state.roots.map((r) => el('div', { class: 'card' },
    title(r),
    !r.up ? el('div', { class: 'empty' }, 'Its daemon is not running, so its activity cannot be read.')
      : r.recent.length === 0 ? el('div', { class: 'empty' }, 'Nothing synced since the daemon started.')
        : el('table', {}, r.recent.map((a) => el('tr', {},
          el('td', { class: 'time' }, when(a.time)),
          el('td', { class: 'dir', title: a.direction === 'up' ? 'sent from here' : 'from the server' },
            a.direction === 'up' ? '↑ sent' : '↓ came'),
          el('td', { class: 'change' }, CHANGE[a.change] || a.change),
          el('td', { class: 'p' }, a.to ? `${a.path} → ${a.to}` : a.path))))));
}

function drawProblems() {
  if (state.roots.length === 0) return noRoots();
  return state.roots.map((r) => el('div', { class: 'card' },
    title(r),
    r.problems.length === 0 ? el('div', { class: 'empty' }, 'No problems.')
      : el('ul', { class: 'list' }, r.problems.map((p) => el('li', {},
        el('div', {}, p.what), p.todo && el('div', { class: 'todo' }, p.todo))))));
}

function conflictButtons(r, c) {
  const k = (a) => `${r.root_id}/${c.seq}/${a}`;
  const waiting = busy.has(`${r.root_id}/${c.seq}`);
  const run = async (action) => {
    confirming.delete(k(action));
    busy.add(`${r.root_id}/${c.seq}`);
    draw(true);
    await call('conflict', { rootId: r.root_id, seq: c.seq, what: action });
    busy.delete(`${r.root_id}/${c.seq}`);
    conflictsAskedAt = 0;
    askConflicts();
    draw(true);
  };
  const ask = (action) => { confirming.clear(); confirming.add(k(action)); draw(true); };
  const cancel = () => { confirming.clear(); draw(true); };
  const out = [el('div', { class: 'buttons' },
    el('button', { class: 'act', disabled: waiting || !(c.copy_here || c.original_here), onclick: () => run('show') }, 'Show in folder'),
    el('button', { class: 'act', disabled: waiting || !(c.copy_here || c.original_here), onclick: () => run('open_both') }, 'Open both'),
    el('button', { class: 'act', disabled: waiting || !c.copy_here, onclick: () => ask('keep_original') }, 'Keep the original…'),
    el('button', { class: 'act', disabled: waiting || !c.can_swap, onclick: () => ask('keep_copy') }, 'Keep the copy…'))];
  if (confirming.has(k('keep_original'))) {
    out.push(el('div', { class: 'confirm' },
      el('div', {}, `${c.copy_display} goes to the Trash, and the original stays. The server keeps both versions in its history.`),
      el('div', { class: 'buttons' },
        el('button', { class: 'act danger', onclick: () => run('keep_original') }, 'Move the copy to the Trash'),
        el('button', { class: 'act', onclick: cancel }, 'Cancel'))));
  }
  if (confirming.has(k('keep_copy'))) {
    out.push(el('div', { class: 'confirm' },
      el('div', {}, `${c.original_display} goes to the Trash, and the copy takes its name. The server keeps both versions in its history.`),
      el('div', { class: 'buttons' },
        el('button', { class: 'act danger', onclick: () => run('keep_copy') }, 'Put the copy in its place'),
        el('button', { class: 'act', onclick: cancel }, 'Cancel'))));
  }
  return out;
}

function drawConflicts() {
  if (state.roots.length === 0) return noRoots();
  return state.roots.map((r) => {
    let body;
    if (!r.up) body = el('div', { class: 'empty' }, 'Its daemon is not running, so its conflicts cannot be listed.');
    else if (!r.conflicts) body = el('div', { class: 'empty' }, 'Asking its daemon…');
    else if (r.conflicts.error) body = el('div', { class: 'empty' }, 'Cannot list them: ' + r.conflicts.error);
    else if (r.conflicts.open.length === 0) body = el('div', { class: 'empty' }, 'No conflict waits for you.');
    else {
      body = el('ul', { class: 'list' }, r.conflicts.open.map((c) => el('li', {},
        el('div', {}, c.copy_display),
        el('div', { class: 'todo' },
          `the competing version of ${c.original_display || 'what is since deleted'}, from ${c.device_name} at ${when(c.time)}`
          + (c.copy_here ? '' : ' · the copy is not on this device')),
        conflictButtons(r, c))));
    }
    return el('div', { class: 'card' }, title(r),
      r.conflicts && !r.conflicts.error ? el('div', { class: 'meta' }, 'listed at ' + when(r.conflicts.at)) : null,
      body);
  });
}

setInterval(tick, 1000);
tick();
