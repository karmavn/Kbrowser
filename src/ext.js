const { invoke } = window.__TAURI__.core;
const $ = (s) => document.querySelector(s);
async function load() {
  const exts = await invoke("list_extensions");
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
    const meta = document.createElement("span");
    meta.textContent = `${e.version} · ${e.active ? "On" : "Starts after restart"}`;
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
      catch (err) { $("#msg").textContent = String(err); }
    };
    const rm = document.createElement("button");
    rm.className = "subtle";
    rm.textContent = "Remove";
    rm.onclick = async () => {
      try { await invoke("remove_extension", { id: e.id }); $("#msg").textContent = `Removed ${e.name}.`; }
      catch (err) { $("#msg").textContent = String(err); }
      load();
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

$("#addform").onsubmit = async (ev) => {
  ev.preventDefault();
  const input = $("#src").value.trim();
  if (!input) return;
  const btn = $("#addform button[type='submit']");
  const srcInput = $("#src");
  btn.disabled = true;
  srcInput.disabled = true;
  $("#msg").textContent = "Adding…";
  try {
    const e = await invoke("install_extension", { input });
    $("#msg").textContent = `Added ${e.name}.` + (e.active ? " Reload open tabs to use it." : "");
    srcInput.value = "";
  } catch (err) {
    $("#msg").textContent = String(err);
  } finally {
    btn.disabled = false;
    srcInput.disabled = false;
  }
  load();
};

$("#restart").onclick = () => invoke("restart_app");
load();
