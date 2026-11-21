const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s) => document.querySelector(s);

function applyTheme(theme) {
  if (theme === "light" || theme === "dark") document.documentElement.dataset.theme = theme;
  else delete document.documentElement.dataset.theme;
}

function setMsg(text, kind = "") {
  const el = $("#msg");
  el.className = kind;
  el.textContent = text;
}

function renderList(exts) {
  const list = $("#list");
  const frag = document.createDocumentFragment();
  if (!exts.length) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = "No extensions yet. Open an extension on the Chrome Web Store and select Add to browser.";
    frag.append(li);
  }
  for (const e of exts) {
    const li = document.createElement("li");
    const text = document.createElement("div");
    text.className = "ext-text";
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
    text.append(name, meta);

    const opts = document.createElement("button");
    opts.className = "subtle";
    opts.textContent = "Settings";
    opts.disabled = !e.options || !e.active;
    opts.title = !e.options
      ? "This extension has no settings page"
      : !e.active ? "Restart the browser to start this extension" : "";
    opts.onclick = async () => {
      try { await invoke("open_extension_options", { id: e.id }); }
      catch (err) { setMsg(String(err), "msg-error"); }
    };

    const rm = document.createElement("button");
    rm.className = "subtle danger-hover";
    rm.textContent = "Remove";
    rm.onclick = async () => {
      rm.disabled = true;
      rm.textContent = "Removing…";
      try {
        await invoke("remove_extension", { id: e.id });
        setMsg(`Removed ${e.name}.`, "msg-ok");
      } catch (err) {
        setMsg(String(err), "msg-error");
        rm.disabled = false;
        rm.textContent = "Remove";
        return;
      }
      await load();
    };

    const actions = document.createElement("div");
    actions.className = "actions";
    actions.append(opts, rm);
    li.append(text, actions);
    frag.append(li);
  }
  list.replaceChildren(frag);
  $("#banner").hidden = !exts.some((e) => !e.active);
}

async function load() {
  const list = $("#list");
  list.setAttribute("aria-busy", "true");
  try {
    const exts = await invoke("list_extensions");
    renderList(exts);
  } finally {
    list.removeAttribute("aria-busy");
  }
}

$("#addform").onsubmit = async (ev) => {
  ev.preventDefault();
  const input = $("#src").value.trim();
  if (!input) return;
  const btn = $("#addform button[type='submit']");
  const srcInput = $("#src");
  btn.disabled = true;
  srcInput.disabled = true;
  setMsg("Adding…");
  try {
    const e = await invoke("install_extension", { input });
    setMsg(`Added ${e.name}.` + (e.active ? " Reload open tabs to use it." : ""), "msg-ok");
    srcInput.value = "";
  } catch (err) {
    setMsg(String(err), "msg-error");
  } finally {
    btn.disabled = false;
    srcInput.disabled = false;
  }
  load();
};

$("#restart").onclick = () => invoke("restart_app");
listen("settings-changed", ({ payload }) => applyTheme(payload?.theme));

(async () => {
  const list = $("#list");
  list.setAttribute("aria-busy", "true");
  try {
    const [initSettings, exts] = await Promise.all([
      invoke("get_settings").catch(() => null),
      invoke("list_extensions").catch(() => []),
    ]);
    if (initSettings) applyTheme(initSettings.theme);
    renderList(exts);
  } finally {
    list.removeAttribute("aria-busy");
  }
})();
