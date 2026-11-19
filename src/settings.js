const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getVersion} = window.__TAURI__.app;

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
  root.replaceChildren();
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
    rm.className = "subtle";
    rm.textContent = "Remove";
    rm.onclick = () => {
      const custom_engines = settings.custom_engines.filter((x) => x.id !== e.id);
      save({ custom_engines, search: settings.search === e.id ? "duckduckgo" : settings.search });
    };
    card.append(label, rm);
    root.append(card);
  }
}

$("#eadd").onclick = () => {
  const name = $("#ename").value.trim();
  const url = $("#eurl").value.trim();
  const msg = $("#emsg");
  if (!name) return void (msg.textContent = "Give the search engine a name.");
  let ok = false;
  try { ok = /^https?:$/.test(new URL(url.replaceAll("%s", "test")).protocol); } catch {}
  if (!ok || !url.includes("%s")) {
    return void (msg.textContent = "Enter an http(s) address that contains %s where the search terms go.");
  }
  const id = "c" + Date.now().toString(36);
  msg.textContent = `Added ${name}.`;
  $("#ename").value = "";
  $("#eurl").value = "";
  save({ custom_engines: [...(settings.custom_engines ?? []), { id, name, url }] });
};

function syncControls() {
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
  settings = { ...settings, ...patch };
  syncControls();
  try { await invoke("set_settings", { settings }); } catch (e) { console.error(e); }

  
//deleting the session the moment of deactivation to avoid unnecessary data storeage
  if (!settings.save_session){
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
$("#savehist").onclick = () => save({ save_history: !settings.save_history });
$("#savesession").onclick = () => save({save_session: !settings.save_session});
$("#showrec").onclick = () => save({show_suggestions: !settings.show_suggestions});

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

const RM_SVG_NODE = (() => {
  const d = document.createElement("div");
  d.innerHTML = '<svg viewBox="0 0 24 24"><path d="M6 6l12 12M18 6L6 18"/></svg>';
  return d.firstElementChild;
})();

// todayStart is passed in so a 1,000-row render computes "today" once, and each
// row reuses one Date (setHours) instead of allocating three plus a closure.
function dayLabel(ms, todayStart) {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  const diff = Math.round((todayStart - d.getTime()) / 86400000);
  if (diff === 0) return "Today";
  if (diff === 1) return "Yesterday";
  return dayFormatter.format(ms);
}

const SEARCH_HOST = /(^|\.)(duckduckgo\.com|google\.[a-z.]+|bing\.com|brave\.com|ecosia\.org)$/;

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

let historySeq = 0;
async function loadHistory() {
  const seq = ++historySeq;
  const q = $("#hq").value;
  const items = await invoke("history_list", { query: q });
  if (seq !== historySeq) return; // a newer query already answered: drop stale list
  const root = $("#hlist");
  const empty = $("#hempty");
  empty.hidden = items.length > 0;
  empty.textContent = q.trim() ? "No matching history." : "No browsing history yet.";

  const frag = document.createDocumentFragment();
  const todayStart = new Date().setHours(0, 0, 0, 0);
  let lastDay = "";
  for (const e of items) {
    const day = dayLabel(e.time, todayStart);
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

    const main = document.createElement("div");
    main.className = "hmain";
    const open = document.createElement("button");
    open.className = "link";
    open.textContent = d.title;
    open.title = e.url;
    open.onclick = () => invoke("open_page", { url: e.url });
    const sub = document.createElement("span");
    sub.className = "sub";
    sub.textContent = `${d.host} · ${timeFormatter.format(e.time)}`;
    main.append(open, sub);

    const rm = document.createElement("button");
    rm.className = "icon";
    rm.title = "Remove from history";
    rm.setAttribute("aria-label", "Remove from history");
    rm.appendChild(RM_SVG_NODE.cloneNode(true));
    rm.onclick = () => {
      // Optimistic UI removal: remove row immediately without waiting for disk flush
      row.remove();
      invoke("history_remove", { time: e.time, url: e.url });
      if (!root.querySelector(".hrow")) {
        root.replaceChildren();
        empty.hidden = false;
      }
    };

    row.append(main, rm);
    frag.append(row);
  }
  root.replaceChildren(frag);
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
  const exts = await invoke("list_extensions");
  const root = $("#xlist");
  $("#xmsg").textContent = exts.length ? "" : "No extensions installed.";
  const frag = document.createDocumentFragment();
  for (const e of exts) {
    const card = document.createElement("div");
    card.className = "card";

    const label = document.createElement("div");
    label.className = "label";
    const name = document.createElement("strong");
    name.textContent = e.name;
    const meta = document.createElement("span");
    meta.textContent = `${e.version} · ${e.active ? "On" : "Starts after restart"}`;
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
      catch (err) { $("#xmsg").textContent = String(err); }
    };

    card.append(label, btn);
    frag.append(card);
  }
  root.replaceChildren(frag);
}

$("#store").onclick = () => invoke("open_page", { url: "https://chromewebstore.google.com/" });
$("#manage").onclick = () => invoke("open_page", { url: "about:extensions" });

// ---------- Events + startup ----------

listen("settings-changed", ({ payload }) => { settings = payload; syncControls(); });
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
  const versionExists = $("#kbrowser-version");
  if (versionExists && appVersion) {
    versionExists.innerText = appVersion;
  }
})();
