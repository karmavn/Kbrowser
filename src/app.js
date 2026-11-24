const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (s) => document.querySelector(s);
const parseSvg = (markup) => {
  const d = document.createElement("div");
  d.innerHTML = markup;
  const svg = d.firstElementChild;
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  return svg;
};

const CLOSE_ICON_NODE = parseSvg('<svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6L6 18"/></svg>');
const LOADING_ICON_NODE = parseSvg('<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="6" stroke-dasharray="28" stroke-dashoffset="10"/></svg>');
const SEARCH_ICON_NODE = parseSvg('<svg viewBox="0 0 24 24"><circle cx="10.5" cy="10.5" r="6.5"/><path d="M15.5 15.5 21 21"/></svg>');
const PAGE_ICON_NODE = parseSvg('<svg viewBox="0 0 24 24"><path d="M6 3h8l4 4v14H6z"/><path d="M14 3v4h4"/></svg>');
const MAX_ICON_NODE = parseSvg('<svg viewBox="0 0 10 10"><path d="M.5.5h9v9h-9z"/></svg>');
const RESTORE_ICON_NODE = parseSvg('<svg viewBox="0 0 10 10"><path d="M2.5 2.5v-2h7v7h-2M.5 2.5h7v7h-7z"/></svg>');

const $tabs = $("#tabs");
const $progress = $("#progress");
const $addr = $("#addr");
const $add = $("#add");
const $suggest = $("#suggest");
const $winMax = $("#win-max");

const INTERNAL = new Set(["about:extensions", "about:settings", "about:new"]);

// Mirrors the Rust `Settings`; replaced by the saved values at startup.
let settings = { theme: "system", search: "duckduckgo", home: "about:new", save_history: true, show_suggestions: true, save_session: false };

function applyTheme() {
  const t = settings.theme;
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
}

// Web Store detail pages look like /detail/<slug>/<32-letter id>
const storeId = (url) =>
  /^https:\/\/chromewebstore\.google\.com\/detail\/[^/]+\/([a-p]{32})/.exec(url ?? "")?.[1] ?? null;
// Our own pages are served from tauri.localhost; show a friendly address.
const shown = (url) => {
  const m = /^https?:\/\/tauri\.localhost\/(extensions|settings|new)\.html/.exec(url ?? "");
  return m ? `about:${m[1]}` : url ?? "";
};

const tabs = new Map(); // id -> { title, url, loading }
let active = null;
let seq = 0;
const closingTabs = new Set();
let shuttingDown = false;
let restoring = true; // no saving while the old session is being reopened
let lastSaved = "";

async function openTab(url = settings.home) {
  if (shuttingDown) return;

  const id = "t" + ++seq;
  const isExtensionPage = /^chrome-extension:\/\//i.test(url);

  tabs.set(id, { title: "New tab", url, loading: true });
  active = id;
  render();

  try {
    // Let the native webview initialize and load its extensions first.
    await invoke("new_tab", {
      id,
      url: isExtensionPage ? "about:blank" : url,
    });

    // Navigate to the extension page after creation is complete.
    if (isExtensionPage) {
      await invoke("navigate", { id, input: url });
    }
  } catch (e) {
    const tab = tabs.get(id);
    if (tab) {
      tab.title = "Failed to open";
      tab.loading = false;
      render();
    }
    console.error("Failed to open tab:", e);
  }
}

// Internal pages (settings, extensions) reuse an already-open tab.
function openInternal(url) {
  const found = [...tabs].find(([, t]) => t.url === url || shown(t.url) === url);
  found ? activate(found[0]) : openTab(url);
}

function activate(id) {
  active = id;
  render();
  invoke("activate_tab", { id });
}

function cycleTab(dir) {
  const ids = [...tabs.keys()];
  if (ids.length < 2) return;
  const idx = ids.indexOf(active);
  const next = ids[(idx + dir + ids.length) % ids.length];
  if (next) activate(next);
}

function handleTabKey(e, id) {
  if (e.target !== e.currentTarget) return;
  if (e.key === "Enter" || e.key === " ") {
    e.preventDefault();
    activate(id);
  } else if (e.key === "Delete") {
    e.preventDefault();
    closeTab(id, { keepFocus: true });
  } else if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
    e.preventDefault();
    const ids = [...tabs.keys()];
    const idx = ids.indexOf(id);
    if (idx === -1) return;
    const nextId = ids[(idx + (e.key === "ArrowRight" ? 1 : -1) + ids.length) % ids.length];
    if (nextId && nextId !== id) {
      e.currentTarget.setAttribute("tabindex", "-1");
      const nextEl = $tabs.querySelector(`.tab[data-id="${nextId}"]`);
      if (nextEl) {
        nextEl.setAttribute("tabindex", "0");
        nextEl.focus();
      }
    }
  }
}

async function closeTab(id, { keepFocus = false } = {}) {
  if (shuttingDown || closingTabs.has(id) || !tabs.has(id)) {
    return;
  }

  const ids = [...tabs.keys()];
  const idx = ids.indexOf(id);
  if (idx === -1) return;

  closingTabs.add(id);
  const wasActive = active === id;
  tabs.delete(id);
  const remaining = [...tabs.keys()];
  const nextFocusId = remaining[Math.min(idx, remaining.length - 1)] ?? null;

  if (wasActive) {
    active = nextFocusId;
  }

  // Optimistic UI: tab closes immediately with zero click delay
  render();
  if (keepFocus && nextFocusId) {
    const nextEl = $tabs.querySelector(`.tab[data-id="${nextFocusId}"]`);
    if (nextEl) {
      nextEl.setAttribute("tabindex", "0");
      nextEl.focus();
    }
  }

  if (tabs.size === 0) {
    shuttingDown = true;
    try {
      await invoke("save_tabs", { tabs: [], active: 0 });
      await invoke("exit_app");
    } catch (e) {
      shuttingDown = false;
      console.error("exit_app failed:", e);
    }
    return;
  }

  if (wasActive && active) {
    invoke("activate_tab", { id: active, focus: !keepFocus }).catch(() => {});
  }

  // Destroy the native webview asynchronously in the background
  try {
    await invoke("close_tab", { id });
  } catch (e) {
    console.error("close_tab failed:", e);
  } finally {
    closingTabs.delete(id);
  }
}

function render() {
  const currentEls = $tabs.children;
  const targetIds = Array.from(tabs.keys());

  let sameIds = currentEls.length === targetIds.length;
  if (sameIds) {
    for (let i = 0; i < targetIds.length; i++) {
      if (currentEls[i].dataset.id !== targetIds[i]) {
        sameIds = false;
        break;
      }
    }
  }

  if (sameIds) {
    // Fast path: update attributes and text in place without destroying DOM elements
    for (let i = 0; i < currentEls.length; i++) {
      const el = currentEls[i];
      const id = targetIds[i];
      const t = tabs.get(id);
      const isActive = id === active;
      const expectedClass = "tab" + (isActive ? " active" : "") + (t.loading ? " loading" : "");
      if (el.className !== expectedClass) el.className = expectedClass;
      const selected = isActive ? "true" : "false";
      if (el.getAttribute("aria-selected") !== selected) el.setAttribute("aria-selected", selected);
      const ti = isActive ? "0" : "-1";
      if (el.getAttribute("tabindex") !== ti) el.setAttribute("tabindex", ti);
      if (el.title !== t.title) {
        el.title = t.title;
        const titleSpan = el.children[1];
        if (titleSpan && titleSpan.textContent !== t.title) titleSpan.textContent = t.title;
      }
    }
  } else {
    // Structural change: build with DocumentFragment for single reflow
    const frag = document.createDocumentFragment();
    for (const [id, t] of tabs) {
      const isActive = id === active;
      const el = document.createElement("div");
      el.dataset.id = id;
      el.className = "tab" + (isActive ? " active" : "") + (t.loading ? " loading" : "");
      el.setAttribute("role", "tab");
      el.setAttribute("aria-selected", isActive ? "true" : "false");
      el.setAttribute("tabindex", isActive ? "0" : "-1");
      el.title = t.title;

      const spin = document.createElement("span");
      spin.className = "tab-spin";
      spin.setAttribute("aria-hidden", "true");
      spin.appendChild(LOADING_ICON_NODE.cloneNode(true));

      const title = document.createElement("span");
      title.className = "title";
      title.textContent = t.title;

      const x = document.createElement("button");
      x.className = "close icon";
      x.setAttribute("aria-label", "Close tab");
      x.appendChild(CLOSE_ICON_NODE.cloneNode(true));
      x.onclick = (e) => { e.stopPropagation(); closeTab(id); };

      el.append(spin, title, x);
      el.onclick = () => activate(id);
      el.onauxclick = (e) => { if (e.button === 1) closeTab(id); };
      el.onkeydown = (e) => handleTabKey(e, id);
      frag.append(el);
    }
    $tabs.replaceChildren(frag);
  }

  const t = tabs.get(active);
  $progress.classList.toggle("on", !!t?.loading);
  if (document.activeElement !== $addr) $addr.value = shown(t?.url);
  $add.hidden = !storeId(t?.url);
  saveSession();
}

//saves current tabs and window to json
function saveSession() {
  if (restoring || shuttingDown || !settings.save_session) return;
  const list = [...tabs]
    .map(([id, t]) => ({ id, url: shown(t.url) }))
    .filter((e) => e.url && !e.url.startsWith("chrome-search://"));
  const urls = list.map((e) => e.url);
  const idx = Math.max(0, list.findIndex((e) => e.id === active));
  const key = JSON.stringify([urls, idx]);
  if (key === lastSaved) return; // titles/loading state change often, the list rarely
  lastSaved = key;
  invoke("save_tabs", { tabs: urls, active: idx }).catch(() => {});
}

// Adds a finished page load to the history (the backend ignores internal pages and dedupes).
function record(t) {
  if (!settings.save_history || t.loading) return;
  let u;
  try { u = new URL(t.url); } catch { return; }
  if (!/^https?:$/.test(u.protocol) || u.hostname === "tauri.localhost") return;
  invoke("history_add", { url: t.url, title: t.title === "New tab" ? "" : t.title });
}

listen("tab-updated", ({ payload: p }) => {
  const t = tabs.get(p.id);
  if (!t) return;
  // Safety net: if a bounce to Chrome's new tab page slips past the backend, step back once.
  if (p.url?.startsWith("chrome-search://")) {
    if (!t.bounced) { t.bounced = true; invoke("nav", { id: p.id, action: "back" }); }
    return;
  }
  let changed = false;
  if (p.url != null && p.url !== t.url) {
    t.url = p.url;
    changed = true;
    if (t.title === "New tab") {
      try { t.title = new URL(p.url).hostname; } catch {}
    }
  }
  if (p.title != null && p.title !== t.title) {
    t.title = p.title;
    changed = true;
  }
  if (p.loading != null && p.loading !== t.loading) {
    t.loading = p.loading;
    changed = true;
  }
  if (changed) {
    render();
    // Record when a load finishes, and again if the title arrives afterwards.
    if (p.loading === false || (p.title && !t.loading)) record(t);
  }
});

listen("open-tab", ({ payload: url }) => (INTERNAL.has(url) ? openInternal(url) : openTab(url)));
listen("settings-changed", ({ payload }) => { settings = payload; applyTheme(); });
listen("ui-action", ({ payload }) => {
  if (payload === "new-tab") openTab();
  else if (payload === "focus-addr") $addr.focus();
  else if (payload === "open-settings") openInternal("about:settings");
});

const nav = (action) => active && invoke("nav", { id: active, action });
$("#back").onclick = () => nav("back");
$("#fwd").onclick = () => nav("forward");
$("#reload").onclick = () => nav("reload");
$("#new").onclick = () => openTab();
$("#ext").onclick = () => openInternal("about:extensions");
$("#settings").onclick = () => openInternal("about:settings");

$add.onclick = async () => {
  const id = storeId(tabs.get(active)?.url);
  if (!id) return;
  $add.disabled = true;
  $add.textContent = "Adding…";
  try {
    await invoke("install_extension", { input: id });
    openTab("about:extensions"); // shows the restart prompt
  } catch (e) {
    $add.textContent = "Couldn’t add";
    $add.title = String(e);
    await new Promise((r) => setTimeout(r, 2500));
  }
  $add.disabled = false;
  $add.textContent = "Add to browser";
  $add.title = "";
};

// ---------- Address-bar suggestions (from the history) ----------
// The UI webview is only 84px tall, so while the list is open it grows by the list's height
// and the page webviews slide down by the same amount (see `set_suggest_height` in main.rs).
const ROW_H = 40, PAD = 8;
let rows = [];
let sel = -1;
let suggestTimer = 0;
let suggestSeq = 0;
let currentSuggestH = 0;

function closeSuggest() {
  clearTimeout(suggestTimer);
  suggestSeq++; // drop any answer still on its way
  $addr.setAttribute("aria-expanded", "false");
  $addr.removeAttribute("aria-activedescendant");
  if (!rows.length && currentSuggestH === 0) return;
  rows = [];
  sel = -1;
  $suggest.hidden = true;
  if (currentSuggestH !== 0) {
    currentSuggestH = 0;
    invoke("set_suggest_height", { height: 0 });
  }
}

function updateSuggestSelection() {
  const children = $suggest.children;
  for (let i = 0; i < children.length; i++) {
    const selected = i === sel;
    children[i].classList.toggle("sel", selected);
    children[i].setAttribute("aria-selected", selected ? "true" : "false");
  }
  if (sel >= 0 && children[sel]) {
    $addr.setAttribute("aria-activedescendant", children[sel].id);
  } else {
    $addr.removeAttribute("aria-activedescendant");
  }
}

function paintSuggest() {
  const frag = document.createDocumentFragment();
  rows.forEach((r, i) => {
    const isSel = i === sel;
    const el = document.createElement("div");
    el.id = "sopt-" + i;
    el.className = "srow" + (isSel ? " sel" : "");
    el.setAttribute("role", "option");
    el.setAttribute("aria-selected", isSel ? "true" : "false");

    const ico = document.createElement("span");
    ico.className = "sico";
    ico.setAttribute("aria-hidden", "true");
    ico.appendChild((r.query != null ? SEARCH_ICON_NODE : PAGE_ICON_NODE).cloneNode(true));

    const main = document.createElement("span");
    main.className = "smain";
    if (r.query != null) {
      main.textContent = r.query;
    } else {
      let host = r.url;
      try { host = new URL(r.url).hostname.replace(/^www\./, ""); } catch {}
      const t = document.createElement("span");
      t.className = "stitle";
      t.textContent = r.title || host;
      const u = document.createElement("span");
      u.className = "surl";
      u.textContent = host;
      main.append(t, u);
    }
    el.append(ico, main);
    // mousedown (not click) so the address box keeps focus and its blur does not close the list first
    el.onmousedown = (e) => { e.preventDefault(); pick(r); };
    frag.append(el);
  });
  $suggest.replaceChildren(frag);
}

function pick(r) {
  $addr.value = r.url;
  closeSuggest();
  if (active) invoke("navigate", { id: active, input: r.url });
  $addr.blur();
}

//updates the suggested address bar inputs based on input and search history (if enabled)
async function updateSuggest() {
  if (!settings.show_suggestions)
    return;
  const q = $addr.value.trim();
  const seq = ++suggestSeq;
  if (!q || INTERNAL.has(q)) return closeSuggest();
  let list = [];
  try { list = await invoke("history_suggest", { query: q }); } catch {}
  if (seq !== suggestSeq || document.activeElement !== $addr) return; // stale answer
  if (!list.length) { suggestSeq++; return closeSuggest(); }
  rows = list;
  sel = -1;
  paintSuggest();
  $suggest.hidden = false;
  $addr.setAttribute("aria-expanded", "true");
  $addr.removeAttribute("aria-activedescendant");
  const targetH = rows.length * ROW_H + PAD;
  if (currentSuggestH !== targetH) {
    currentSuggestH = targetH;
    invoke("set_suggest_height", { height: targetH });
  }
}

//address bar
$addr.onfocus = () => $addr.select();
$addr.oninput = () => { clearTimeout(suggestTimer); suggestTimer = setTimeout(updateSuggest, 150); };
$addr.onblur = () => closeSuggest();
$addr.onkeydown = (e) => {
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    if (!rows.length) return;
    e.preventDefault();
    const n = rows.length;
    sel = e.key === "ArrowDown" ? (sel + 1) % n : (sel - 1 + n) % n;
    updateSuggestSelection();
  } else if (e.key === "Enter" && active && $addr.value.trim()) {
    if (sel >= 0 && rows[sel]) return pick(rows[sel]);
    const v = $addr.value.trim();
    closeSuggest();
    if (INTERNAL.has(v)) openInternal(v);
    else invoke("navigate", { id: active, input: $addr.value });
    $addr.blur();
  } else if (e.key === "Escape") {
    closeSuggest();
    $addr.blur();
    render();
  }
};

// Shortcuts work while the toolbar has focus; page focus needs native handling.
window.addEventListener("keydown", (e) => {
  if (e.key === "F5") { e.preventDefault(); nav("reload"); return; }
  if (e.altKey && e.key === "ArrowLeft") { e.preventDefault(); nav("back"); return; }
  if (e.altKey && e.key === "ArrowRight") { e.preventDefault(); nav("forward"); return; }
  if (!e.ctrlKey || e.altKey) return;
  if (e.key === "Tab") { e.preventDefault(); cycleTab(e.shiftKey ? -1 : 1); return; }
  const k = e.key.toLowerCase();
  if (k === "t") { e.preventDefault(); openTab(); }
  else if (k === "w") { e.preventDefault(); active && closeTab(active); }
  else if (k === "l") { e.preventDefault(); $addr.focus(); }
  else if (k === "r") { e.preventDefault(); nav("reload"); }
  else if (k === ",") { e.preventDefault(); openInternal("about:settings"); }
  else if (k === "n") { e.preventDefault(); openInternal("about:new"); }
});

// Custom window frame: drag and double-click maximize are handled natively by data-tauri-drag-region.
//window controls
$("#win-min").onclick = () => invoke("win_minimize");
$winMax.onclick = () => invoke("win_toggle_maximize");
$("#win-close").onclick = () => invoke("win_close");
let lastMax = null;
async function syncMax() {
  const max = await invoke("win_is_maximized");
  if (max === lastMax) return; // unchanged: skip the DOM write on every resize tick
  lastMax = max;
  $winMax.replaceChildren((max ? RESTORE_ICON_NODE : MAX_ICON_NODE).cloneNode(true));
  $winMax.title = max ? "Restore" : "Maximize";
  $winMax.setAttribute("aria-label", $winMax.title);
}
let resizeTimer;
window.addEventListener("resize", () => {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(syncMax, 150);
});
syncMax();

(async () => {
  const [initSettings, hasMica, launchUrl, initSession] = await Promise.all([
    invoke("get_settings").catch(() => null),
    invoke("has_mica").catch(() => false),
    invoke("take_launch_url").catch(() => null),
    invoke("load_session").catch(() => null),
  ]);

  if (initSettings) settings = { ...settings, ...initSettings };
  applyTheme();
  if (hasMica) document.documentElement.classList.add("mica");

  const session = settings.save_session ? initSession : null;
  const launch = launchUrl;

  // Reopen the previous tabs one after another (parallel webview creation is risky).
  for (const u of session?.tabs ?? []) await openTab(u);
  const ids = [...tabs.keys()];
  if (ids.length) activate(ids[Math.min(session?.active ?? 0, ids.length - 1)]);

  // protects from bad cli launch url, opens with home tab.
  if (launch) await openTab(launch ?? undefined);
  else if (!ids.length) await openTab(); // nothing to restore: home page

  restoring = false;
  saveSession();
})();
