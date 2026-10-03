/* Styx Web —— 前端逻辑
 *
 * 一条原则：**服务端的 transcript 是唯一真相**。
 *
 * 所以每次回合结束后不做"局部插入新气泡"，而是重新拉一次事件流、
 * 整体重建舞台，只给 seq 大于上次所见的事件加入场动画与打字机。
 * 这样刷新页面、切会话、点重置，走的是同一条渲染路径，
 * 不会出现"刷新后消息顺序变了"这类只有前端才知道的状态。
 */

'use strict';

const el = (id) => document.getElementById(id);
const esc = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) =>
    ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const fmt = (n) => (typeof n === 'number' ? (n >= 0 ? '+' : '') + n.toFixed(2) : '—');

const S = {
  session: localStorage.getItem('styx.session') || 'web',
  card: '',
  stickers: [],
  byId: new Map(),
  lastSeq: 0,
  busy: false,
  typing: !window.matchMedia('(prefers-reduced-motion: reduce)').matches,
};

/* ------------------------------------------------------------------ 网络 */

async function api(path, opts = {}) {
  const qs = opts.query ? '?' + new URLSearchParams(opts.query).toString() : '';
  const init = { method: opts.method || (opts.body ? 'POST' : 'GET') };
  if (opts.body) {
    init.headers = { 'Content-Type': 'application/json' };
    init.body = JSON.stringify(opts.body);
  }
  const res = await fetch(path + qs, init);
  const text = await res.text();
  let data;
  try {
    data = JSON.parse(text);
  } catch {
    throw new Error(`服务端返回了非 JSON（HTTP ${res.status}）：${text.slice(0, 120)}`);
  }
  return data;
}

/* ------------------------------------------------------------------ 启动 */

async function boot() {
  setConn('busy');
  try {
    const b = await api('/api/bootstrap', { query: { session: S.session } });
    const hello = b.hello || {};
    if (hello.ok === false) throw new Error(hello.error || '握手失败');

    S.session = hello.session || S.session;
    S.card = hello.card || b.hello?.card || '角色';
    localStorage.setItem('styx.session', S.session);

    document.title = `${S.card} · Styx`;
    el('card-name').textContent = S.card;

    applyStickers(b.stickers);
    applyState(b.state);
    applyScene(b.scene);
    applyStatus(b.status, hello);
    renderEvents(b.events?.events || [], 0);
    setConn('online');
  } catch (e) {
    setConn('offline');
    toast('接不上服务：' + e.message);
    el('scene-line').textContent = '未连接';
    renderEvents([], 0);
  }
}

/* ------------------------------------------------------------ 舞台渲染 */

function welcomeHTML() {
  const who = esc(S.card || '角色');
  const n = S.stickers.length;
  const stickerTip = n
    ? `点左下角的 ☺ 可以从 <b>${n}</b> 张表情包里挑一张发过去 —— 角色看得懂，也会用它回你。`
    : `把 PNG 放进 <code>${esc(el('sticker-dir')?.textContent || 'web/stickers')}</code> 就能发表情包了。`;
  return `<div class="welcome">
    <h1>${who} 已经就位</h1>
    <p>直接说话就是在演戏 —— 你说的话会作为剧情的一部分。</p>
    <p class="hint">${stickerTip}</p>
  </div>`;
}

/**
 * 用事件流重建整个舞台。
 *
 * @param {Array} events 服务端返回的事件（按 seq 升序）
 * @param {number} animateAfter 只给 seq 大于它的事件做入场动画
 */
function renderEvents(events, animateAfter) {
  const stage = el('stage');
  stage.innerHTML = '';
  if (!events || !events.length) {
    stage.innerHTML = welcomeHTML();
    return;
  }

  const actor = S.card || '角色';
  let current = null; // 当前正在累积的角色气泡

  for (const e of events) {
    const animated = animateAfter > 0 && e.seq > animateAfter;
    switch (e.kind) {
      case 'user_input':
        current = null;
        stage.appendChild(userBubble(e, animated));
        break;
      case 'speech':
      case 'action':
      case 'thought':
      case 'sticker':
        if (!current) {
          current = newActorBubble(actor, animated);
          stage.appendChild(current.root);
        }
        fillActorLine(current.body, e, animated);
        break;
      case 'scene_change':
        current = null;
        stage.appendChild(sceneNote(e, animated));
        break;
      default:
        // system / tool_call / tool_result / memory_write 不进舞台：
        // 它们是给机器看的，混进来只会让表演变得嘈杂。
        break;
    }
  }
  scrollBottom();
}

function avatar(text) {
  const a = document.createElement('div');
  a.className = 'avatar';
  a.textContent = text || '·';
  return a;
}

function userBubble(e, animated) {
  const wrap = document.createElement('div');
  wrap.className = 'msg user' + (animated ? ' enter' : '');
  wrap.appendChild(avatar('我'));

  const body = document.createElement('div');
  body.className = 'body';

  const sid = e.meta && e.meta.sticker_id;
  if (sid) {
    body.appendChild(stickerNode(sid, (e.meta && e.meta.sticker_label) || '表情包'));
  } else {
    const p = document.createElement('p');
    p.className = 'line';
    p.textContent = e.text;
    body.appendChild(p);
  }

  wrap.appendChild(body);
  return wrap;
}

function newActorBubble(actor, animated) {
  const root = document.createElement('div');
  root.className = 'msg actor' + (animated ? ' enter' : '');
  root.appendChild(avatar([...actor][0] || '角'));
  const body = document.createElement('div');
  body.className = 'body';
  root.appendChild(body);
  return { root, body };
}

function fillActorLine(body, e, animated) {
  if (e.kind === 'sticker') {
    const id = (e.meta && e.meta.sticker_id) || '';
    body.appendChild(stickerNode(id, e.text || '表情包'));
    return;
  }
  const p = document.createElement('p');
  p.className = 'line ' + e.kind;
  if (e.kind === 'speech' && animated) {
    typeInto(p, e.text);
  } else {
    p.textContent = e.text;
  }
  body.appendChild(p);
}

/** 表情包节点：找得到图就显示图，找不到就退回一句可读的说明。 */
function stickerNode(id, label) {
  const meta = S.byId.get(id);
  if (!meta) {
    const span = document.createElement('span');
    span.className = 'sticker-missing';
    span.textContent = `［表情包：${label || id}（未加载）］`;
    return span;
  }
  const fig = document.createElement('div');
  const img = document.createElement('img');
  img.className = 'sticker-img';
  img.src = meta.url;
  img.alt = meta.label;
  img.loading = 'lazy';
  img.title = `${meta.label} —— ${meta.description}`;
  fig.appendChild(img);

  const cap = document.createElement('div');
  cap.className = 'sticker-caption';
  cap.textContent = meta.label;
  fig.appendChild(cap);
  return fig;
}

function sceneNote(e, animated) {
  const d = document.createElement('div');
  d.className = 'scene-note' + (animated ? ' enter' : '');
  const s = document.createElement('span');
  s.textContent = e.text;
  d.appendChild(s);
  return d;
}

/** 打字机：只对新到的台词生效，长文本按比例提速。 */
function typeInto(node, text) {
  if (!S.typing || !text) {
    node.textContent = text || '';
    return;
  }
  if (text.length > 400) {
    node.textContent = text;
    return;
  }
  node.textContent = '';
  const step = Math.max(1, Math.ceil(text.length / 80));
  let i = 0;
  const tick = () => {
    i = Math.min(text.length, i + step);
    node.textContent = text.slice(0, i);
    if (i < text.length) {
      keepAtBottom();
      setTimeout(tick, 20);
    }
  };
  tick();
}

/* ------------------------------------------------------------ 回合驱动 */

async function sendText() {
  const box = el('input');
  const text = box.value.trim();
  if (!text || S.busy) return;
  box.value = '';
  autoGrow();
  await runTurn('/api/say', { text }, { text });
}

async function sendSticker(id) {
  if (!id || S.busy) return;
  await runTurn('/api/sticker', { id }, { stickerId: id });
}

/**
 * 跑一个回合：乐观显示 → 打请求 → 用服务端事件流重建舞台。
 */
async function runTurn(path, payload, optimistic) {
  S.busy = true;
  setBusy(true);
  const before = S.lastSeq;

  appendOptimistic(optimistic);
  showThinking();

  try {
    const r = await api(path, { method: 'POST', body: { ...payload, session: S.session } });
    if (r.ok === false) throw new Error(r.error || '这个回合没能完成');
    applyTurn(r);
  } catch (e) {
    toast(e.message);
  } finally {
    hideThinking();
    await refreshEvents(before);
    S.busy = false;
    setBusy(false);
    el('input').focus();
  }
}

/** 用户输入先落地一个临时气泡，避免"按下回车后毫无反应"。 */
function appendOptimistic(o) {
  if (!o) return;
  const stage = el('stage');
  const welcome = stage.querySelector('.welcome');
  if (welcome) welcome.remove();

  const fake = { seq: -1, kind: 'user_input', actor: '我', text: '', meta: {} };
  if (o.stickerId) {
    fake.meta = { sticker_id: o.stickerId, sticker_label: S.byId.get(o.stickerId)?.label || '' };
  } else {
    fake.text = o.text;
  }
  stage.appendChild(userBubble(fake, true));
  scrollBottom();
}

async function refreshEvents(since) {
  try {
    const r = await api('/api/events', { query: { limit: 200, session: S.session } });
    const events = r.events || [];
    if (events.length) S.lastSeq = events[events.length - 1].seq;
    renderEvents(events, since);
  } catch {
    /* 拉不到历史不致命：这一回合的内容已经由 applyTurn 展示过了 */
  }
}

function showThinking() {
  if (el('thinking')) return;
  const d = document.createElement('div');
  d.id = 'thinking';
  d.className = 'thinking';
  d.innerHTML = `<div class="avatar">${esc([...(S.card || '角')][0])}</div>
    <span>${esc(S.card || '对方')} 正在斟酌</span>
    <span class="dots"><i></i><i></i><i></i></span>`;
  el('stage').appendChild(d);
  scrollBottom();
}

function hideThinking() {
  el('thinking')?.remove();
}

/* ------------------------------------------------------------ 侧栏更新 */

function applyTurn(r) {
  applyState(r.state);
  applyScene(r.scene);

  const rows = [];
  rows.push(`<b>第 ${r.turn} 回合</b>`);
  if (r.state_delta) rows.push(`状态：${esc(r.state_delta)}`);
  if (r.scene_changed && r.scene_changed.length)
    rows.push(`场景：${esc(r.scene_changed.join('；'))}`);
  if (r.prompt) rows.push(esc(r.prompt));
  if (r.usage)
    rows.push(
      `${esc(r.usage.model)} @ ${esc(r.usage.endpoint)} · ` +
        `提示 ${r.usage.prompt_tokens} / 生成 ${r.usage.completion_tokens} tok`
    );
  if (r.memory_written) rows.push(`写入长期记忆 ${r.memory_written} 条`);
  if (r.pool_written) rows.push(`写入共享记忆池 ${r.pool_written} 条`);
  if (r.retries) rows.push(`一致性重试 ${r.retries} 次`);
  for (const n of r.notices || []) rows.push(`⚠ ${esc(n)}`);
  el('report').innerHTML = rows.join('<br>');

  renderChips('memory-box', (r.recalled || []).map((m) => m.text), 'accent');
  renderChips(
    'assoc-box',
    (r.associations || []).map((a) => `${a.word}`),
    ''
  );

  for (const v of (r.audit && r.audit.violations) || []) toast('违反角色设定：' + v);
}

function applyState(st) {
  if (!st || st.ok === false) return;
  const mood = st.mood || {};
  el('mood-line').innerHTML = `${esc(mood.label || '—')} <span class="v">${fmt(mood.valence)} / ${fmt(
    mood.arousal
  )}</span>`;

  meter('energy', st.energy, 0, 1, false);
  meter('tension', st.tension, 0, 1, false);
  meter('arousal', mood.arousal, 0, 1, false);
  meter('valence', mood.valence, -1, 1, true);

  const names = new Set([
    ...Object.keys(st.affinity || {}),
    ...Object.keys(st.trust || {}),
  ]);
  const box = el('relations');
  if (!names.size) {
    box.className = 'relations muted';
    box.textContent = '暂无';
  } else {
    box.className = 'relations';
    box.innerHTML = [...names]
      .map((n) => {
        const a = (st.affinity || {})[n] ?? 0;
        const t = (st.trust || {})[n] ?? 0.5;
        return `<div class="rel"><span>${esc(n)}</span>
          <span class="num ${a >= 0 ? 'pos' : 'neg'}">好感 ${fmt(a)} · 信任 ${fmt(t)}</span></div>`;
      })
      .join('');
  }
}

function meter(key, value, min, max, bipolar) {
  const bar = el('m-' + key);
  const out = el('v-' + key);
  if (!bar || !out) return;
  if (typeof value !== 'number' || Number.isNaN(value)) {
    out.textContent = '—';
    bar.style.width = '0';
    return;
  }
  const span = max - min;
  const ratio = Math.max(0, Math.min(1, (value - min) / span));
  out.textContent = fmt(value);
  if (bipolar) {
    // 以中点为 0：正向向右长，负向向左长
    const half = Math.abs(value) / Math.max(Math.abs(min), Math.abs(max));
    bar.style.width = (half * 50).toFixed(1) + '%';
    bar.style.marginLeft = (value >= 0 ? 50 : 50 - half * 50).toFixed(1) + '%';
  } else {
    bar.style.marginLeft = '0';
    bar.style.width = (ratio * 100).toFixed(1) + '%';
  }
}

function applyScene(sc) {
  if (!sc || sc.ok === false) return;
  const rows = [
    ['世界', sc.world],
    ['地点', sc.location],
    ['时间', sc.time],
    ['天气', sc.weather],
    ['局面', sc.situation],
    ['基调', sc.tone],
    ['在场', (sc.present || []).join('、')],
    ['身份', sc.user_role],
  ].filter(([, v]) => v && String(v).trim());

  el('scene-box').innerHTML = rows.length
    ? rows.map(([k, v]) => `<b>${k}</b> ${esc(v)}`).join('<br>')
    : '—';
  el('scene-line').textContent =
    [sc.location, sc.time].filter(Boolean).join(' · ') || '（未设定场景）';
}

function applyStatus(st, hello) {
  if (!st || st.ok === false) return;
  const rows = [
    `模型 ${esc(st.llm)}（${st.llm_endpoints ?? 0} 端点）`,
    `长期记忆 ${esc(st.memory)}`,
    `联想 ${esc(st.assoc)}`,
    `共享池 ${esc(st.pool || '（未启用）')}`,
    `工具 ${esc(st.tools || '（未启用）')}`,
    `表情包 ${esc(st.stickers || '（未加载）')}`,
    `会话 ${esc(st.turn ?? 0)} 回合 / ${st.transcript ?? 0} 事件`,
  ];
  if (st.degraded && st.degraded.length) rows.push(`⚠ 降级：${esc(st.degraded.join('；'))}`);
  if (hello && hello.factory) rows.push(`工厂 ${esc(hello.factory)}`);
  el('status-box').innerHTML = rows.join('<br>');
}

function renderChips(target, items, cls) {
  const box = el(target);
  const list = (items || []).filter(Boolean).slice(0, 8);
  box.innerHTML = list.length
    ? list.map((t) => `<span class="chip ${cls}">${esc(t)}</span>`).join('')
    : '<span class="chip">（空）</span>';
}

/* ------------------------------------------------------------ 表情面板 */

function applyStickers(payload) {
  S.stickers = (payload && payload.stickers) || [];
  S.byId = new Map(S.stickers.map((s) => [s.id, s]));
  const dir = (payload && payload.dir) || 'web/stickers';
  const dirNode = el('sticker-dir');
  if (dirNode) dirNode.textContent = dir;

  el('sticker-empty').hidden = S.stickers.length > 0;
  el('sticker-hint').textContent = S.stickers.length ? `${S.stickers.length} 张` : '';
  renderStickerGrid('');
}

function renderStickerGrid(query) {
  const grid = el('sticker-grid');
  const kw = (query || '').trim().toLowerCase();
  const list = kw
    ? S.stickers.filter((s) =>
        `${s.id} ${s.label} ${s.emotion} ${(s.tags || []).join(' ')} ${s.description}`
          .toLowerCase()
          .includes(kw)
      )
    : S.stickers;

  grid.innerHTML = '';
  for (const s of list) {
    const b = document.createElement('button');
    b.className = 'sticker-btn';
    b.type = 'button';
    b.title = `${s.label} —— ${s.description}`;
    b.innerHTML = `<img src="${esc(s.url)}" alt="${esc(s.label)}" loading="lazy"><span>${esc(s.label)}</span>`;
    b.addEventListener('click', () => {
      toggleStickers(false);
      sendSticker(s.id);
    });
    grid.appendChild(b);
  }
  if (kw && !list.length) {
    const d = document.createElement('div');
    d.className = 'empty';
    d.textContent = '没有匹配的表情包';
    grid.appendChild(d);
  }
}

function toggleStickers(force) {
  const panel = el('sticker-panel');
  const btn = el('btn-stickers');
  const show = force === undefined ? panel.hidden : force;
  panel.hidden = !show;
  btn.classList.toggle('active', show);
  if (show) el('sticker-search').focus();
}

/* ---------------------------------------------------------------- 杂项 */

function setConn(state) {
  const d = el('conn-dot');
  d.className = 'dot ' + state;
  d.title = { online: '已连接', busy: '正在演', offline: '未连接' }[state] || '';
}

function setBusy(busy) {
  el('btn-send').disabled = busy;
  if (busy) setConn('busy');
  else if (el('conn-dot').classList.contains('busy')) setConn('online');
}

let toastTimer = null;
function toast(msg) {
  const t = el('toast');
  t.textContent = msg;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 4200);
}

function scrollBottom() {
  const s = el('stage');
  s.scrollTop = s.scrollHeight;
}

/** 只在用户本来就在底部时才跟随，避免打断上翻阅读。 */
function keepAtBottom() {
  const s = el('stage');
  if (s.scrollHeight - s.scrollTop - s.clientHeight < 160) scrollBottom();
}

function autoGrow() {
  const box = el('input');
  box.style.height = 'auto';
  box.style.height = Math.min(box.scrollHeight, 168) + 'px';
}

/* ------------------------------------------------------------------ 绑定 */

function bind() {
  el('btn-send').addEventListener('click', sendText);

  const box = el('input');
  box.addEventListener('input', autoGrow);
  box.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      sendText();
    }
  });

  el('btn-stickers').addEventListener('click', () => toggleStickers());
  el('sticker-search').addEventListener('input', (e) => renderStickerGrid(e.target.value));

  el('btn-reset').addEventListener('click', async () => {
    if (S.busy) return;
    if (!confirm('清空这一场戏？角色的状态、场景与剧情都会回到起点（记忆保留）。')) return;
    try {
      const r = await api('/api/reset', { method: 'POST', body: { session: S.session } });
      if (r.ok === false) throw new Error(r.error || '重置失败');
      S.lastSeq = 0;
      await boot();
      toast('已重置。');
    } catch (e) {
      toast(e.message);
    }
  });

  el('btn-inspect').addEventListener('click', () => {
    el('inspector').classList.toggle('open');
  });

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') toggleStickers(false);
    if (e.ctrlKey && (e.key === 'e' || e.key === 'E')) {
      e.preventDefault();
      toggleStickers();
    }
    if (e.ctrlKey && (e.key === 'i' || e.key === 'I')) {
      e.preventDefault();
      el('inspector').classList.toggle('open');
    }
  });
}

bind();
boot();
