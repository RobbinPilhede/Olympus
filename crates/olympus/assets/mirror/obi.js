"use strict";
window.Obi = (() => {
  let root = null;
  let tab = "today";
  let openRel = null;
  let graph = null;
  let timer = null;
  const shell = () => window.OlympusShell;
  const obi = (body) => shell().apiJson("/api/obi", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  const el = (...args) => shell().el(...args);
  const clock = (stamp) => new Date(stamp * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });

  function tabs() {
    const bar = el("div", "obi-tabs");
    for (const [key, label] of [["today", "Today"], ["notes", "Notes"], ["graph", "Graph"], ["search", "Search"], ["apps", "Apps"], ["jarvis", "Jarvis"]]) {
      const button = el("button", `control${tab === key ? " accent on" : ""}`, label);
      button.addEventListener("click", () => show(key));
      bar.append(button);
    }
    return bar;
  }

  function page(content) {
    stopGraph();
    root.classList.remove("tabs-only");
    document.getElementById("transcript").hidden = true;
    document.getElementById("latest").hidden = true;
    root.replaceChildren(tabs(), content);
  }

  function showJarvis() {
    stopGraph();
    tab = "jarvis";
    root.classList.add("tabs-only");
    root.replaceChildren(tabs());
    const transcript = document.getElementById("transcript");
    transcript.hidden = false;
    transcript.scrollTop = transcript.scrollHeight;
  }

  function show(next) {
    tab = next;
    if (next === "today") return showToday();
    if (next === "notes") return openRel ? showNote(openRel) : showTree();
    if (next === "graph") return showGraph();
    if (next === "jarvis") return showJarvis();
    if (next === "apps") return showApps();
    return showSearch();
  }

  const MODES = [["read", "Read"], ["time_only", "Time only"], ["off", "Off"]];

  async function appIcon(app) {
    const box = el("span", "obi-app-icon", app.icon ? "" : (app.name[0] || "?").toUpperCase());
    if (!app.icon) return box;
    const response = await shell().api(`/api/image?path=${encodeURIComponent(app.icon)}`);
    if (response.ok) {
      const image = document.createElement("img");
      image.alt = "";
      image.src = URL.createObjectURL(await response.blob());
      box.append(image);
    }
    return box;
  }

  function appRow(app) {
    const row = el("div", "obi-app");
    const icon = el("span", "obi-app-icon", app.icon ? "" : (app.name[0] || "?").toUpperCase());
    row.append(icon, el("span", "obi-app-name", app.name));
    appIcon(app).then((loaded) => icon.replaceWith(loaded));
    if (app.locked) {
      row.append(el("span", "dim-text", "Never read"));
      return row;
    }
    const modes = el("div", "obi-modes");
    for (const [mode, label] of MODES) {
      const button = el("button", `control${app.mode === mode ? " accent on" : ""}`, label);
      button.addEventListener("click", async () => { await obi({ do: "set_app", exe: app.exe, mode }); showApps(); });
      modes.append(button);
    }
    row.append(modes);
    return row;
  }

  async function showApps() {
    const data = await obi({ do: "apps" });
    if (tab !== "apps") return;
    const box = el("div", "obi-page");
    box.append(el("h2", "", "What Obiview reads"));
    box.append(el("p", "dim-text", "Every app starts off. Read keeps what's on screen. Time only keeps how long you used an app and its window titles. Off keeps nothing. Changes apply to new reading: what it already read stays for up to 14 days, and Jarvis can still see it."));
    box.append(el("p", "dim-text", "Always skipped: password fields, password managers, private browsing and banks."));
    box.append(el("h3", "", "What to read"));
    for (const [key, label, hint] of [["text", "Text in apps", "What's shown on screen, so you can search it and summarize it."], ["behind", "Windows behind the one you're using", "Checked once a minute, so mail and chats that come in get noticed."], ["files", "Files you save", "File names only, in your home folder, plus that day's Git commits."], ["processes", "Programs starting and stopping", "Just which ones and when."]]) {
      const row = el("div", "obi-read");
      const on = data.reads[key];
      const words = el("div", "obi-read-words");
      words.append(el("span", "", label), el("span", "dim-text", hint));
      const button = el("button", `obi-toggle${on ? " on" : ""}`);
      button.setAttribute("role", "switch");
      button.setAttribute("aria-checked", String(on));
      button.setAttribute("aria-label", label);
      button.addEventListener("click", async () => { await obi({ do: "set_read", key, on: !on }); showApps(); });
      row.append(words, button);
      box.append(row);
    }
    box.append(el("h3", "", "Apps"));
    const scope = el("div", "obi-modes");
    for (const [only, label] of [[false, "Every app except those I turn off"], [true, "Only the apps I pick"]]) {
      const button = el("button", `control${data.only === only ? " accent on" : ""}`, label);
      button.addEventListener("click", async () => { await obi({ do: "set_only", only }); showApps(); });
      scope.append(button);
    }
    box.append(scope);
    if (data.only && !data.apps.some((app) => app.mode !== "off")) box.append(el("p", "warn-text", "Nothing is read until you choose Read or Time only on an app."));
    if (data.loading && !data.apps.length) {
      box.append(el("p", "dim-text", "Looking for your apps..."));
      setTimeout(() => { if (tab === "apps") showApps(); }, 1500);
    }
    const open = data.apps.filter((app) => app.open);
    const others = data.apps.filter((app) => !app.open);
    if (open.length) box.append(el("p", "dim-text", "Open now"), ...open.map(appRow));
    if (others.length) box.append(el("p", "dim-text", "Installed"), ...others.map(appRow));
    box.append(el("h3", "", "Inside your browser"));
    box.append(el("p", "dim-text", "Sites appear here once you've used them."));
    for (const site of data.sites) {
      const row = el("div", "obi-app");
      row.append(el("span", "obi-app-name", site.name));
      const button = el("button", "control", site.read ? "Stop reading" : "Read again");
      button.addEventListener("click", async () => { await obi({ do: "set_site", name: site.name, read: !site.read }); showApps(); });
      row.append(button);
      box.append(row);
    }
    const scroll = root.querySelector(".obi-page")?.scrollTop ?? 0;
    page(box);
    box.scrollTop = scroll;
  }

  async function showToday() {
    const data = await obi({ do: "home" });
    if (tab !== "today") return;
    const box = el("div", "obi-page");
    box.append(el("h2", "", "Today"));
    box.append(el("p", "dim-text", data.summaries ? `${data.summaries} tidies your notes from what Obiview reads, on your subscription.${data.summaryMessages ? " That includes mail and chats." : " Mail, chats and meetings stay out."}` : "Nothing is tidied in the background until you choose an app."));
    if (data.stored != null) box.append(el("p", "dim-text", data.stored === 1 ? "1 thing read since it started." : `${data.stored} things read since it started.`));
    if (data.problem) box.append(el("p", "error-text", data.problem));
    const line = el("div", "obi-switch-row");
    line.append(el("span", "", data.line));
    const toggle = el("button", "primary", data.on ? "Stop reading" : "Start reading");
    toggle.addEventListener("click", async () => { await obi({ do: "toggle" }); showToday(); });
    line.append(toggle);
    box.append(line);
    if (!data.on) box.append(el("p", "dim-text", "Every app starts off. Turn this on, then pick apps on the Apps tab, like Teams, Outlook or your browser. What it reads stays on your PC. It skips password fields, password managers, private browsing and banks. Your notes work either way."));
    if (data.sources.length) {
      box.append(el("h3", "", "What it's reading"));
      for (const source of data.sources) {
        const row = el("div", "obi-row");
        row.append(el("span", "", source.source), el("span", "dim-text", `${source.captures} read, last ${clock(source.last_seen)}`));
        box.append(row);
      }
    }
    box.append(el("h3", "", "Your day"));
    if (!data.timeline.length) box.append(el("p", "dim-text", "Nothing yet today."));
    for (const row of data.timeline) {
      const line2 = el("div", "obi-row");
      line2.append(el("span", "dim-text", clock(row.start)), el("span", "t-bright", row.source ?? row.app), el("span", "obi-grow", row.title));
      box.append(line2);
    }
    page(box);
  }

  async function showTree() {
    const data = await obi({ do: "tree" });
    if (tab !== "notes") return;
    const box = el("div", "obi-page");
    const head = el("div", "obi-switch-row");
    const today = el("button", "primary", "Today's note");
    today.addEventListener("click", async () => { const note = await obi({ do: "daily" }); showNote(note.rel); });
    const create = el("button", "control dim", "New note");
    create.addEventListener("click", () => {
      const name = prompt("Name the note");
      if (name) obi({ do: "create", name }).then((note) => (note.error ? shell().toast(note.error) : showNote(note.rel)));
    });
    head.append(today, create);
    box.append(head);
    if (!data.notes.length) box.append(el("p", "dim-text", "No notes yet."));
    const sorted = [...data.notes].sort((a, b) => a.rel.localeCompare(b.rel));
    for (const note of sorted) {
      const row = el("button", "obi-note");
      row.append(el("span", "", note.title));
      const folder = note.rel.includes("/") ? note.rel.slice(0, note.rel.lastIndexOf("/")) : "";
      if (folder) row.append(el("small", "dim-text", folder));
      row.addEventListener("click", () => showNote(note.rel));
      box.append(row);
    }
    if (data.tags.length) {
      box.append(el("h3", "", "Tags"));
      for (const { tag, count } of data.tags) {
        const row = el("button", "obi-note obi-tag");
        row.append(el("span", "", `#${tag}`), el("small", "dim-text", String(count)));
        row.addEventListener("click", () => { query = `tag:${tag}`; show("search"); });
        box.append(row);
      }
    }
    page(box);
  }

  function linkList(box, title, links) {
    if (!links.length) return;
    box.append(el("h3", "", title));
    for (const link of links) {
      const row = el("button", "obi-note");
      row.append(el("span", "t-bright", link.from), el("small", "dim-text", link.context));
      row.addEventListener("click", () => showNote(link.from));
      box.append(row);
    }
  }

  async function showNote(rel) {
    openRel = rel;
    tab = "notes";
    const data = await obi({ do: "note", rel });
    if (data.missing) { await obi({ do: "save", rel, text: "" }); return showNote(rel); }
    if (data.error) { shell().toast(data.error); return showTree(); }
    openRel = data.rel;
    const box = el("div", "obi-page");
    const head = el("div", "obi-switch-row obi-note-head");
    const back = el("button", "control dim", "All notes");
    back.addEventListener("click", () => { openRel = null; showTree(); });
    const edit = el("button", "control accent", "Edit");
    edit.addEventListener("click", () => editNote(data));
    const local = el("button", "control dim", "Local graph");
    local.addEventListener("click", () => showGraph(data.rel));
    head.append(back, el("span", "grow"), local, edit);
    box.append(el("div", "obi-path dim-text", data.rel), head);
    const body = el("div", "block note-body");
    for (const part of data.parts) body.append(shell().partNode(part, `note:${data.rel}`));
    box.append(body);
    linkList(box, `Linked here (${data.backlinks.length})`, data.backlinks);
    linkList(box, `Mentioned without a link (${data.unlinked.length})`, data.unlinked);
    page(box);
  }

  function editNote(data) {
    const box = el("div", "obi-page obi-edit");
    const area = el("textarea", "obi-area");
    area.value = data.text;
    area.setAttribute("aria-label", "Note text");
    const head = el("div", "obi-switch-row");
    const cancel = el("button", "control dim", "Cancel");
    cancel.addEventListener("click", () => showNote(data.rel));
    const save = el("button", "primary", "Save");
    save.addEventListener("click", async () => {
      const reply = await obi({ do: "save", rel: data.rel, text: area.value });
      if (reply.error) shell().toast(reply.error); else showNote(data.rel);
    });
    head.append(el("span", "obi-grow dim-text", data.rel), cancel, save);
    box.append(head, area);
    page(box);
  }

  let query = "";
  function showSearch() {
    const box = el("div", "obi-page");
    const input = el("input", "obi-input");
    input.placeholder = "Search. tag:, file:, path:, [property], -not, \"phrases\"";
    input.setAttribute("aria-label", "Search your notes and what Obiview has read");
    input.value = query;
    const results = el("div", "obi-results");
    let pending = 0;
    const run = async () => {
      query = input.value;
      const ticket = ++pending;
      if (!query.trim()) { results.replaceChildren(); return; }
      const data = await obi({ do: "search", q: query });
      if (ticket !== pending) return;
      results.replaceChildren(el("h3", "", `${data.hits.length} in your notes`));
      for (const hit of data.hits) {
        const row = el("button", "obi-note");
        row.append(el("span", "t-bright", hit.rel), el("small", "dim-text", hit.snippet));
        row.addEventListener("click", () => showNote(hit.rel));
        results.append(row);
      }
      if (data.read?.length) {
        results.append(el("h3", "", "In what Obiview has read"));
        for (const hit of data.read) {
          const row = el("div", "obi-note");
          row.append(el("span", "t-bright", `${clock(hit.ts)} ${hit.source ?? hit.exe}`), el("small", "dim-text", hit.snippet));
          results.append(row);
        }
      }
    };
    input.addEventListener("input", run);
    box.append(input, results);
    page(box);
    run();
  }

  function stopGraph() {
    clearTimeout(timer);
    timer = null;
    graph = null;
  }

  async function showGraph(local) {
    tab = "graph";
    const data = await obi({ do: "graph", local: local ?? null });
    const wrap = el("div", "obi-graph");
    const canvas = el("canvas");
    canvas.setAttribute("aria-label", "Map of your notes");
    wrap.append(canvas);
    if (!data.nodes.length) wrap.append(el("p", "dim-text obi-empty", "Nothing to map yet. Link notes with [[double brackets]]."));
    page(wrap);
    graph = { data, canvas, positions: [], zoom: 0.5, pan: [0, 0], hover: null, neighbours: new Map() };
    for (const [a, b] of data.edges) {
      if (!graph.neighbours.has(a)) graph.neighbours.set(a, new Set());
      if (!graph.neighbours.has(b)) graph.neighbours.set(b, new Set());
      graph.neighbours.get(a).add(b);
      graph.neighbours.get(b).add(a);
    }
    attachGestures(graph);
    pollPositions(graph);
  }

  async function pollPositions(current) {
    if (graph !== current) return;
    const data = await obi({ do: "positions" });
    if (graph !== current) return;
    current.positions = data.positions;
    fit(current);
    draw(current);
    timer = setTimeout(() => pollPositions(current), data.settled ? 800 : 70);
  }

  const radius = (degree) => 4 + Math.sqrt(degree);
  const labelAlpha = (zoom) => Math.max(0, Math.min(1, (Math.log2(zoom) + 2) / 0.5));

  function fit(current) {
    const { positions, canvas } = current;
    if (current.held || !positions.length) return;
    const xs = positions.map((p) => p[0]);
    const ys = positions.map((p) => p[1]);
    const [left, right, top, bottom] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
    const room = (size) => Math.max(60, size - 170);
    current.zoom = Math.max(0.05, Math.min(1.2, Math.min(room(canvas.clientWidth) / Math.max(1, right - left), room(canvas.clientHeight) / Math.max(1, bottom - top))));
    current.pan = [-(left + right) / 2, -(top + bottom) / 2];
  }

  function screen(current, position, width, height) {
    return [width / 2 + (position[0] + current.pan[0]) * current.zoom, height / 2 + (position[1] + current.pan[1]) * current.zoom];
  }

  function draw(current) {
    const { canvas, data, positions, hover } = current;
    const scale = devicePixelRatio || 1;
    const width = canvas.clientWidth;
    const height = canvas.clientHeight;
    if (canvas.width !== width * scale) { canvas.width = width * scale; canvas.height = height * scale; }
    const ctx = canvas.getContext("2d");
    ctx.setTransform(scale, 0, 0, scale, 0, 0);
    ctx.clearRect(0, 0, width, height);
    const styles = getComputedStyle(document.documentElement);
    const color = (name) => styles.getPropertyValue(`--${name}`);
    const lit = (i) => hover == null || hover === i || current.neighbours.get(hover)?.has(i);
    ctx.lineWidth = 1;
    for (const [a, b] of data.edges) {
      if (!positions[a] || !positions[b]) continue;
      const [x1, y1] = screen(current, positions[a], width, height);
      const [x2, y2] = screen(current, positions[b], width, height);
      ctx.globalAlpha = hover == null || hover === a || hover === b ? 1 : 0.25;
      ctx.strokeStyle = color("line");
      ctx.beginPath();
      ctx.moveTo(x1, y1);
      ctx.lineTo(x2, y2);
      ctx.stroke();
    }
    ctx.font = "12px 'JetBrains Mono', monospace";
    ctx.textAlign = "center";
    ctx.textBaseline = "top";
    data.nodes.forEach((node, i) => {
      if (!positions[i]) return;
      const [x, y] = screen(current, positions[i], width, height);
      const r = radius(node.degree) * Math.sqrt(current.zoom);
      ctx.globalAlpha = lit(i) ? 1 : 0.22;
      ctx.fillStyle = hover === i ? color("accent") : { Tag: color("dim"), Unresolved: color("line"), Attachment: color("dim") }[node.kind] ?? color("text");
      ctx.beginPath();
      ctx.arc(x, y, r, 0, Math.PI * 2);
      ctx.fill();
      const alpha = hover === i ? 1 : labelAlpha(current.zoom) * (lit(i) ? 1 : 0.2);
      if (alpha > 0.02) {
        ctx.globalAlpha = alpha;
        ctx.fillStyle = color("text");
        ctx.fillText(node.label, x, y + r + 2);
      }
    });
    ctx.globalAlpha = 1;
  }

  function nodeAt(current, x, y) {
    const { canvas, positions, data } = current;
    let best = null;
    let closest = Infinity;
    positions.forEach((position, i) => {
      const [nx, ny] = screen(current, position, canvas.clientWidth, canvas.clientHeight);
      const distance = Math.hypot(nx - x, ny - y);
      // A fingertip needs more room than a dot is wide.
      const reach = Math.max(18, radius(data.nodes[i]?.degree ?? 0) * Math.sqrt(current.zoom));
      if (distance <= reach && distance < closest) { closest = distance; best = i; }
    });
    return best;
  }

  function attachGestures(current) {
    const { canvas } = current;
    const pointers = new Map();
    let dragged = null;
    let moved = 0;
    let pinch = null;
    const local = (event) => { const box = canvas.getBoundingClientRect(); return [event.clientX - box.left, event.clientY - box.top]; };
    canvas.addEventListener("pointerdown", (event) => {
      canvas.setPointerCapture(event.pointerId);
      pointers.set(event.pointerId, local(event));
      moved = 0;
      if (pointers.size === 1) { dragged = nodeAt(current, ...local(event)); current.hover = dragged; draw(current); }
      else { dragged = null; pinch = null; }
    });
    canvas.addEventListener("pointermove", (event) => {
      const previous = pointers.get(event.pointerId);
      const at = local(event);
      if (!previous) {
        const hover = nodeAt(current, ...at);
        if (hover !== current.hover) { current.hover = hover; draw(current); }
        return;
      }
      pointers.set(event.pointerId, at);
      current.held = true;
      moved += Math.hypot(at[0] - previous[0], at[1] - previous[1]);
      if (pointers.size === 2) {
        const [a, b] = [...pointers.values()];
        const distance = Math.hypot(a[0] - b[0], a[1] - b[1]);
        if (pinch) current.zoom = Math.max(0.05, Math.min(6, current.zoom * (distance / pinch)));
        pinch = distance;
      } else if (dragged != null) {
        const x = (at[0] - canvas.clientWidth / 2) / current.zoom - current.pan[0];
        const y = (at[1] - canvas.clientHeight / 2) / current.zoom - current.pan[1];
        obi({ do: "pin", index: dragged, x, y });
      } else {
        current.pan[0] += (at[0] - previous[0]) / current.zoom;
        current.pan[1] += (at[1] - previous[1]) / current.zoom;
      }
      draw(current);
    });
    const release = (event) => {
      pointers.delete(event.pointerId);
      pinch = null;
      if (dragged != null) {
        obi({ do: "pin", index: dragged, x: null, y: null });
        const node = current.data.nodes[dragged];
        if (moved < 8 && node && (node.kind === "Note" || node.kind === "Unresolved")) showNote(node.id);
        dragged = null;
      }
    };
    canvas.addEventListener("pointerup", release);
    canvas.addEventListener("pointercancel", release);
    canvas.addEventListener("wheel", (event) => {
      event.preventDefault();
      current.held = true;
      current.zoom = Math.max(0.05, Math.min(6, current.zoom * Math.exp(-event.deltaY * 0.0015)));
      draw(current);
    }, { passive: false });
  }

  return {
    open(container) { root = container; show(tab); },
    showJarvis,
    onSummary() {},
    isNoteLink: (href) => href.startsWith("obi:") || href.startsWith("obi-tag:"),
    openLink(href) {
      if (href.startsWith("obi-tag:")) { query = `tag:${decodeURIComponent(href.slice(8))}`; tab = "search"; showSearch(); return; }
      showNote(decodeURIComponent(href.slice(4)));
    },
  };
})();
