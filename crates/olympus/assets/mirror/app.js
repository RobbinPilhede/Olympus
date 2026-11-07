"use strict";
const $ = (id) => document.getElementById(id);
const TOKEN_KEY = "olympus-key";
const SAFE_LINK = /^(https?:|mailto:)/i;
const EXAMPLES = [
  ["/auto add login rate limiting", "Plan, build in parallel, verify, review"],
  ["@codex is this approach sound?", "Ask another model without leaving this chat"],
  ["/council which queue should we use?", "Several models, one recommendation"],
  ["/claude", "The real Claude Code, in a window"],
];

const store = {
  get() { try { return localStorage.getItem(TOKEN_KEY); } catch { return null; } },
  set(value) { try { localStorage.setItem(TOKEN_KEY, value); } catch { /* the key then lasts until this page closes */ } },
  clear() { try { localStorage.removeItem(TOKEN_KEY); } catch { /* nothing was kept */ } },
};

const state = {
  token: store.get(), boot: null, meta: null, threads: [], picker: null, status: "", limits: [],
  current: null, view: null, attachments: [], hintIndex: 0, open: new Set(), lastLoadedRev: null, loading: false,
};

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function toast(text) {
  const box = $("toast");
  box.textContent = text;
  box.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => { box.hidden = true; }, 4000);
}

async function copyText(text) {
  // navigator.clipboard only exists on HTTPS, and this page is served over plain HTTP.
  if (navigator.clipboard) {
    try { await navigator.clipboard.writeText(text); toast("Copied"); return; } catch { /* fall through */ }
  }
  const area = el("textarea");
  area.value = text;
  document.body.append(area);
  area.select();
  const done = document.execCommand("copy");
  area.remove();
  toast(done ? "Copied" : "Your browser blocked copying here.");
}

async function api(path, options = {}) {
  const headers = { ...(options.headers ?? {}) };
  if (state.token) headers.Authorization = `Bearer ${state.token}`;
  const response = await fetch(path, { ...options, headers });
  if (response.status === 401) { lockOut(); throw new Error("not paired"); }
  return response;
}

const apiJson = async (path, options) => (await api(path, options)).json();
const post = (body) => apiJson("/api/do", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });

function lockOut() {
  store.clear();
  state.token = null;
  $("app").hidden = true;
  $("pair").hidden = false;
}

function applyReply(reply) {
  if (reply.flash) toast(reply.flash);
  if (reply.prefill != null) { $("input").value = reply.prefill; autosize(); $("input").focus(); showHints(); }
  if (reply.open != null) { pendingOpen = reply.open; openPending(); }
}

let pendingOpen = null;
function openPending() {
  if (pendingOpen != null && state.threads.some((t) => t.id === pendingOpen)) { selectThread(pendingOpen); pendingOpen = null; }
}

/* ---------- pairing ---------- */

async function pair(code) {
  const response = await fetch("/api/pair", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ code, name: navigator.userAgent }) });
  if (!response.ok) return false;
  const { token } = await response.json();
  state.token = token;
  store.set(token);
  return true;
}

function pairFailed() {
  const error = $("pair-error");
  error.textContent = "That code didn't work. Check it on your PC. After five wrong tries it shows a new one.";
  error.hidden = false;
}

$("pair-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  if (await pair($("pair-code").value)) start(); else pairFailed();
});

/* ---------- live connection ---------- */

async function listen() {
  for (;;) {
    try {
      const response = await api("/api/events");
      if (!response.ok) throw new Error(String(response.status));
      setLink("");
      const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
      let buffer = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += value;
        let cut;
        while ((cut = buffer.indexOf("\n\n")) >= 0) {
          onFrame(buffer.slice(0, cut));
          buffer = buffer.slice(cut + 2);
        }
      }
    } catch (error) {
      if (!state.token) return;
    }
    setLink("Reconnecting…");
    await new Promise((resolve) => setTimeout(resolve, 2000));
  }
}

function setLink(text) {
  $("link").textContent = text;
  $("link").hidden = !text;
}

function onFrame(frame) {
  let name = "message";
  let data = "";
  for (const line of frame.split("\n")) {
    if (line.startsWith("event:")) name = line.slice(6).trim();
    else if (line.startsWith("data:")) data += line.slice(5).trimStart();
  }
  if (name === "hello") {
    // A new id means Olympus restarted, and every thread id and revision with it.
    if (state.boot && state.boot !== data) { state.view = null; state.current = null; }
    state.boot = data;
    return;
  }
  if (data) onSummary(JSON.parse(data));
}

function onSummary(summary) {
  state.threads = summary.threads;
  state.status = summary.status;
  state.limits = summary.limits;
  state.picker = summary.picker;
  state.reading = summary.reading;
  if (state.current == null || !state.threads.some((t) => t.id === state.current)) {
    const fallback = summary.threads.find((t) => t.id === summary.active && t.kind !== "obi") ?? summary.threads.find((t) => t.kind === "chat") ?? summary.threads[0];
    if (fallback) selectThread(fallback.id);
  }
  renderThreads();
  renderFooter();
  renderPicker();
  renderComposerState();
  openPending();
  const thread = currentThread();
  if (thread && thread.rev !== state.lastLoadedRev) refreshCurrent();
  if (thread?.kind === "obi") window.Obi?.onSummary();
}

const currentThread = () => state.threads.find((t) => t.id === state.current);

/* ---------- sidebar ---------- */

function markFor(thread) {
  const box = el("span", "thread-mark");
  const known = { "claude.svg": "mark-claude", "openai.svg": "mark-openai", "jarvis.svg": "mark-jarvis" }[thread.mark];
  if (known) {
    const mark = el("span", `mark ${known}`);
    if (thread.activity === "running") mark.classList.add("mark-running");
    box.append(mark);
    if (thread.activity === "unseen" || thread.activity === "failed") box.append(el("span", `dot ${thread.activity}`));
  } else if (thread.activity === "running") {
    box.append(el("span", "glyph-live", "●"));
  } else if (thread.activity === "unseen") {
    box.append(el("span", "glyph-ok", "●"));
  } else if (thread.activity === "failed") {
    box.append(el("span", "glyph-failed", "✗"));
  } else {
    box.append(el("span", "glyph-dim", "·"));
  }
  return box;
}

function threadRow(thread) {
  const row = el("div", `thread${thread.id === state.current ? " on" : ""}`);
  row.setAttribute("role", "button");
  row.tabIndex = 0;
  row.addEventListener("click", () => selectThread(thread.id));
  row.addEventListener("keydown", (event) => { if (event.key === "Enter") selectThread(thread.id); });
  row.append(markFor(thread));
  const text = el("span", "thread-text");
  text.append(el("span", "thread-title", thread.title));
  text.append(el("span", "thread-line", thread.who));
  if (thread.details.length) text.append(el("span", `thread-line${thread.alert ? " alert" : ""}`, thread.details.join(" · ")));
  if (thread.used != null) {
    const bar = el("span", `usage${thread.alert ? " alert" : ""}`);
    const fill = el("i");
    fill.style.width = `${Math.round(thread.used * 100)}%`;
    bar.append(fill);
    text.append(bar);
  }
  row.append(text);
  if (thread.kind !== "obi") {
    const close = el("button", "thread-close", "×");
    close.setAttribute("aria-label", `Close ${thread.title}`);
    close.addEventListener("click", (event) => {
      event.stopPropagation();
      post({ do: "close", thread: thread.id }).then(applyReply);
    });
    row.append(close);
  } else {
    const toggle = el("button", `switch${state.reading ? " on" : ""}`);
    toggle.setAttribute("role", "switch");
    toggle.setAttribute("aria-checked", String(!!state.reading));
    toggle.setAttribute("aria-label", "Obiview reading");
    toggle.append(el("i"));
    toggle.addEventListener("click", (event) => {
      event.stopPropagation();
      apiJson("/api/obi", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ do: "toggle" }) }).catch(() => toast("Unable to reach Olympus."));
    });
    row.append(toggle);
  }
  return row;
}

function renderThreads() {
  const pin = state.threads.findIndex((t) => t.kind === "obi");
  const pinned = $("obi-row");
  pinned.replaceChildren();
  if (pin >= 0) {
    const row = threadRow(state.threads[pin]);
    row.classList.add("obi-pin");
    pinned.append(row);
  }
  const box = $("threads");
  const top = box.scrollTop;
  box.replaceChildren(...state.threads.filter((t) => t.kind !== "obi").map((t) => threadRow(t)));
  box.scrollTop = top;
  const index = state.threads.findIndex((t) => t.id === state.current);
  $("position").textContent = index >= 0 ? `${index + 1}/${state.threads.length}` : "";
}

function renderFooter() {
  const right = $("foot-right");
  right.replaceChildren();
  if (state.status) right.append(el("span", "foot-status", state.status));
  for (const limit of state.limits) {
    const percent = Math.round(limit.used * 100);
    const hot = limit.blocked || percent >= 90;
    const meter = el("span", `meter${hot ? " hot" : ""}`, `${limit.vendor} ${limit.window} ${percent}%${limit.blocked ? " limit hit" : ""}`);
    const bar = el("i");
    const fill = el("b");
    fill.style.width = `${Math.min(100, percent)}%`;
    bar.append(fill);
    meter.append(bar);
    right.append(meter);
  }
}

function closeDrawer() {
  $("sidebar").classList.remove("open");
  $("scrim").hidden = true;
  $("menu").setAttribute("aria-expanded", "false");
}

$("menu").addEventListener("click", () => {
  const open = $("sidebar").classList.toggle("open");
  $("scrim").hidden = !open;
  $("menu").setAttribute("aria-expanded", String(open));
});
$("scrim").addEventListener("click", closeDrawer);

/* ---------- transcript ---------- */

function spansInto(parent, spans) {
  for (const span of spans ?? []) {
    let node;
    if (span.href && SAFE_LINK.test(span.href)) {
      node = el("a", "", span.t);
      node.href = span.href;
      node.target = "_blank";
      node.rel = "noopener noreferrer";
    } else if (span.href && window.Obi?.isNoteLink(span.href)) {
      node = el("a", "wiki", span.t);
      node.addEventListener("click", () => window.Obi.openLink(span.href));
    } else {
      node = el("span", `t-${span.tone}`, span.t);
    }
    for (const flag of ["b", "i", "s", "c"]) if (span[flag]) node.classList.add(`s-${flag}`);
    parent.append(node);
  }
  return parent;
}

function glyph(status, state_) {
  if (status === "running" || ["working", "checking", "merging"].includes(state_)) return el("span", "glyph-live", "●");
  if (status === "ok" || state_ === "done") return el("span", "glyph-ok", "✓");
  if (status === "failed" || state_ === "failed") return el("span", "glyph-failed", "✗");
  if (state_ === "skipped") return el("span", "glyph-dim", "–");
  return el("span", "glyph-dim", "○");
}

function copyButton(text, className = "") {
  const button = el("button", `control dim ${className}`, "Copy");
  button.addEventListener("click", (event) => { event.stopPropagation(); copyText(text); });
  return button;
}

async function pictureNode(part) {
  const image = el("img", "p-picture");
  image.alt = "Attached picture";
  image.width = Math.round(part.w);
  image.height = Math.round(part.h);
  image.style.height = "auto";
  const response = await api(`/api/image?path=${encodeURIComponent(part.path)}`);
  if (response.ok) image.src = URL.createObjectURL(await response.blob());
  return image;
}

function plain(spans) {
  return (spans ?? []).map((span) => span.t).join("");
}

function partNode(part, blockKey) {
  switch (part.k) {
    case "user": {
      const box = el("div", "p-user");
      box.append(el("b", "", "›"));
      box.append(spansInto(el("div"), part.spans));
      return box;
    }
    case "label": case "phase": case "para": return spansInto(el("div", "p-para"), part.spans);
    case "heading": return spansInto(el("div", `p-h${Math.min(part.level, 3)}`), part.spans);
    case "item": {
      const box = el("div", "p-item");
      box.style.paddingLeft = `${part.depth * 18}px`;
      box.append(el("b", "", part.marker));
      box.append(spansInto(el("div"), part.spans));
      return box;
    }
    case "quote": return spansInto(el("div", "p-quote"), part.spans);
    case "rule": return el("div", "p-rule");
    case "code": {
      const box = el("div", "p-code");
      const head = el("div", "code-head");
      head.append(el("span", "", part.lang));
      head.append(copyButton(plain(part.spans)));
      box.append(head);
      box.append(spansInto(el("pre"), part.spans));
      return box;
    }
    case "table": {
      const box = el("div", "p-table");
      const table = el("table");
      for (const row of part.rows) {
        const line = el("tr");
        for (const cell of row) line.append(spansInto(el("td"), cell));
        table.append(line);
      }
      box.append(table);
      return box;
    }
    case "tool": {
      const box = el("div", `p-tool${state.open.has(blockKey) ? " open" : ""}`);
      const head = el(part.detail ? "button" : "div", "p-tool-head");
      head.append(glyph(part.status));
      head.append(spansInto(el("div", "p-tool-text"), part.spans));
      if (part.detail) {
        const toggle = el("span", "p-tool-toggle", state.open.has(blockKey) ? "Hide" : "Details");
        head.append(toggle);
        head.addEventListener("click", () => {
          if (state.open.has(blockKey)) state.open.delete(blockKey); else state.open.add(blockKey);
          box.replaceWith(partNode(part, blockKey));
        });
      }
      box.append(head);
      if (part.detail && state.open.has(blockKey)) box.append(el("div", "p-tool-detail", part.detail));
      return box;
    }
    case "note": {
      const box = el("div", "p-note");
      box.append(el("span", "glyph-dim", "·"));
      box.append(spansInto(el("div"), part.spans));
      return box;
    }
    case "error": {
      const box = el("div", "p-error");
      box.append(el("span", "glyph-failed", "✗"));
      box.append(spansInto(el("div"), part.spans));
      return box;
    }
    case "task": {
      const box = el("div", "p-task");
      box.append(glyph(null, part.state));
      box.append(spansInto(el("div"), part.spans));
      return box;
    }
    case "picture": {
      const holder = el("div");
      pictureNode(part).then((image) => holder.append(image)).catch(() => holder.append(el("span", "t-dim", "Unable to load this picture.")));
      return holder;
    }
    default: return el("div");
  }
}

function blockNode(block) {
  const key = `${state.current}:${block.i}`;
  const row = el("div", `row${block.tool && block.after_tool ? " tool-run" : ""}`);
  row.dataset.i = block.i;
  const body = el("div", `block${block.tool ? " tool" : ""}${block.tasks ? " tasks" : ""}`);
  for (const part of block.parts) body.append(partNode(part, key));
  if (block.assistant) {
    const text = block.parts.flatMap((part) => (part.spans ? [plain(part.spans)] : [])).filter(Boolean).join("\n\n");
    if (text) body.append(copyButton(text, "copy-all"));
  }
  row.append(body);
  return row;
}

function emptyState() {
  const box = el("div", "empty");
  if (currentThread()?.kind === "obi") {
    box.append(el("p", "dim-text", "Ask Jarvis what's going on, what came in, or what's in your notes."));
    return box;
  }
  const head = el("div");
  head.append(el("span", "mark logo big"), el("h1", "", "OLYMPUS"), el("p", "dim-text", "Every model you're signed into, working as one team."));
  box.append(head);
  const list = el("div", "empty-list");
  for (const [label, ready] of state.meta?.providers ?? []) {
    const line = el("div", "empty-line");
    line.append(el("span", ready ? "glyph-ok" : "glyph-dim", ready ? "✓" : "–"), el("span", ready ? "" : "dim-text", label));
    list.append(line);
  }
  box.append(list);
  const cards = el("div", "card-list");
  for (const [example, what] of EXAMPLES) {
    const card = el("button", "card");
    card.append(el("span", "", example), el("small", "", what));
    card.addEventListener("click", () => { $("input").value = example; autosize(); $("input").focus(); });
    cards.append(card);
  }
  box.append(cards);
  return box;
}

const atBottom = () => {
  const box = $("transcript");
  return box.scrollHeight - box.scrollTop - box.clientHeight < 60;
};

function scrollToEnd() {
  const box = $("transcript");
  box.scrollTop = box.scrollHeight;
}

function applyThread(data, reset) {
  const box = $("transcript");
  const follow = reset || atBottom();
  if (reset) box.replaceChildren();
  box.querySelector(".empty")?.remove();
  if (data.count === 0) {
    box.replaceChildren(emptyState());
  } else {
    for (const row of [...box.querySelectorAll(".row")]) if (Number(row.dataset.i) >= data.from) row.remove();
    box.querySelector(".more")?.remove();
    for (const block of data.blocks) box.append(blockNode(block));
    const first = box.querySelector(".row");
    if (first && Number(first.dataset.i) > 0) box.prepend(moreButton(Number(first.dataset.i)));
  }
  state.view = data;
  state.lastLoadedRev = data.rev;
  $("place").textContent = data.place;
  $("model").textContent = data.model;
  $("account").textContent = `account: ${data.account}`;
  $("profile").textContent = data.full_profile ? "full setup" : "lean setup";
  $("profile").dataset.next = data.full_profile ? "/profile lean" : "/profile full";
  if (follow) scrollToEnd();
  $("latest").hidden = atBottom();
}

function moreButton(before) {
  const button = el("button", "control accent more", "Show earlier");
  button.addEventListener("click", async () => {
    const data = await apiJson(`/api/thread/${state.current}?before=${before}`);
    const box = $("transcript");
    const height = box.scrollHeight;
    button.remove();
    const nodes = data.blocks.map(blockNode);
    box.prepend(...nodes);
    if (data.from > 0) box.prepend(moreButton(data.from));
    box.scrollTop += box.scrollHeight - height;
  });
  return button;
}

$("transcript").addEventListener("scroll", () => { $("latest").hidden = atBottom(); }, { passive: true });
$("latest").addEventListener("click", scrollToEnd);

async function refreshCurrent() {
  if (state.loading) { state.again = true; return; }
  const thread = currentThread();
  if (!thread) return;
  state.loading = true;
  try {
    if (thread.kind === "term") {
      const data = await apiJson(`/api/term/${thread.id}`);
      if (thread.id === state.current && !data.missing) { renderTerm(data); state.lastLoadedRev = thread.rev; }
    } else {
      const reset = state.view?.id !== thread.id;
      const since = reset ? "" : `?since=${state.lastLoadedRev}`;
      const data = await apiJson(`/api/thread/${thread.id}${since}`);
      if (thread.id === state.current && !data.missing) applyThread(data, reset);
    }
  } catch { /* the live connection retries, and the next summary refetches */ }
  state.loading = false;
  if (state.again) { state.again = false; refreshCurrent(); }
}

function selectThread(id) {
  closeDrawer();
  if (state.current === id) return;
  state.current = id;
  state.view = null;
  state.lastLoadedRev = null;
  const thread = currentThread();
  const kind = thread?.kind;
  $("obi").classList.remove("tabs-only");
  $("transcript").hidden = kind === "term" || kind === "obi";
  $("latest").hidden = true;
  $("term").hidden = kind !== "term";
  $("obi").hidden = kind !== "obi";
  $("composer-wrap").hidden = kind === "term";
  $("changes").hidden = kind === "term" || kind === "obi";
  document.body.classList.toggle("in-obi", kind === "obi");
  if (kind === "obi") window.Obi?.open($("obi"));
  if (kind === "term") $("place").textContent = thread.title;
  post({ do: "seen", thread: id }).catch(() => {});
  renderThreads();
  renderComposerState();
  refreshCurrent();
}

/* ---------- composer ---------- */

function autosize() {
  const input = $("input");
  input.style.height = "auto";
  input.style.height = `${Math.min(input.scrollHeight, 168)}px`;
}

function renderComposerState() {
  const thread = currentThread();
  const running = thread?.activity === "running";
  $("composer").classList.toggle("busy", running);
  $("scan").hidden = !running;
  $("stop").hidden = !running;
  $("send").hidden = running && !$("input").value.trim();
  const obi = thread?.kind === "obi";
  $("bypass").textContent = obi ? "read-only" : "bypass on";
  $("bypass").classList.toggle("quiet", obi);
  $("input").placeholder = obi ? "Ask Jarvis what's going on" : running ? "Working. Type to queue the next message." : "Message, /command or @model";
}

function currentHints() {
  const text = $("input").value;
  if (!text.startsWith("/") || /\s/.test(text)) return [];
  return (state.meta?.hints ?? []).filter((hint) => hint.name.startsWith(text));
}

function showHints() {
  const hints = currentHints();
  const box = $("hints");
  box.hidden = hints.length === 0;
  state.hintIndex = Math.min(state.hintIndex, Math.max(0, hints.length - 1));
  box.replaceChildren(...hints.map((hint, index) => {
    const row = el("button", `hint${index === state.hintIndex ? " on" : ""}`);
    row.type = "button";
    row.append(el("b", "", hint.usage), el("span", "", hint.about));
    row.addEventListener("click", () => chooseHint(hint));
    return row;
  }));
}

function chooseHint(hint) {
  $("input").value = hint.args ? `${hint.name} ` : hint.name;
  $("hints").hidden = true;
  autosize();
  $("input").focus();
  if (!hint.args) sendMessage();
}

async function sendMessage() {
  const input = $("input");
  const text = input.value;
  const files = state.attachments.map((file) => file.name);
  if (!text.trim() && files.length === 0) return;
  const thread = state.current;
  if (currentThread()?.kind === "obi") window.Obi.showJarvis();
  input.value = "";
  state.attachments = [];
  renderAttachments();
  autosize();
  $("hints").hidden = true;
  renderComposerState();
  try {
    applyReply(await post({ do: "send", thread, text, files }));
    setTimeout(scrollToEnd, 50);
  } catch {
    input.value = text;
    autosize();
    toast("Unable to reach Olympus. Your message is still in the box.");
  }
}

const runCommand = (line) => post({ do: "send", thread: state.current, text: line, files: [] }).then(applyReply);

$("composer").addEventListener("submit", (event) => { event.preventDefault(); sendMessage(); });
$("input").addEventListener("input", () => { state.hintIndex = 0; autosize(); showHints(); renderComposerState(); });
$("input").addEventListener("keydown", (event) => {
  const hints = currentHints();
  if (hints.length) {
    if (event.key === "ArrowDown") { event.preventDefault(); state.hintIndex = Math.min(state.hintIndex + 1, hints.length - 1); showHints(); return; }
    if (event.key === "ArrowUp") { event.preventDefault(); state.hintIndex = Math.max(state.hintIndex - 1, 0); showHints(); return; }
    if (event.key === "Tab") { event.preventDefault(); $("input").value = `${hints[state.hintIndex].name} `; $("hints").hidden = true; return; }
    if (event.key === "Escape") { $("input").value = ""; $("hints").hidden = true; return; }
    if (event.key === "Enter" && !event.shiftKey && !matchMedia("(pointer: coarse)").matches) { event.preventDefault(); chooseHint(hints[state.hintIndex]); return; }
  }
  // On a touch keyboard Enter is the way to make a new line, and Send is right there.
  if (event.key === "Enter" && !event.shiftKey && !matchMedia("(pointer: coarse)").matches) { event.preventDefault(); sendMessage(); }
  if (event.key === "Escape" && currentThread()?.activity === "running") post({ do: "stop", thread: state.current });
});
$("stop").addEventListener("click", () => post({ do: "stop", thread: state.current }).then(applyReply));
$("model").addEventListener("click", () => runCommand("/model"));
$("account").addEventListener("click", () => runCommand("/account"));
$("profile").addEventListener("click", (event) => runCommand(event.currentTarget.dataset.next));
$("attach").addEventListener("click", () => $("file").click());
$("file").addEventListener("change", async (event) => {
  for (const file of event.target.files) await uploadFile(file);
  event.target.value = "";
});
$("input").addEventListener("paste", async (event) => {
  const files = [...(event.clipboardData?.files ?? [])];
  for (const file of files) await uploadFile(file);
});

async function uploadFile(file) {
  const ext = (file.name.split(".").pop() || file.type.split("/").pop() || "").toLowerCase();
  const response = await api(`/api/upload?ext=${encodeURIComponent(ext)}`, { method: "POST", body: file });
  if (!response.ok) { toast(`${file.name}: ${await response.text() || "Unable to upload that file."}`); return; }
  const { file: name } = await response.json();
  state.attachments.push({ name, shown: file.name });
  renderAttachments();
  renderComposerState();
}

function renderAttachments() {
  const box = $("attachments");
  box.hidden = state.attachments.length === 0;
  box.replaceChildren(...state.attachments.map((file, index) => {
    const chip = el("div", "chip");
    const remove = el("button", "", "×");
    remove.type = "button";
    remove.setAttribute("aria-label", `Remove ${file.shown}`);
    remove.addEventListener("click", () => { state.attachments.splice(index, 1); renderAttachments(); });
    chip.append(el("span", "", file.shown), remove);
    return chip;
  }));
}

/* ---------- sidebar actions ---------- */

$("new-chat").addEventListener("click", () => { closeDrawer(); runCommand("/new"); });
$("palette").addEventListener("click", () => { closeDrawer(); post({ do: "palette", thread: state.current }).then(applyReply); });
document.querySelectorAll("[data-run]").forEach((button) => button.addEventListener("click", () => { closeDrawer(); runCommand(button.dataset.run); }));

/* ---------- picker ---------- */

let pickerKey = "";
function renderPicker() {
  const picker = state.picker;
  $("picker").hidden = !picker;
  if (!picker) { pickerKey = ""; return; }
  const key = JSON.stringify([picker.title, picker.items.length, picker.loading]);
  if (key !== pickerKey) {
    pickerKey = key;
    $("picker-query").value = "";
    if (!matchMedia("(pointer: coarse)").matches) $("picker-query").focus();
  }
  $("picker-title").textContent = picker.title;
  renderPickerRows();
}

function renderPickerRows() {
  const picker = state.picker;
  if (!picker) return;
  const words = $("picker-query").value.toLowerCase().split(/\s+/).filter(Boolean);
  const rows = picker.items.filter((item) => {
    const haystack = `${item.label} ${item.detail}`.toLowerCase();
    return words.every((word) => haystack.includes(word));
  });
  const box = $("picker-rows");
  if (picker.loading) box.replaceChildren(el("div", "picker-message", "Loading…"));
  else if (!rows.length) box.replaceChildren(el("div", "picker-message", picker.items.length ? "Nothing matches that filter." : picker.empty || "Nothing here yet."));
  else box.replaceChildren(...rows.map((item) => {
    const row = el("button", "pick");
    row.append(el("b", "", item.label), el("span", "", item.detail));
    row.addEventListener("click", () => post({ do: "pick", thread: state.current, item: item.i }).then(applyReply));
    return row;
  }));
  $("picker-count").textContent = rows.length ? `${rows.length} of ${picker.items.length}` : "";
}

$("picker-query").addEventListener("input", renderPickerRows);
$("picker-query").addEventListener("keydown", (event) => {
  if (event.key === "Escape") post({ do: "pick_close" });
  if (event.key === "Enter") $("picker-rows").querySelector(".pick")?.click();
});
$("picker-close").addEventListener("click", () => post({ do: "pick_close" }));
$("picker").addEventListener("click", (event) => { if (event.target === $("picker")) post({ do: "pick_close" }); });

/* ---------- terminal ---------- */

function renderTerm(data) {
  const screen = $("term-screen");
  const width = screen.clientWidth - 20;
  // A terminal keeps the PC's columns, so a narrow phone scales it down until reading gets impossible.
  const size = Math.max(7.5, Math.min(13, width / (data.cols * 0.6)));
  const grid = el("div", "term-grid");
  grid.style.fontSize = `${size}px`;
  grid.style.width = `${data.cols}ch`;
  for (const runs of data.lines) {
    const row = el("div", "term-row");
    for (const run of runs) {
      const cell = el("span", "term-run", run.t);
      cell.style.left = `${run.c}ch`;
      cell.style.width = `${run.w}ch`;
      cell.style.color = run.fg;
      if (run.bg) cell.style.background = run.bg;
      if (run.b) cell.style.fontWeight = "700";
      if (run.i) cell.style.fontStyle = "italic";
      if (run.u) cell.style.textDecoration = "underline";
      row.append(cell);
    }
    grid.append(row);
  }
  screen.replaceChildren(grid);
  screen.scrollTop = screen.scrollHeight;
}

$("term-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const input = $("term-input");
  post({ do: "term", thread: state.current, text: input.value }).then(() => post({ do: "key", thread: state.current, key: "enter" }));
  input.value = "";
});
document.querySelectorAll("[data-key]").forEach((button) => button.addEventListener("click", () => post({ do: "key", thread: state.current, key: button.dataset.key })));
document.querySelectorAll("[data-scroll]").forEach((button) => button.addEventListener("click", () => post({ do: "scroll", thread: state.current, lines: Number(button.dataset.scroll) }).then(refreshCurrent)));

/* ---------- changes ---------- */

$("changes").addEventListener("click", async () => {
  $("review").hidden = false;
  const body = $("review-body");
  body.replaceChildren(el("div", "picker-message", "Loading…"));
  $("review-sum").textContent = "";
  const data = await (await api(`/api/changes/${state.current}`)).json();
  if (data.error) { body.replaceChildren(el("div", "picker-message", data.error)); return; }
  if (!data.files.length) { body.replaceChildren(el("div", "picker-message", "No changes here.")); return; }
  const added = data.files.reduce((sum, file) => sum + file.added, 0);
  const removed = data.files.reduce((sum, file) => sum + file.removed, 0);
  $("review-sum").textContent = `${data.files.length} files  +${added} -${removed}`;
  body.replaceChildren(...data.files.flatMap(fileNodes));
});
$("review-close").addEventListener("click", () => { $("review").hidden = true; });

function fileNodes(file) {
  const head = el("button", "diff-file");
  head.append(el("b", "", file.path), el("span", "add-text", `+${file.added}`), el("span", "remove-text", `-${file.removed}`));
  const lines = el("div");
  if (file.binary) lines.append(el("div", "picker-message", "Binary file."));
  for (const line of file.lines) {
    const row = el("div", `diff-line ${line.k}`);
    row.append(el("i", "", line.new ?? line.old ?? ""), el("span", "", line.t));
    lines.append(row);
  }
  head.addEventListener("click", () => { lines.hidden = !lines.hidden; });
  return [head, lines];
}

/* ---------- start ---------- */

async function start() {
  $("connected").textContent = `Connected to ${location.host}`;
  $("pair").hidden = true;
  $("app").hidden = false;
  state.meta = await apiJson("/api/meta");
  listen();
}

async function boot() {
  const code = new URLSearchParams(location.hash.slice(1)).get("code");
  if (code) {
    history.replaceState(null, "", location.pathname);
    if (await pair(code)) { start(); return; }
    if (!state.token) { $("pair").hidden = false; pairFailed(); return; }
  }
  if (state.token) start().catch(() => { $("pair").hidden = !!state.token; });
  else $("pair").hidden = false;
}

window.OlympusShell = { api, apiJson, el, toast, partNode, spansInto, runCommand, copyText };
boot();
