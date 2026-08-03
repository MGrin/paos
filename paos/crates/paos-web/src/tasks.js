/* The kanban board.
 *
 * Dependency-free, and textContent everywhere — NEVER innerHTML. Every string rendered
 * here (titles, bodies, note text, handles) is arbitrary text written by other sessions,
 * so the safe primitive is the only primitive. Same rule as index.html, same reason.
 *
 * Exposes one global, `TasksView`, which index.html calls when its nav lands on 'tasks'.
 */
(function () {
  const COLUMNS = [
    ['proposed', 'Proposed'],
    ['ready', 'Ready'],
    ['in_progress', 'In progress'],
    ['review', 'Review'],
    ['done', 'Done'],
  ];

  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
  };

  const state = {
    data: null,
    filters: { repo: '', owner: '', scope: '', orphaned: false, q: '' },
    swimlanes: false,
    // Everything that must survive a poll. A refresh that discarded a half-typed comment
    // or cancelled a drag is how a live board becomes one people close.
    dragging: null,
    openId: null,
    dirty: false,
    timer: null,
  };

  function readHash() {
    const h = new URLSearchParams((location.hash || '').replace(/^#/, ''));
    if (h.get('view') !== 'tasks') return;
    state.filters.repo = h.get('repo') || '';
    state.filters.owner = h.get('owner') || '';
    state.filters.scope = h.get('scope') || '';
    state.filters.orphaned = h.get('orphaned') === '1';
    state.filters.q = h.get('q') || '';
    state.swimlanes = h.get('lanes') === '1';
  }

  /** Filters live in the URL so a filtered board is a link you can send yourself. */
  function writeHash() {
    const p = new URLSearchParams({ view: 'tasks' });
    const f = state.filters;
    if (f.repo) p.set('repo', f.repo);
    if (f.owner) p.set('owner', f.owner);
    if (f.scope) p.set('scope', f.scope);
    if (f.orphaned) p.set('orphaned', '1');
    if (f.q) p.set('q', f.q);
    if (state.swimlanes) p.set('lanes', '1');
    history.replaceState(null, '', '#' + p.toString());
  }

  async function api(path, opts) {
    const r = await fetch(path, opts);
    if (!r.ok) throw new Error((await r.text()) || r.status);
    return r.json();
  }

  const post = (path, data) =>
    api(path, {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams(data).toString(),
    });

  function toast(msg) {
    const t = el('div', 'toast', msg);
    document.body.append(t);
    setTimeout(() => t.remove(), 4200);
  }

  function visible(t) {
    const f = state.filters;
    if (f.repo && t.repo !== f.repo) return false;
    if (f.owner && t.claimed_by !== f.owner) return false;
    if (f.scope && t.scope !== f.scope) return false;
    if (f.orphaned && !(t.orphaned && t.unowned)) return false;
    if (f.q) {
      const hay = (t.title + ' ' + (t.body || '') + ' ' + t.id).toLowerCase();
      if (!hay.includes(f.q.toLowerCase())) return false;
    }
    return true;
  }

  // ---- cards ---------------------------------------------------------------

  function card(t) {
    const c = el('div', 'card' + (t.rescue ? ' rescue' : ''));
    c.draggable = true;
    c.dataset.id = t.id;

    c.addEventListener('dragstart', (e) => {
      state.dragging = t.id;
      c.classList.add('dragging');
      e.dataTransfer.setData('text/plain', t.id);
      e.dataTransfer.effectAllowed = 'move';
    });
    c.addEventListener('dragend', () => {
      state.dragging = null;
      c.classList.remove('dragging');
    });
    c.addEventListener('click', (e) => {
      if (e.target.tagName === 'SELECT') return;
      openDetail(t.id);
    });

    c.append(el('div', 't', t.title));

    const meta = el('div', 'meta');
    meta.append(el('span', 'pill p' + t.priority, 'p' + t.priority));
    if (t.rescue) {
      meta.append(el('span', 'pill rescue', '⤺ rescue' + (t.last_owner ? ' · ' + t.last_owner : '')));
    } else if (t.claimed_by) {
      meta.append(el('span', 'pill owner', '@' + t.claimed_by));
    } else if (t.unowned && t.last_owner) {
      meta.append(el('span', 'pill', 'released'));
    }
    if (t.blocked) meta.append(el('span', 'pill blocked', '⛔ blocked'));
    if (t.repo) meta.append(el('span', 'pill', t.repo));
    if (t.notes) meta.append(el('span', 'pill', '💬 ' + t.notes));

    // The accessible path, and the only one that works on a phone — which is where the
    // operator answers from. Drag is the flourish; this is the interface.
    const sel = el('select');
    for (const [v, label] of COLUMNS) {
      const o = el('option', null, label);
      o.value = v;
      if (v === t.state) o.selected = true;
      sel.append(o);
    }
    const dropped = el('option', null, 'Dropped');
    dropped.value = 'dropped';
    if (t.state === 'dropped') dropped.selected = true;
    sel.append(dropped);
    sel.onchange = () => move(t.id, sel.value, t.state);
    meta.append(sel);

    c.append(meta);
    return c;
  }

  /** Optimistic, with a rollback. The SERVER decides whether a move is legal. */
  async function move(id, to, from) {
    if (to === from) return;
    try {
      await post('/api/task/state', { id, to });
      await load();
    } catch (e) {
      toast(String(e.message || e));
      await load(); // snap the card back to whatever the server actually thinks
    }
  }

  // ---- board ---------------------------------------------------------------

  function boardFor(tasks) {
    const board = el('div', 'board');
    for (const [key, label] of COLUMNS) {
      const col = el('div', 'col');
      const mine = tasks.filter((t) => t.state === key);
      const h = el('h3');
      h.append(el('span', null, label), el('span', 'n', String(mine.length)));
      col.append(h);

      const drop = el('div', 'drop');
      drop.addEventListener('dragover', (e) => {
        e.preventDefault();
        drop.classList.add('over');
      });
      drop.addEventListener('dragleave', () => drop.classList.remove('over'));
      drop.addEventListener('drop', (e) => {
        e.preventDefault();
        drop.classList.remove('over');
        const id = e.dataTransfer.getData('text/plain') || state.dragging;
        const t = state.data.tasks.find((x) => x.id === id);
        if (t) move(id, key, t.state);
      });
      for (const t of mine) drop.append(card(t));
      col.append(drop);
      board.append(col);
    }
    return board;
  }

  function render(root) {
    root.replaceChildren();
    if (!state.data) {
      root.append(el('div', 'empty', 'loading…'));
      return;
    }
    root.append(controls());

    const tasks = state.data.tasks.filter(visible);
    if (!tasks.length) {
      root.append(el('div', 'empty', 'no tasks match — clear the filters, or create one above'));
      return;
    }

    if (!state.swimlanes) {
      root.append(boardFor(tasks));
    } else {
      const byId = new Map(state.data.tasks.map((t) => [t.id, t]));
      const epics = new Map();
      const loose = [];
      for (const t of tasks) {
        if (t.parent_id && byId.has(t.parent_id)) {
          if (!epics.has(t.parent_id)) epics.set(t.parent_id, []);
          epics.get(t.parent_id).push(t);
        } else {
          loose.push(t);
        }
      }
      for (const [pid, kids] of epics) {
        root.append(lane(byId.get(pid).title, kids, pid));
      }
      if (loose.length) root.append(lane('Unparented', loose, '_loose'));
    }
    if (state.openId) openDetail(state.openId, true);
  }

  function lane(title, tasks, key) {
    const wrap = el('div', 'lane');
    const open = sessionStorage.getItem('lane:' + key) !== 'closed';
    wrap.dataset.open = String(open);
    const h = el('h2');
    h.append(el('span', 'caret', open ? '▼' : '▶'), el('span', null, title),
             el('span', 'n', String(tasks.length)));
    h.onclick = () => {
      const now = wrap.dataset.open !== 'true';
      wrap.dataset.open = String(now);
      sessionStorage.setItem('lane:' + key, now ? 'open' : 'closed');
      h.firstChild.textContent = now ? '▼' : '▶';
    };
    wrap.append(h, boardFor(tasks));
    return wrap;
  }

  function controls() {
    const bar = el('div', 'board-bar');

    const add = el('input');
    add.placeholder = 'new task — type a title and press Enter';
    add.onkeydown = async (e) => {
      if (e.key !== 'Enter' || !add.value.trim()) return;
      try {
        await post('/api/task/create', {
          title: add.value.trim(),
          scope: state.filters.repo ? 'project' : 'global',
          repo: state.filters.repo || '',
        });
        add.value = '';
        await load();
      } catch (err) {
        toast(String(err.message || err));
      }
    };
    bar.append(add);

    const pick = (label, key, values) => {
      const s = el('select');
      const any = el('option', null, label);
      any.value = '';
      s.append(any);
      for (const v of values) {
        const o = el('option', null, v);
        o.value = v;
        if (state.filters[key] === v) o.selected = true;
        s.append(o);
      }
      s.onchange = () => {
        state.filters[key] = s.value;
        writeHash();
        render(document.getElementById('app'));
      };
      return s;
    };
    bar.append(pick('any repo', 'repo', state.data.repos));
    bar.append(pick('anyone', 'owner', state.data.owners));
    bar.append(pick('any scope', 'scope', ['global', 'org', 'project']));

    const orph = el('button', 'chip', '⤺ unowned only');
    orph.setAttribute('aria-pressed', String(state.filters.orphaned));
    orph.onclick = () => {
      state.filters.orphaned = !state.filters.orphaned;
      writeHash();
      render(document.getElementById('app'));
    };
    bar.append(orph);

    const lanes = el('button', 'chip', 'swimlanes');
    lanes.setAttribute('aria-pressed', String(state.swimlanes));
    lanes.onclick = () => {
      state.swimlanes = !state.swimlanes;
      writeHash();
      render(document.getElementById('app'));
    };
    bar.append(lanes);

    const q = el('input');
    q.placeholder = 'filter…';
    q.value = state.filters.q;
    q.style.maxWidth = '160px';
    q.oninput = () => {
      state.filters.q = q.value;
      writeHash();
      const app = document.getElementById('app');
      const scroll = app.scrollTop;
      render(app);
      app.scrollTop = scroll;
      const again = document.querySelector('.board-bar input:last-of-type');
      if (again) { again.focus(); again.setSelectionRange(q.value.length, q.value.length); }
    };
    bar.append(el('span', 'spacer'), q);
    return bar;
  }

  // ---- detail panel --------------------------------------------------------

  async function openDetail(id, silent) {
    state.openId = id;
    let d;
    try {
      d = await api('/api/task?id=' + encodeURIComponent(id));
    } catch (e) {
      if (!silent) toast(String(e.message || e));
      state.openId = null;
      return;
    }
    document.querySelectorAll('.panel, .backdrop').forEach((n) => n.remove());

    const back = el('div', 'backdrop');
    back.onclick = closeDetail;
    const p = el('div', 'panel');
    const t = d.task;

    const x = el('button', 'act close', 'close');
    x.onclick = closeDetail;
    p.append(x);

    p.append(el('div', 'k', t.id + '  ·  ' + t.state + '  ·  p' + t.priority));
    p.append(el('h3', null, t.title));

    // Why it is unowned goes ABOVE the body: someone picking this up needs to know what
    // they are inheriting before they read what the work is.
    if (t.unowned && t.state !== 'done' && t.state !== 'dropped' && t.last_owner) {
      p.append(el('div', 'why', t.orphaned
        ? '⤺ Unowned — ' + t.last_owner + ' ended while holding this. Open to rescue.'
        : '⤺ Unowned — released by ' + t.last_owner + '.'));
    }

    const meta = [];
    meta.push('scope ' + t.scope);
    if (t.repo) meta.push('repo ' + t.repo);
    if (t.claimed_by) meta.push('held by ' + t.claimed_by);
    meta.push('created by ' + t.created_by + ' (' + t.origin + ')');
    if (t.close_grant) meta.push('sessions may close this');
    p.append(el('div', 'fix', meta.join(' · ')));

    if (t.body) p.append(el('div', 'txt', t.body));

    if (d.deps.length) {
      p.append(el('h2', null, 'depends on'));
      for (const dep of d.deps) {
        const row = el('div', 'fix', dep.id + ' [' + dep.state + '] ' + dep.title);
        p.append(row);
      }
    }

    if (d.notes.length) {
      p.append(el('h2', null, 'log'));
      const log = el('div', 'log');
      for (const n of d.notes) {
        const e = el('div', 'entry ' + (n.kind || 'note'));
        e.append(el('div', 'who', n.ts + '  ' + n.author));
        e.append(el('div', 'txt', n.text));
        log.append(e);
      }
      p.append(log);
    }

    p.append(el('h2', null, 'comment'));
    const box = el('textarea');
    box.placeholder = 'a note on this task…';
    box.oninput = () => { state.dirty = box.value.trim().length > 0; };
    p.append(box);

    const wakeWrap = el('label');
    const wake = el('input');
    wake.type = 'checkbox';
    wake.checked = !!t.claimed_by;
    wake.style.width = 'auto';
    wakeWrap.append(wake, document.createTextNode(
      t.claimed_by ? 'wake ' + t.claimed_by : 'wake the owner (nobody holds this)'));
    p.append(wakeWrap);

    const acts = el('div', 'acts');
    const send = el('button', 'act primary', 'comment');
    send.onclick = async () => {
      if (!box.value.trim()) return;
      try {
        const r = await post('/api/task/note', {
          id: t.id, text: box.value.trim(), ...(wake.checked ? { wake: '1' } : {}),
        });
        box.value = '';
        state.dirty = false;
        toast(r.delivered ? 'commented and woke ' + t.claimed_by
                          : 'commented — ' + (r.why || 'not delivered'));
        openDetail(t.id, true);
      } catch (e) {
        toast(String(e.message || e));
      }
    };
    acts.append(send);

    if (t.origin === 'operator' && !t.close_grant) {
      const g = el('button', 'act', 'let a session close this');
      g.onclick = async () => {
        try {
          await post('/api/task/grant', { id: t.id });
          toast('granted');
          openDetail(t.id, true);
          load();
        } catch (e) { toast(String(e.message || e)); }
      };
      acts.append(g);
    }
    p.append(acts);

    document.body.append(back, p);
    if (!silent) box.focus();
  }

  function closeDetail() {
    state.openId = null;
    state.dirty = false;
    document.querySelectorAll('.panel, .backdrop').forEach((n) => n.remove());
  }

  // ---- lifecycle -----------------------------------------------------------

  async function load() {
    state.data = await api('/api/tasks');
    const badge = document.getElementById('tasks-badge');
    if (badge) {
      badge.textContent = state.data.needs_operator || '';
      badge.style.display = state.data.needs_operator ? '' : 'none';
    }
    render(document.getElementById('app'));
  }

  const TasksView = {
    async show() {
      readHash();
      writeHash();
      await load();
      clearInterval(state.timer);
      // Poll, but never on top of the user. A drag in flight or a half-typed comment is
      // work in progress, and clobbering it costs more than five seconds of staleness.
      state.timer = setInterval(() => {
        if (state.dragging || state.dirty) return;
        load().catch(() => {});
      }, 5000);
    },
    hide() {
      clearInterval(state.timer);
      state.timer = null;
      closeDetail();
    },
    async badge() {
      try {
        const d = await api('/api/tasks');
        return d.needs_operator || 0;
      } catch (e) {
        return 0;
      }
    },
  };

  window.TasksView = TasksView;
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && state.openId) closeDetail();
  });
})();
