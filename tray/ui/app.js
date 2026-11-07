// The tray's window: draws what `snapshot` says each second, and passes a person's choices back.
// Everything shown is set as text, never as markup, since paths and reasons come from outside.
'use strict';

const invoke = window.__TAURI__.core.invoke;
let state = null;
let tab = 'status';
let seenTab = 0;
let seenNotice = 0;
let drawn = '';
// Actions awaiting a confirmation: "rootId/seq/action" for a conflict, "rootId/forget" for a root.
const confirming = new Set();
// Conflicts and roots with an action under way, so their buttons wait.
const busy = new Set();
let conflictsAskedAt = 0;
// Recently deleted, per root: its daemon's last answer. Asked for only when the tab opens, the period
// changes, Refresh is pressed or something was brought back, as each first answer has the daemon read
// the server's whole history. Kept apart from the snapshot, which comes every second.
const deleted = {};
let deletedDays = 30;
let deletedQuery = '';
let deletedShown = 200;
let deletedVersion = 0;
let deletedToken = 0;
// "rootId/id" of what is ticked.
const picked = new Set();

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
  if (t === 'deleted') askDeleted(false);
  draw(true);
}

function askDeleted(force, only) {
  if (!state) return;
  for (const r of state.roots.filter((r) => r.up && (!only || r.root_id === only))) {
    const cur = deleted[r.root_id];
    // A recent answer for the same period does, unless asked again.
    if (!force && cur && (cur.loading || (cur.days === deletedDays && Date.now() - cur.asked < 60000))) continue;
    const token = ++deletedToken;
    const days = deletedDays;
    deleted[r.root_id] = { ...(cur || {}), loading: true, days, token, asked: Date.now() };
    deletedVersion++;
    invoke('deleted', { rootId: r.root_id, days })
      .then((list) => {
        if (deleted[r.root_id].token === token) deleted[r.root_id] = { loading: false, days, token, asked: Date.now(), list };
      })
      .catch((e) => {
        if (deleted[r.root_id].token === token) {
          deleted[r.root_id] = { ...deleted[r.root_id], loading: false, error: String(e) };
        }
      })
      .finally(() => { deletedVersion++; draw(true); });
  }
}

document.getElementById('deleted-search').addEventListener('input', (e) => {
  deletedQuery = e.target.value.trim().toLowerCase();
  deletedShown = 200;
  draw(true);
});
document.getElementById('deleted-days').addEventListener('change', (e) => {
  deletedDays = Number(e.target.value);
  picked.clear();
  askDeleted(true);
  draw(true);
});
document.getElementById('deleted-refresh').addEventListener('click', () => { askDeleted(true); draw(true); });

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

const TABS = {
  status: 'Status', recent: 'Recent activity', problems: 'Problems', conflicts: 'Conflicts',
  deleted: 'Recently deleted', settings: 'Settings',
};

function draw(force) {
  if (!state) return;
  // The deleted list can be long: drawn again only when it, the roots or a choice changes, not with
  // every snapshot.
  const key = tab === 'deleted'
    ? JSON.stringify([tab, state.light, state.roots.map((r) => [r.root_id, r.name, r.path, r.light, r.up,
      r.problems.length, r.conflicts && r.conflicts.open.length]), deletedVersion, deletedQuery, deletedShown,
    [...picked], [...busy]])
    : JSON.stringify([tab, { ...state, now: 0 }, [...confirming], [...busy]]);
  if (!force && key === drawn) return;
  drawn = key;
  document.getElementById('deleted-bar').hidden = tab !== 'deleted';
  const { problems, conflicts } = counts();
  document.getElementById('overall-dot').className = 'dot ' + state.light;
  document.getElementById('overall-text').textContent =
    state.roots.length === 0 ? 'Cairn: no roots' : 'Cairn: ' + state.light_word;
  for (const b of document.querySelectorAll('#tabs button')) {
    b.classList.toggle('on', b.dataset.tab === tab);
    const n = b.dataset.tab === 'problems' ? problems : b.dataset.tab === 'conflicts' ? conflicts : 0;
    const label = TABS[b.dataset.tab];
    b.textContent = n ? `${label} (${n})` : label;
  }
  const main = document.getElementById('content');
  const body = {
    status: drawStatus, recent: drawRecent, problems: drawProblems, conflicts: drawConflicts,
    deleted: drawDeleted, settings: drawSettings,
  }[tab]();
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
      el('button', { class: 'act', disabled: !r.server, onclick: () => call('open_server', { rootId: r.root_id }) }, 'Open in browser')),
    forgetControls(r)));
}

// A root shown as gone: taken off this device's list once confirmed, as `cairn forget` does. Once done,
// its buttons stay waiting until the registry, read again every few seconds, no longer lists it.
function forgetControls(r) {
  if (!r.gone) return [];
  const k = `${r.root_id}/forget`;
  const waiting = busy.has(k);
  const run = async () => {
    confirming.delete(k);
    busy.add(k);
    draw(true);
    if (!(await call('forget', { rootId: r.root_id }))) busy.delete(k);
    draw(true);
  };
  const ask = () => { confirming.clear(); confirming.add(k); draw(true); };
  const cancel = () => { confirming.clear(); draw(true); };
  const out = [el('div', { class: 'buttons' },
    el('button', { class: 'act', disabled: waiting, onclick: ask }, 'Forget…'))];
  if (confirming.has(k)) {
    out.push(el('div', { class: 'confirm' },
      el('div', {}, `${r.path} comes off this device's list of roots. Nothing in it or on the server changes, and should the root come back there, its daemon lists it again when it starts.`),
      el('div', { class: 'buttons' },
        el('button', { class: 'act danger', disabled: waiting, onclick: run }, 'Take it off the list'),
        el('button', { class: 'act', onclick: cancel }, 'Cancel'))));
  }
  return out;
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
          el('td', { class: 'time' }, a.when),
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
        el('div', {}, p.what), p.todo && el('div', { class: 'todo' }, p.todo)))),
    forgetControls(r)));
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
          `the competing version of ${c.original_display || 'what is since deleted'}, from ${c.device_name} at ${c.when}`
          + (c.copy_here ? '' : ' · the copy is not on this device')),
        conflictButtons(r, c))));
    }
    return el('div', { class: 'card' }, title(r),
      r.conflicts && !r.conflicts.error ? el('div', { class: 'meta' }, 'listed at ' + r.conflicts.at_when) : null,
      body);
  });
}

function matches(item) {
  if (!deletedQuery) return true;
  return item.path.toLowerCase().includes(deletedQuery) || item.device_name.toLowerCase().includes(deletedQuery);
}

// Brings these back through the daemon, which says where they came back; then the list is asked anew.
async function undelete(r, ids) {
  const k = `${r.root_id}/undelete`;
  busy.add(k);
  draw(true);
  await call('undelete', { rootId: r.root_id, ids });
  busy.delete(k);
  for (const id of ids) picked.delete(`${r.root_id}/${id}`);
  askDeleted(true, r.root_id);
  draw(true);
}

function drawDeleted() {
  if (state.roots.length === 0) return noRoots();
  return state.roots.map((r) => {
    const d = deleted[r.root_id];
    const waiting = busy.has(`${r.root_id}/undelete`);
    let body;
    let head = null;
    if (!r.up) body = el('div', { class: 'empty' }, 'Its daemon is not running, so what was deleted cannot be listed.');
    else if (!d || (d.loading && !d.list)) {
      body = el('div', { class: 'empty' },
        'Asking its daemon, which reads the server\'s history for it: the first time can take a minute.');
    } else if (!d.list) body = el('div', { class: 'empty' }, 'Cannot list it: ' + d.error);
    else {
      const all = d.list.items.filter(matches);
      const shown = all.slice(0, deletedShown);
      const mine = d.list.items.filter((i) => picked.has(`${r.root_id}/${i.id}`)).map((i) => i.id);
      head = el('div', { class: 'meta' },
        `${d.list.items.length} deleted in the last ${d.days} days`
        + (deletedQuery ? `, ${all.length} matching` : '')
        + ` · listed at ${d.list.at_when}` + (d.loading ? ' · asking again…' : '')
        + (d.error ? ' · could not list it again: ' + d.error : ''));
      if (d.list.items.length === 0) body = el('div', { class: 'empty' }, 'Nothing to restore.');
      else if (all.length === 0) body = el('div', { class: 'empty' }, 'Nothing deleted matches.');
      else {
        body = [
          el('div', { class: 'buttons' },
            el('button', { class: 'act', disabled: waiting || mine.length === 0, onclick: () => undelete(r, mine) },
              mine.length ? `Restore selected (${mine.length})` : 'Restore selected'),
            el('button', {
              class: 'act', disabled: waiting,
              onclick: () => { for (const i of shown) picked.add(`${r.root_id}/${i.id}`); draw(true); },
            }, 'Select all shown'),
            mine.length ? el('button', {
              class: 'act', disabled: waiting,
              onclick: () => { for (const id of mine) picked.delete(`${r.root_id}/${id}`); draw(true); },
            }, 'Clear selection') : null),
          el('table', {}, shown.map((i) => {
            const k = `${r.root_id}/${i.id}`;
            const box = el('input', Object.assign({
              type: 'checkbox', title: 'Select',
              onclick: (e) => { if (e.target.checked) picked.add(k); else picked.delete(k); draw(true); },
            }, picked.has(k) ? { checked: '' } : {}));
            return el('tr', {},
              el('td', { class: 'pick' }, box),
              el('td', { class: 'what' },
                el('div', {}, i.kind === 'dir' ? i.path + '/' : i.path),
                el('div', { class: 'meta' },
                  `${i.size} · deleted ${i.when} by ${i.device_name}`
                  + (i.folder_gone ? ' · its folder is deleted too and comes back with it' : ''))),
              el('td', { class: 'go' },
                el('button', { class: 'act', disabled: waiting, onclick: () => undelete(r, [i.id]) }, 'Restore')));
          })),
          all.length > shown.length
            ? el('div', { class: 'buttons' },
              el('button', { class: 'act', onclick: () => { deletedShown += 200; draw(true); } },
                `Show more (${all.length - shown.length} not shown)`))
            : null,
        ];
      }
    }
    return el('div', { class: 'card' }, title(r), head, body,
      el('div', { class: 'note' },
        'Restore brings it back on every device as it was when deleted. Nothing is replaced: where its name is '
        + 'taken now, it comes back beside it under another. If an antivirus removed it, it may remove it again '
        + 'once it is back here; tell the antivirus to allow it.'));
  });
}

// Times come from the tray already written on the clock chosen here, as the menu shows them.
function drawSettings() {
  const choice = (value, label, example) => el('label', { class: 'choice' },
    el('input', Object.assign({
      type: 'radio', name: 'clock', value,
      // Drawn again either way, so a choice the tray could not keep shows unchosen.
      onclick: async () => { await call('set_clock', { clock: value }); await tick(); draw(true); },
    }, state.clock === value ? { checked: '' } : {})),
    label, el('span', { class: 'meta' }, example));
  return [drawUpdates(), el('div', { class: 'card' },
    el('h2', {}, 'Clock'),
    el('div', { class: 'choices' },
      choice('24h', '24-hour', '16:52'),
      choice('12h', '12-hour', '4:52 PM')),
    state.settings_error ? el('div', { class: 'todo' }, state.settings_error + '; the defaults are in use until a choice here writes it anew.') : null,
    state.settings_path ? el('div', { class: 'meta' }, 'Kept in ' + state.settings_path) : null)];
}

// One updater for every daemon of this user here and the tray beside them: what it last found, and how
// it is set. Update now asks a daemon, which installs whatever the setting.
function drawUpdates() {
  const u = state.update;
  const choice = (value, label, about) => el('label', { class: 'choice' },
    el('input', Object.assign({
      type: 'radio', name: 'updates', value,
      onclick: async () => { await call('set_update_policy', { policy: value }); await tick(); draw(true); },
    }, u.policy === value ? { checked: '' } : {})),
    label, el('span', { class: 'meta' }, about));
  const versions = [...new Set(state.roots.filter((r) => r.up && r.version).map((r) => r.version))];
  const lines = [
    el('div', { class: 'meta' }, 'This tray ' + u.tray_version
      + (versions.length ? '; daemons ' + versions.join(', ') : '')),
    u.checked_when
      ? el('div', { class: 'meta' }, 'Last asked ' + u.checked_when + (u.latest ? ': the latest is ' + u.latest : ''))
      : el('div', { class: 'meta' }, 'Not asked yet'),
  ];
  if (u.available) lines.push(el('div', { class: 'todo' }, u.available + ' is available' + (u.waits ? ': ' + u.waits : '')));
  else if (u.waits) lines.push(el('div', { class: 'meta' }, u.waits));
  if (u.installed && u.installed_when) lines.push(el('div', { class: 'meta' }, u.installed + ' installed ' + u.installed_when));
  if (u.error) lines.push(el('div', { class: 'todo' }, 'Could not update' + (u.error_when ? ' since ' + u.error_when : '') + ': ' + u.error));
  if (u.rejected.length) lines.push(el('div', { class: 'meta' }, 'Taken back here, not installed again unless asked: ' + u.rejected.join(', ')));
  if (u.problem) lines.push(el('div', { class: 'todo' }, u.problem));
  return el('div', { class: 'card' },
    el('h2', {}, 'Updates'),
    el('div', { class: 'choices' },
      choice('auto', 'Automatic', 'install each release as it comes'),
      choice('notify', 'Notify only', 'show it here and in the menu'),
      choice('off', 'Off', 'never ask')),
    ...lines,
    el('div', { class: 'buttons' },
      el('button', {
        class: 'act', disabled: !u.can_ask,
        title: u.can_ask ? '' : 'No daemon is running here to ask',
        onclick: async () => { await call('update_now', {}); await tick(); draw(true); },
      }, 'Update now')));
}

setInterval(tick, 1000);
tick();
