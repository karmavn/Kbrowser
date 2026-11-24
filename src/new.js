const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (s) => document.querySelector(s);
const SEARCH_HOST = /^(?:(?:[a-z0-9-]+\.)*(?:duckduckgo\.com|bing\.com|brave\.com|ecosia\.org)|(?:www\.)?google\.(?:[a-z]{2,3}|(?:co|com)\.[a-z]{2,3}))$/i;

let settings = { theme: "system" };

function applyTheme() {
  const t = settings?.theme;
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
}

function renderRecent(items) {
  const wrap = $("#recent-wrap");
  const grid = $("#recent");
  const seenHosts = new Set();
  const tiles = [];

  for (const e of items ?? []) {
    let u;
    try { u = new URL(e.url); } catch { continue; }
    if ((u.protocol !== "http:" && u.protocol !== "https:") || u.hostname === "tauri.localhost") continue;
    if (SEARCH_HOST.test(u.hostname) && u.searchParams.get("q")) continue;
    const host = u.hostname.replace(/^www\./, "");
    if (!host || seenHosts.has(host)) continue;
    seenHosts.add(host);
    tiles.push({ url: e.url, title: e.title || host, host });
    if (tiles.length >= 6) break;
  }

  if (!tiles.length) {
    wrap.hidden = true;
    grid.replaceChildren();
    return;
  }

  const frag = document.createDocumentFragment();
  for (const t of tiles) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "quick-tile";
    btn.dataset.url = t.url;
    btn.title = t.url;

    const badge = document.createElement("span");
    badge.className = "tile-badge";
    badge.setAttribute("aria-hidden", "true");
    badge.textContent = (t.host[0] || "•").toUpperCase();

    const text = document.createElement("span");
    text.className = "tile-text";
    const title = document.createElement("span");
    title.className = "tile-title";
    title.textContent = t.title;
    const host = document.createElement("span");
    host.className = "tile-host";
    host.textContent = t.host;
    text.append(title, host);

    btn.append(badge, text);
    frag.append(btn);
  }
  grid.replaceChildren(frag);
  wrap.hidden = false;
}

$("#new-search").onsubmit = (ev) => {
  ev.preventDefault();
  const raw = $("#q").value.trim();
  if (!raw) return;
  invoke("navigate_self", { input: raw }).catch((err) => console.error("navigate_self failed:", err));
};

$("#recent").onclick = (ev) => {
  const tile = ev.target.closest(".quick-tile");
  if (tile?.dataset.url) {
    invoke("navigate_self", { input: tile.dataset.url }).catch((err) => console.error("navigate_self failed:", err));
  }
};

$("#open-settings").onclick = () => invoke("open_page", { url: "about:settings" });
$("#open-extensions").onclick = () => invoke("open_page", { url: "about:extensions" });

window.addEventListener("keydown", (e) => {
  if (!e.ctrlKey || e.altKey) return;
  const k = e.key.toLowerCase();
  if (k === "t") { e.preventDefault(); invoke("ui_action", { action: "new-tab" }); }
  else if (k === "l") { e.preventDefault(); invoke("ui_action", { action: "focus-addr" }); }
  else if (k === ",") { e.preventDefault(); invoke("ui_action", { action: "open-settings" }); }
});

listen("settings-changed", ({ payload }) => {
  if (payload) {
    settings = payload;
    applyTheme();
  }
});

let reloadTimer;
let historyDirty = false;
function scheduleRefresh() {
  clearTimeout(reloadTimer);
  reloadTimer = setTimeout(async () => {
    const items = await invoke("recent_sites", { limit: 6 }).catch(() => []);
    renderRecent(items);
  }, 150);
}

listen("history-updated", () => {
  if (document.hidden) {
    historyDirty = true;
    return;
  }
  scheduleRefresh();
});

document.addEventListener("visibilitychange", () => {
  if (!document.hidden && historyDirty) {
    historyDirty = false;
    scheduleRefresh();
  }
});

(async () => {
  const [initSettings, items] = await Promise.all([
    invoke("get_settings").catch(() => null),
    invoke("recent_sites", { limit: 6 }).catch(() => []),
  ]);
  if (initSettings) settings = { ...settings, ...initSettings };
  applyTheme();
  renderRecent(items);
})();
