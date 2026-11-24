const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (s) => document.querySelector(s);
const navItems = [...document.querySelectorAll(".navitem")];

let settings = null;

// ---------- Settings ----------

function applyTheme() {
  const t = settings?.theme;
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
}

// ---------- Custom search engines ----------

let engineSig = null;

function renderEngines() {
  const engines = settings.custom_engines ?? [];
  const sig = JSON.stringify(engines);
  if (sig === engineSig) return;
  engineSig = sig;

  // Dropdown: built-in engines are static in the HTML; custom ones live in their own group.
  const select = $("#search");
  select.querySelector("optgroup")?.remove();
  if (engines.length) {
    const group = document.createElement("optgroup");
    group.label = "Custom";
    for (const e of engines) {
      const o = document.createElement("option");
      o.value = e.id;
      o.textContent = e.name;
      group.append(o);
    }
    select.append(group);
  }

  const root = $("#elist");
  const frag = document.createDocumentFragment();
  for (const e of engines) {
    const card = document.createElement("div");
    card.className = "card";
    const label = document.createElement("div");
    label.className = "label";
    const name = document.createElement("strong");
    name.textContent = e.name;
    const url = document.createElement("span");
    url.className = "mono";
    url.textContent = e.url;
    label.append(name, url);
    const rm = document.createElement("button");
    rm.className = "subtle danger-hover";
    rm.textContent = "Remove";
    rm.onclick = () => {
      const custom_engines = settings.custom_engines.filter((x) => x.id !== e.id);
      save({ custom_engines, search: settings.search === e.id ? "duckduckgo" : settings.search });
    };
    card.append(label, rm);
    frag.append(card);
  }
  root.replaceChildren(frag);
}

$("#eform").onsubmit = (ev) => {
  ev.preventDefault();
  if (!settings) return;
  const name = $("#ename").value.trim();
  const url = $("#eurl").value.trim();
  const msg = $("#emsg");
  if (!name) {
    msg.className = "empty msg-error";
    msg.textContent = "Give the search engine a name.";
    return;
  }
  let ok = false;
  try { ok = /^https?:$/.test(new URL(url.replaceAll("%s", "test")).protocol); } catch {}
  if (!ok || !url.includes("%s")) {
    msg.className = "empty msg-error";
    msg.textContent = "Enter an http(s) address that contains %s where the search terms go.";
    return;
  }
  const id = "c" + Date.now().toString(36);
  msg.className = "empty msg-ok";
  msg.textContent = `Added ${name}.`;
  $("#ename").value = "";
  $("#eurl").value = "";
  save({ custom_engines: [...(settings.custom_engines ?? []), { id, name, url }] });
};

function setControlsDisabled(disabled) {
  for (const el of document.querySelectorAll("#pane-general select, #pane-general input, #pane-general button, #pane-appearance select, #pane-privacy button.switch")) {
    el.disabled = disabled;
  }
}
setControlsDisabled(true);

function syncControls() {
  if (!settings) return;
  setControlsDisabled(false);
  applyTheme();
  renderEngines();
  const set = (el, v) => { if (document.activeElement !== el) el.value = v; };
  set($("#search"), settings.search);
  set($("#home"), settings.home);
  set($("#theme"), settings.theme);
  $("#savehist").setAttribute("aria-checked", String(settings.save_history));
  $("#savesession").setAttribute("aria-checked", String(settings.save_session));
  $("#showrec").setAttribute("aria-checked", String(settings.show_suggestions));
}

async function save(patch) {
  if (!settings) return;
  const wasSaveSession = settings.save_session;
  settings = { ...settings, ...patch };
  syncControls();
  try { await invoke("set_settings", { settings }); } catch (e) { console.error(e); }

  // Clear saved tabs only when Save session transitions from on -> off.
  if (wasSaveSession && !settings.save_session) {
    try {
      await invoke("delete_session");
    } catch (err) {
      console.error("Failed to delete session file:", err);
    }
  }
}

$("#search").onchange = (e) => save({ search: e.target.value });
$("#theme").onchange = (e) => save({ theme: e.target.value });
$("#home").onchange = (e) => save({ home: e.target.value });
$("#home").onkeydown = (e) => { if (e.key === "Enter") e.target.blur(); };
$("#savehist").onclick = () => settings && save({ save_history: !settings.save_history });
$("#savesession").onclick = () => settings && save({ save_session: !settings.save_session });
$("#showrec").onclick = () => settings && save({ show_suggestions: !settings.show_suggestions });

// ---------- Navigation ----------

function show(name) {
  for (const b of navItems) {
    const on = b.dataset.pane === name;
    if (on) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
    $(`#pane-${b.dataset.pane}`).hidden = !on;
  }
  if (name === "privacy") loadHistory();
  if (name === "extensions") loadExts();
}
for (const b of navItems) b.onclick = () => show(b.dataset.pane);

// ---------- History ----------

const timeFormatter = new Intl.DateTimeFormat([], { hour: "numeric", minute: "2-digit" });
const dayFormatter = new Intl.DateTimeFormat([], { weekday: "long", month: "long", day: "numeric" });
const dayWithYearFormatter = new Intl.DateTimeFormat([], { weekday: "long", month: "long", day: "numeric", year: "numeric" });

const RM_SVG_NODE = (() => {
  const d = document.createElement("div");
  d.innerHTML = '<svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6L6 18"/></svg>';
  const svg = d.firstElementChild;
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  return svg;
})();

// todayStart is passed in so a 1,000-row render computes "today" once, and each
// row reuses one Date (setHours) instead of allocating three plus a closure.
function dayLabel(ms, todayStart, currentYear) {
  const d = new Date(ms);
  const year = d.getFullYear();
  d.setHours(0, 0, 0, 0);
  const diff = Math.round((todayStart - d.getTime()) / 86400000);
  if (diff === 0) return "Today";
  if (diff === 1) return "Yesterday";
  return year === currentYear ? dayFormatter.format(ms) : dayWithYearFormatter.format(ms);
}

const SEARCH_HOST = /^(?:(?:[a-z0-9-]+\.)*(?:duckduckgo\.com|bing\.com|brave\.com|ecosia\.org)|(?:www\.)?google\.(?:[a-z]{2,3}|(?:co|com)\.[a-z]{2,3}))$/i;

// Search result pages are shown as "Search: <query>" so searches are easy to spot.
function describe(e) {
  try {
    const u = new URL(e.url);
    const host = u.hostname.replace(/^www\./, "");
    const q = SEARCH_HOST.test(u.hostname) && u.searchParams.get("q");
    return { title: q ? `Search: ${q}` : e.title || host, host };
  } catch {
    return { title: e.title || e.url, host: e.url };
  }
}

const $hlist = $("#hlist");
const $hempty = $("#hempty");

// Single delegated click handler for up to 1,000 history rows (avoids 2,000 per-row closures).
$hlist.onclick = (ev) => {
  const rmBtn = ev.target.closest("button.icon");
  if (rmBtn) {
    const row = rmBtn.closest(".hrow");
    if (!row) return;
    const time = Number(row.dataset.time);
    const url = row.dataset.url;
    // Remove orphaned day heading when its last row is removed.
    const prev = row.previousElementSibling;
    const next = row.nextElementSibling;
    if (prev?.classList.contains("day") && (!next || next.classList.contains("day"))) {
      prev.remove();
    }
    row.remove();
    invoke("history_remove", { time, url });
    if (!$hlist.querySelector(".hrow")) {
      $hlist.replaceChildren();
      $hempty.hidden = false;
      $hempty.textContent = $("#hq").value.trim() ? "No matching history." : "No browsing history yet.";
    }
    return;
  }
  const linkBtn = ev.target.closest("button.link");
  if (linkBtn) {
    const row = linkBtn.closest(".hrow");
    if (row?.dataset.url) invoke("open_page", { url: row.dataset.url });
  }
};

let historySeq = 0;
async function loadHistory() {
  const seq = ++historySeq;
  const q = $("#hq").value;
  $hlist.setAttribute("aria-busy", "true");
  try {
    const items = await invoke("history_list", { query: q });
    if (seq !== historySeq) return; // a newer query already answered: drop stale list
    $hempty.hidden = items.length > 0;
    $hempty.textContent = q.trim() ? "No matching history." : "No browsing history yet.";

    const frag = document.createDocumentFragment();
    const todayDate = new Date();
    const currentYear = todayDate.getFullYear();
    const todayStart = todayDate.setHours(0, 0, 0, 0);
    let lastDay = "";
    for (const e of items) {
      const day = dayLabel(e.time, todayStart, currentYear);
      if (day !== lastDay) {
        lastDay = day;
        const h = document.createElement("h4");
        h.className = "day";
        h.textContent = day;
        frag.append(h);
      }
      const d = describe(e);
      const row = document.createElement("div");
      row.className = "hrow";
      row.dataset.time = String(e.time);
      row.dataset.url = e.url;

      const main = document.createElement("div");
      main.className = "hmain";
      const open = document.createElement("button");
      open.className = "link";
      open.textContent = d.title;
      open.title = e.url;
      const sub = document.createElement("span");
      sub.className = "sub";
      sub.textContent = `${d.host} · ${timeFormatter.format(e.time)}`;
      main.append(open, sub);

      const rm = document.createElement("button");
      rm.className = "icon";
      rm.title = "Remove from history";
      rm.setAttribute("aria-label", "Remove from history");
      rm.appendChild(RM_SVG_NODE.cloneNode(true));

      row.append(main, rm);
      frag.append(row);
    }
    $hlist.replaceChildren(frag);
  } finally {
    if (seq === historySeq) $hlist.removeAttribute("aria-busy");
  }
}

let timer;
$("#hq").oninput = () => { clearTimeout(timer); timer = setTimeout(loadHistory, 200); };

$("#clear").onclick = () => $("#dlg").showModal();
$("#dlg-cancel").onclick = () => $("#dlg").close();
$("#dlg-ok").onclick = async () => {
  await invoke("history_clear", { range: $("#range").value });
  $("#dlg").close();
};

// ---------- Extensions ----------

async function loadExts() {
  const root = $("#xlist");
  root.setAttribute("aria-busy", "true");
  try {
    const exts = await invoke("list_extensions");
    $("#xmsg").className = "empty";
    $("#xmsg").textContent = exts.length ? "" : "No extensions installed.";
    const frag = document.createDocumentFragment();
    for (const e of exts) {
      const card = document.createElement("div");
      card.className = "card";

      const label = document.createElement("div");
      label.className = "label";
      const name = document.createElement("strong");
      name.textContent = e.name;
      const meta = document.createElement("div");
      meta.className = "ext-meta";
      const ver = document.createElement("span");
      ver.textContent = `v${e.version}`;
      const badge = document.createElement("span");
      badge.className = e.active ? "badge ok" : "badge pending";
      badge.textContent = e.active ? "On" : "Starts after restart";
      meta.append(ver, badge);
      label.append(name, meta);

      const btn = document.createElement("button");
      btn.className = "subtle";
      btn.textContent = "Settings";
      btn.disabled = !e.options || !e.active;
      btn.title = !e.options
        ? "This extension has no settings page"
        : !e.active ? "Restart the browser to start this extension" : "";
      btn.onclick = async () => {
        try { await invoke("open_extension_options", { id: e.id }); }
        catch (err) {
          $("#xmsg").className = "empty msg-error";
          $("#xmsg").textContent = String(err);
        }
      };

      card.append(label, btn);
      frag.append(card);
    }
    root.replaceChildren(frag);
  } finally {
    root.removeAttribute("aria-busy");
  }
}

$("#store").onclick = () => invoke("open_page", { url: "https://chromewebstore.google.com/" });
$("#manage").onclick = () => invoke("open_page", { url: "about:extensions" });

// ---------- Events + startup ----------

listen("settings-changed", ({ payload }) => { settings = payload; syncControls(); });
listen("extensions-changed", () => {
  if (!$("#pane-extensions").hidden) loadExts();
});
// Debounced: a burst of history-updated events (e.g. several tabs finishing loads,
// or the echo of our own optimistic removal) coalesces into one list rebuild.
let historyReloadTimer;
listen("history-updated", () => {
  if ($("#pane-privacy").hidden) return;
  clearTimeout(historyReloadTimer);
  historyReloadTimer = setTimeout(loadHistory, 150);
});

(async () => {
  // Fetch initial settings, webview version, and app version in parallel
  const [initSettings, wvVersion, appVersion] = await Promise.all([
    invoke("get_settings").catch(() => null),
    invoke("about_webview").catch(() => "unknown"),
    (async () => {
      try {
        const { getVersion } = window.__TAURI__.app;
        return await getVersion();
      } catch {
        return "";
      }
    })(),
  ]);

  if (initSettings) {
    settings = initSettings;
    syncControls();
  }
  $("#wv").textContent = wvVersion;
  const versionEl = $("#kbrowser-version");
  if (versionEl && appVersion) {
    versionEl.textContent = appVersion;
  }
})();
