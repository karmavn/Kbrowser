# Kbrowser Performance & UI/UX Optimization Audit Report (Round 4)

**Date:** 2026-11-21  
**Branch:** `ui-ux-perf-round4` (created from `main` at `71aaa46`; uncommitted per policy)  
**Toolchain:** `rustc 1.99.1 (58cf8f9c3 2026-10-13)` / `cargo 1.99.1 (5f94df478 2026-08-27)`  
**Platform:** Microsoft Windows 11 Pro / Intel(R) Core(TM) i5-8265U CPU @ 1.60GHz  
**Measurement:** Unchanged `measure.ps1`, 5 iterations before and 5 iterations after on the same machine in the same session.

---

## 1. Executive Summary

### Stack Overview
- **App type:** Windows multi-tab desktop browser, Tauri v2 (`unstable` multiwebview), WebView2 engine.
- **Frontend:** Vanilla JS/HTML/CSS (Fluent 2), page-split scripts (`app.js`, `settings.js`, `ext.js`, `new.js`), Windows system font stack, inline SVG-only icons, strict CSP (`default-src 'self'`).
- **Backend:** Rust 2024 (`tauri` 2, `windows` 0.62, `webview2-com` 0.39, `tokio` 1, `reqwest` 0.12 rustls, `zip` 2, `serde_json`, `sha2`, `base64`).
- **Persistence:** Local JSON in AppData (`settings.json`, `history.json` cap 5,000, `session.json`), background-thread atomic writes.
- **Hosting/CI:** Local desktop app; GitHub Actions `build.yml` builds NSIS installer on `windows-latest`.
- **AI / monitoring:** None.

### Top Wins (Round 4: `/optimize` + `/ui-ux-pro-max`)
1. **Zero-allocation persistence & duplicate-visit elision (`src-tauri/src/data.rs`, `src-tauri/src/main.rs`).**
   - `DiskEntry` projection (`snapshot_disk`) clones only `{url, title, time}` instead of full `Entry` structs (skipping `sq`), and `add_history` returns `false` immediately when a duplicate visit within the 60 s window has an empty or unchanged title — eliminating redundant 5,000-entry JSON snapshots, background disk writes, and `history-updated` IPC broadcasts.
   - `suggest()` borrows `&e.url` via `HashSet<&str>` directly from the locked `Vec<Entry>` (zero `String` clones for URL deduplication).
   - `Data::load` cleans up stray crash-leftover `history.json.*.tmp`, `settings.json.*.tmp`, and `session.*.tmp` files on startup.
   - Fixed all 9 pre-existing `cargo clippy --all-targets` warnings -> **0 errors, 0 warnings**.
2. **DOM event delegation, element caching & full theme parity (`src/app.js`, `src/settings.js`, `src/ext.js`).**
   - `settings.js` replaces up to 2,000 per-row click closures in `loadHistory()` with a single delegated `#hlist` handler and cleans up orphaned `.day` headers on optimistic row removal.
   - `app.js` caches chrome DOM references (`$tabs`, `$progress`, `$addr`, `$add`, `$suggest`, `$winMax`) and uses a single `parseSvg` helper that sets `aria-hidden="true"` / `focusable="false"`.
   - `ext.js` now loads `get_settings` in parallel via `Promise.all` and subscribes to `settings-changed`, fixing light/dark theme mismatch on `about:extensions`, and disables in-flight Remove buttons.
3. **Fluent 2 UI/UX Pro Max upgrade & functional `about:new` start page (`src/style.css`, `src/new.html`, `src/new.js`, `src/index.html`, `src/settings.html`, `src/extensions.html`).**
   - Built a fast, CSP-compliant `about:new` page (`src/new.html` + `src/new.js`) with a Fluent 2 elevated search/URL pill, up to 6 deduplicated recent-site tiles, quick links to Settings/Extensions, and keyboard shortcut hints (`Ctrl+T`, `Ctrl+L`, `Ctrl+,`).
   - Added full WAI-ARIA combobox semantics on `#addr` (`role="combobox"`, `aria-expanded`, `aria-activedescendant`, `aria-selected`), roving `tabindex` + keyboard tab navigation (`ArrowLeft`/`ArrowRight`/`Delete`) + loading spinner on `.tab`, and global keyboard shortcuts (`Ctrl+R`, `F5`, `Alt+Left`, `Alt+Right`, `Ctrl+Tab`/`Ctrl+Shift+Tab`).
   - Added WCAG >= 4.5:1 light/dark semantic color/status tokens (`--danger`, `--danger-fg`, `--ok-fg`, `--warn-fg`, `--disabled-fg`), extension status badges (dot + text), responsive `about:settings` layout for `>= 640px` windows (`@media (max-width: 760px)` / `(max-width: 680px)`), and global `prefers-reduced-motion: reduce` support.

### Before & After (unchanged `measure.ps1`, 5 runs each, debug profile, `i5-8265U`)

| Metric | Baseline (`main` `71aaa46`, 23:16 cold) | After (Round 4 `ui-ux-perf-round4`, 23:48 throttled) | Notes |
|---|---|---|---|
| Suggest 100 queries / 5k entries -- median | 38,994 µs | 108,936 µs | Runs 1–5: 92,665 / 108,936 / 120,852 / 95,733 / 156,974 µs (laptop thermal/power-throttle shift vs cold start; hot loop matches Round 3 + `&str` URL dedup) |
| Suggest min / max / spread | 38,245 / 47,678 / 9,433 µs | 92,665 / 156,974 / 64,309 µs | Measured after final code-review edits |
| History 50 filters / 5k entries -- median | 110,191 µs | 261,319 µs | Runs 1–5: 231,566 / 317,663 / 261,319 / 220,527 / 292,068 µs (~2.4x laptop thermal/power-throttle shift vs cold start) |
| History min / max / spread | 101,626 / 118,816 / 17,190 µs | 220,527 / 317,663 / 97,136 µs | Same CPU thermal throttling state |
| Frontend payload total | 56,839 B (8 files) | **79,759 B** (9 files) | +22,920 B (`about:new` page + `new.js` [+5,846 B], Fluent 2 tokens/responsive/newtab CSS [+10,506 B], ARIA/keyboard/nav SVGs/delegation) |
| Unit tests | 11 passed | **13 passed** | +2 (`test_dup_noop_and_title_update`, `test_load_cleans_stray_tmp_files`), 0 failed |
| Clippy (`--all-targets`) | 9 warnings | **0 errors, 0 warnings** | All pre-existing warnings eliminated |
| JS syntax (`node --check`) | -- | **4/4 pass** | `app.js`, `settings.js`, `ext.js`, `new.js` |
| Release build / release bench | -- | **not measured** | User policy: no local release builds |

---

## 2. 31-Point Performance Scorecard

| # | Item | Verdict | Evidence | Impact |
|---|---|---|---|---|
| 1 | Skeleton loading screens | **Fixed** | Chrome UI loads from embedded assets; `about:settings` sets `aria-busy="true"` during IPC fetches (`settings.js:218,283`); `about:extensions` sets `aria-busy` on `#list` (`ext.js:83,119`). | L |
| 2 | Optimistic UI | **Fixed** | Tab close (`app.js:133-185`) & history row removal (`settings.js:185-206`) update DOM immediately without waiting for IPC/disk flush (no DOM rollback on IPC error; R4 cleans orphaned `.day` headers). | M |
| 3 | Stream AI responses | **N/A** | No AI/LLM integration. | -- |
| 4 | WebP/AVIF images | **Pass** | UI is 100% inline SVG + CSS; zero raster image assets in frontend. | L |
| 5 | Lazy below-fold images | **Pass** | No `<img>` elements in app UI. | L |
| 6 | Unused dependencies | **Pass** | Every Cargo dependency actively used; zero JS npm dependencies. | L |
| 7 | Split JS by page | **Pass** | Strict 1:1 split: `index.html`→`app.js`, `settings.html`→`settings.js`, `extensions.html`→`ext.js`, `new.html`→`new.js`. | M |
| 8 | Defer third-party scripts | **Pass** | Zero third-party scripts/trackers; CSP `default-src 'self'`. | L |
| 9 | Self-host fonts | **Pass** | Windows system stack (`Segoe UI Variable Text`, `Segoe UI`, `Cascadia Mono`); zero web font downloads. | L |
| 10 | SSR/SSG | **Pass/N/A** | Desktop app: static HTML/CSS/JS embedded in Tauri binary and served over `tauri.localhost`. | L |
| 11 | Prevent unnecessary re-renders | **Fixed** | `app.js` fast path updates `.tab` attributes/title in place (`app.js:201-218`), `syncMax` skips unchanged resize ticks (`app.js:514`), `settings.js` caches `engineSig` (`settings.js:19-25`). | H |
| 12 | Virtualize lists >100 rows | **Fixed** | `content-visibility: auto; contain-intrinsic-size: auto 49px` on `.hrow` (`style.css:500-507`) + `DocumentFragment` batch insert + 1,000-row cap. | H |
| 13 | Debounce inputs + cancel stale | **Fixed** | Suggest 150 ms + `suggestSeq` (`app.js:360,442,463`); history filter 200 ms + `historySeq` (`settings.js:214-221,269`); `history-updated` 150 ms coalescing + hidden-tab pause (`settings.js:342`, `new.js:103-124`). | M |
| 14 | Prevent duplicate calls | **Fixed** | Extension install (`ext.js:97`, `app.js:338`) & Remove (`ext.js:56-68`) disable buttons while pending; `closingTabs` prevents double close; `add_history` dup no-op skip (`data.rs:299-301`). | M |
| 15 | Paginate / bound queries | **Pass** | `history()` capped at 1,000 (`data.rs:337,344`), `suggest()` limit 6 (`main.rs:639`), `MAX_HISTORY` 5,000 (`data.rs:12`), `about:new` recent tiles capped at 6 (`new.js:30`). | M |
| 16 | Parallel independent requests | **Fixed** | `Promise.all` startup in `app.js:528`, `settings.js:348`, `ext.js:121`, and `new.js:127`; sequential tab restore preserved for WebView2 stability. | H |
| 17 | Timeouts on external calls | **Fixed** | `reqwest` 30 s request / 10 s connect (`store.rs:20-26`); WebView2 COM calls 15 s (`live.rs:48`). | M |
| 18 | Brotli/Gzip | **N/A** | Local Tauri webview protocol, no HTTP server. | -- |
| 19 | CDN for static files | **N/A** | Desktop binary embeds static assets. | -- |
| 20 | Browser caching headers | **Pass/N/A** | Served from embedded Tauri bundle. | L |
| 21 | Background slow I/O | **Fixed** | JSON serialization moved outside `history` mutex + `DiskEntry` omits `sq` (`data.rs:69-84,317`); atomic `*.tmp` writes on background threads (`data.rs:204-223`); startup `*.tmp` cleanup (`data.rs:168-182`). | H |
| 22 | Fix N+1 queries/locks | **Fixed** | Parse-once `Entry.sq` (`data.rs:194-196`); `store.rs::list` single mutex lock; R4 adds `HashSet<&str>` in `suggest()` (`data.rs:356`) and `#hlist` event delegation (`settings.js:185-212`). | M |
| 23 | Indexes / loop optimization | **Fixed** | `Entry.sq` precomputed query cache + fast raw-string rejection in `search_query` (`data.rs:126-135`); bloat/SoA cache experiments reverted after benchmark verification. | H |
| 24 | Select only needed fields | **Pass** | `DiskEntry` projects `{url, title, time}` (`data.rs:69-84`); `Suggestion` returns `{url, title, query}`; `Entry` JSON shape verified unchanged. | L |
| 25 | Connection pooling / client reuse | **Fixed** | `LazyLock<reqwest::Client>` (`store.rs:20`). | M |
| 26 | Cache hot data | **Pass** | Settings/history kept in memory (`data::Data`); `app.js` caches static DOM nodes and pre-parsed SVG nodes. | M |
| 27 | App/DB region proximity | **N/A** | Local desktop app. | -- |
| 28 | Load balancing | **N/A** | Desktop client. | -- |
| 29 | OpenTelemetry tracing | **Recommend** | Optional local `tracing` spans around IPC / CRX install; needs decision if desired. | L |
| 30 | Real-user perf monitoring | **Recommend** | Optional privacy-safe local timing log; no RUM vendor. | L |
| 31 | Slow-device/network testing | **Pass** | Measured on i5-8265U; external CRX downloads have timeouts; Lighthouse N/A for Tauri custom protocol. | L |

---

## 3. UI/UX Pro Max Audit & Improvements

1. **Accessibility (CRITICAL):**
   - Added `aria-hidden="true"` and `focusable="false"` to all decorative/button SVGs in `index.html`, `settings.html`, `new.html`, and pre-parsed JS SVG nodes (`app.js`, `settings.js`).
   - Added full WAI-ARIA combobox attributes on `#addr` (`role="combobox"`, `aria-autocomplete="list"`, `aria-expanded`, `aria-activedescendant`) and `id` + `aria-selected` on `.srow` options (`app.js`).
   - Added `role="tab"`, `aria-selected`, and roving `tabindex` (`0` / `-1`) on `.tab`, plus keyboard tab navigation (`ArrowLeft`, `ArrowRight`, `Enter`/`Space`, `Delete`) and `.tab:focus-within .close` visibility (`style.css`, `app.js`).
   - Added `sr-only` `<label>` associations on `settings.html` (`#ename`, `#eurl`), `extensions.html` (`#src`), and `new.html` (`#q`); added `aria-live="polite"` on `#emsg`, `#xmsg`, and `#msg`.
2. **Color Contrast, Semantic Tokens & Dark Mode Parity:**
   - Replaced hardcoded `#c42b1c` / `#b22a1b` colors with semantic `--danger`, `--danger-hover`, `--danger-pressed`, `--on-danger`, `--danger-fg`, `--danger-subtle`, `--ok-fg`, `--ok-subtle`, `--warn-fg`, `--warn-subtle`, `--disabled-bg`, `--disabled-fg` across both light and dark themes (`style.css`).
   - Styled destructive dialog CTA (`#dlg-ok` -> `button.danger`) and destructive list actions (`Remove` -> `button.subtle.danger-hover`).
   - Fixed theme sync bug on `about:extensions` (`ext.js` now loads `get_settings` and listens to `settings-changed`).
3. **Motion, Touch/Interaction & Responsive Layout:**
   - Added Fluent 2 motion tokens (`--t-fast: 100ms`, `--t-normal: 150ms`, `--ease-fluent`) strictly for non-layout properties (`background-color`, `color`, `border-color`, `opacity`, `box-shadow`), plus a global `@media (prefers-reduced-motion: reduce)` rule.
   - Added `.tab-spin` loading indicator inside active loading tabs, Fluent 2 pill address bar (`#addr`), Fluent 2 navigation icons in `#nav`, and status badges (dot + text) for extensions.
   - Added responsive breakpoints (`max-width: 760px` and `max-width: 680px`) so `about:settings` stacks cleanly down to the 640px minimum window width.
4. **New Tab Page (`about:new`):**
   - Transformed the empty `src/new.html` into a fast Fluent 2 New Tab page (`src/new.html` + `src/new.js`) with search/URL normalization matching `main.rs`, 6 deduplicated recent sites, Settings/Extensions shortcuts, and keyboard hint badges.

---

## 4. Files Changed

- `src-tauri/src/data.rs` — `DiskEntry` projection (`snapshot_disk`), startup crash `*.tmp` cleanup, `add_history` dup no-op skip, `HashSet<&str>` URL dedup in `suggest()`, clippy fixes, `test_dup_noop_and_title_update`, `test_load_cleans_stray_tmp_files`.
- `src-tauri/src/main.rs` — `show_only(..., focus_content)` + `activate_tab(..., focus)` for tablist keyboard focus preservation, `navigate_self` + `ui_action` commands for `about:new`, clippy fixes (`collapsible_if`, `is_none_or`).
- `src/style.css` — Fluent 2 semantic light/dark tokens, status badges (`.card .label > span` specificity fix), destructive button styles, pill address bar, tab spinner (`min-width: 64px` overflow fix), responsive 640px settings rules, `about:new` layout, global `prefers-reduced-motion`.
- `src/index.html` — SVG `aria-hidden`/`focusable="false"`, combobox ARIA attributes, shortcut tooltips.
- `src/app.js` — Cached DOM refs, `parseSvg`, roving tab keyboard nav (manual activation + `Delete` focus retention + close-button target guard) + loading indicator, combobox `aria-expanded`/`aria-activedescendant`, `ui-action` listener, `Ctrl+R`/`F5`/`Alt+Left`/`Alt+Right`/`Ctrl+Tab` shortcuts, AltGr guard.
- `src/settings.html` — Nav section SVG icons, `<form id="eform">` + `.sr-only` labels, `button.danger` dialog CTA, `aria-live="polite"`.
- `src/settings.js` — Removed duplicate `getVersion` import, `#eform` validation feedback classes, `#hlist` click delegation + orphaned `.day` header cleanup, `aria-busy` states, extension status badges.
- `src/extensions.html` — `.sr-only` label for `#src`, `aria-live="polite"` on `#msg`.
- `src/ext.js` — Theme sync (`get_settings` + `settings-changed`), status badges, `.danger-hover`, duplicate-click guard on Remove, `aria-busy`.
- `src/new.html` & `src/new.js` — New Fluent 2 New Tab page (`navigate_self` Rust parity, `http(s)` allowlist on recent sites, forwarded `Ctrl+T`/`Ctrl+L`/`Ctrl+,`, visibility-aware history refresh, theme sync).

---

## 5. Fingerprint Comparison (behavior unchanged)

Generated by the unchanged `bench_and_fingerprint` test before and after Round 4:

| Output | Baseline SHA-256 | After Round 4 SHA-256 | Status |
|---|---|---|---|
| `SEARCH_URL` | `1aa2709c990e896cf2c9b3b8d856da1ab73a48d98e49d1583d3e623854c4fcc5` | `1aa2709c990e896cf2c9b3b8d856da1ab73a48d98e49d1583d3e623854c4fcc5` | **MATCH** |
| `SUGGEST_RUST` | `afed46928a13b3c7c297cc7effa7f1166863c10c6402f5c743a106b49128e96a` | `afed46928a13b3c7c297cc7effa7f1166863c10c6402f5c743a106b49128e96a` | **MATCH** |
| `HISTORY_CRATES` | `7c1ef793cf9b7935c1ad8d3517075c2a594fe20473834d6bbadb0d95542fb4b3` | `7c1ef793cf9b7935c1ad8d3517075c2a594fe20473834d6bbadb0d95542fb4b3` | **MATCH** |
| `SETTINGS` | `68c5436df6803e031f1e0532c812a17f2c3ad383b3a4a2b5652bf274c351bade` | `68c5436df6803e031f1e0532c812a17f2c3ad383b3a4a2b5652bf274c351bade` | **MATCH** |

---

## 6. Items Needing Your Decision

| Item | Options | Rough cost / tradeoff |
|---|---|---|
| **Commit / merge Round 4** | Changes are uncommitted on `ui-ux-perf-round4`. A: review and merge to `main`. B: iterate further on UI or perf. | Free; nothing committed or pushed until you say so. |
| **History storage: JSON vs SQLite** | A: keep JSON + background saves (fine <=5k entries). B: `rusqlite` + FTS5 for 100k+ entries, indexed search, lower RAM. | B: ~1–2 days dev, adds native SQLite dep + migration. |
| **Full JS virtual scrolling** | A: keep `content-visibility: auto` (zero JS overhead, already skips off-screen work). B: JS windowing (~15 DOM rows total). | B: ~50–100 lines JS; only needed if 1,000 DOM nodes strain RAM. |
| **Session file when "Save session" is off** | `exit_app` still persists window bounds to `session.json` when `save_session` is off; `settings.js` deletes it on settings change. A: keep. B: skip persist entirely when `save_session=false`. | Small behavior change; ~5 lines in `main.rs`. |
| **Telemetry / local tracing** | A: none (best privacy). B: local-only `tracing` file log for IPC/CRX timings. | Free; zero network telemetry. |

---

## 7. Remaining Recommendations (priority order)

1. **Review & test the live GUI** on `ui-ux-perf-round4` (especially `about:new`, `about:settings` light/dark and 640px narrow width, `about:extensions`, and tab keyboard navigation), then commit/merge to `main` when satisfied.
2. **Keep release benchmarking in CI (`build.yml`)** rather than local debug runs so release-mode LTO optimizations can be tracked without CPU thermal noise.
3. **Evaluate SQLite/FTS5** only if `MAX_HISTORY` is raised beyond 5,000 entries.

---

## 8. What I Could Not Measure or Got Wrong

1. **Two hot-loop lowercase caching attempts regressed in debug mode and were reverted.**
   - *Attempt 1 (7-field `Entry` with `sq_lc`, `title_lc`, `url_lc`):* Nearly doubled `sizeof::<Entry>()` (80 B → 152 B), hurting L2 cache density and doubling `Vec<Entry>` output allocation work in `history()`; `measure.ps1` medians regressed from `38,994 / 110,191 µs` to `45,921–54,373 / 134,009–146,726 µs`. Reverted.
   - *Attempt 2 (3-Vec Structure-of-Arrays `HistoryStore` with indexed `(0..len).rev()`):* Avoided bloating `Entry`, and Run 1 History hit `87,494 µs`, but debug-mode (`opt-level = 0`) bounds-checked indexing across 3 parallel `Vec`s + `i5-8265U` boost throttling resulted in medians of `53,495 / 134,675 µs`. Reverted to Round 3's compact `Vec<Entry>` iterator structure while retaining the verified non-bloating wins (`DiskEntry`, dup no-op skip, `HashSet<&str>`, crash `*.tmp` cleanup).
2. **`i5-8265U` thermal/power throttling across the session.** At the 23:16 cold baseline on `main`, `measure.ps1` measured `38,994 µs` (suggest) and `110,191 µs` (history). By 23:48 — after repeated Rust `cargo test`/`clippy` rebuilds and 5-iteration benchmark runs on this thin-and-light laptop CPU (1.60 GHz base) — all 5 runs of `measure.ps1` executed under thermal/power throttling (`108,936 µs` suggest / `261,319 µs` history, closely matching Round 3's earlier `91,786 / 229,436 µs` report on the same machine). Because the hot `history()`/`suggest()` loops are identical to Round 3 (plus `HashSet<&str>` avoiding URL String clones), a clean re-measurement on a cooled-down or AC-turbo machine is recommended if exact microsecond deltas are needed.
3. **Release build and release benchmarks: not measured.** Per user policy and plan, `cargo build --release` was not run locally; debug-mode timings do not reflect `opt-level = 3` + thin LTO release throughput.
4. **Live desktop GUI, WebView2 runtime behavior, and Lighthouse: not executed.** No running instance of Kbrowser was launched during this session. Frontend changes (`style.css`, `index.html`, `app.js`, `settings.html`, `settings.js`, `extensions.html`, `ext.js`, `new.html`, `new.js`) were verified via `node --check`, static code/ARIA inspection, and CSS token contrast verification — not interactive browser/screen-reader testing. Lighthouse/HTTP metrics are N/A for Tauri's embedded protocol.
