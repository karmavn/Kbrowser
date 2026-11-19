# Kbrowser Performance Optimization Audit & Report (Round 3)

**Date:** 2026-11-19  
**Branch:** `perf-optimize-round3` (created from `perf-optimize-round2` HEAD `8ef3b04`; Round 2 changes were uncommitted and carried over; nothing committed, per policy)  
**Toolchain:** `rustc 1.99.1` / `cargo 1.99.1`  
**Platform:** Microsoft Windows 11 Pro / Intel(R) Core(TM) i5-8265U CPU @ 1.60GHz  
**Measurement:** unchanged `measure.ps1` (via `measure.sh`), 5 runs before and 5 runs after, same machine, ~18 minutes apart. Raw logs: `%TMPDIR%\baseline_round3.txt`, `after_round3_final2.txt`.

---

## 1. Executive Summary

### Stack Overview
- **App type:** Windows multi-tab desktop browser, Tauri v2 (`unstable` multiwebview), WebView2 engine.
- **Frontend:** vanilla JS/HTML/CSS (Fluent 2), one script per page (`app.js`, `settings.js`, `ext.js`), system font stack, SVG-only icons.
- **Backend:** Rust 2024 (`tauri` 2, `windows` 0.62, `webview2-com` 0.39, `tokio` 1, `reqwest` 0.12 rustls, `zip` 2, `serde_json`, `sha2`, `base64`).
- **Persistence:** local JSON in AppData (`settings.json`, `history.json` cap 5,000, `session.json`), background-thread writes.
- **Hosting/CI:** no server; GitHub Actions `build.yml` builds NSIS + portable exe on `windows-latest`.
- **AI / monitoring:** none.

### Top 3 Wins (Round 3)
1. **Parse-once search-query cache for suggestions (`src-tauri/src/data.rs`).**
   `suggest()` used to call `Url::parse` on every search-engine URL in history for **every keystroke** (~2,000 parses per query at 5k entries). Each `Entry` now carries a `#[serde(skip)]` cached query computed once at load/visit time. JSON on disk and the `history_list` IPC response are byte-identical (proven by fingerprint + a dedicated shape test).
   **Debug suggest median: 771,823 us -> 91,786 us (8.4x faster, -88.1%); jitter -92.4%.**
2. **History pane rendering (`src/style.css`, `src/settings.js`).**
   `content-visibility: auto` + `contain-intrinsic-size: auto 49px` on `.hrow`: the engine skips layout/paint for off-screen rows, so a 1,000-row history costs roughly the ~15 visible rows (native rendering virtualization, no JS windowing). `dayLabel` now allocates 1 Date per row instead of 3 + a closure, and computes "today" once per render. `loadHistory` got a stale-response sequence guard, and `history-updated` reloads are debounced 150 ms (a burst of page loads -- or the echo of an optimistic row removal -- no longer triggers one full 1,000-row rebuild per event).
3. **Lock contention + persistence correctness (`data.rs`, `main.rs`).**
   `add_history`/`remove`/`clear` now clone a snapshot and **drop the history mutex before JSON serialization**, so typing in the address bar is never blocked by encoding 5,000 entries. Temp files get an atomic sequence number (`history.json.<ms>.<seq>.tmp`, `session.<ms>.<seq>.tmp`): two saves in the same millisecond can no longer share one `.tmp` path and corrupt the rename. `delete_session` uses `persist_sync` so the async writer can no longer resurrect the file after deletion (real race in Round 2 code).

### Before & After (unchanged `measure.ps1`, 5 runs each, debug profile)

| Metric | Before (Round 2 state) | After (Round 3) | Change |
|---|---|---|---|
| Suggest 100 queries / 5k entries -- median | 771,823 us | **91,786 us** | **-88.1% (8.4x)** |
| Suggest min / max | 680,660 / 1,016,868 us | 77,681 / 103,208 us | -- |
| Suggest spread (max-min) | 336,208 us | 25,527 us | **-92.4% jitter** |
| History 50 filters / 5k entries -- median | 226,551 us | 229,436 us | +1.3% (flat, within noise) |
| History spread | 135,401 us | 21,537 us | **-84.1% jitter** |
| Frontend payload total | 55,487 B | 56,839 B | +1,352 B (icon caches, guards, CSS) |
| Unit tests | 8 passed | **11 passed** (3 new) | +3 regression tests |
| Clippy | -- | 0 errors, 9 pre-existing warnings, **0 new** | clean |
| Release build / release bench | -- | **not measured** (user declined local builds) | see section 7 |

Note: the Nov 17 Round 2 report quoted a suggest median of 397,060 us for the *same* pre-Round-3 code; today's baseline for that code was 771,823 us. Machine load/thermals differ between days -- only same-session before/after pairs are comparable.

---

## 2. 31-Point Performance Scorecard

| # | Item | Verdict | Evidence | Impact |
|---|---|---|---|---|
| 1 | Skeleton loading screens | **Pass** | Chrome UI loads instantly from embedded local assets; no blank states. | L |
| 2 | Optimistic UI with rollback | **Fixed** | Tab close + history row removal optimistic (R2); R3 debounce stops the event echo from rebuilding the list right after. | M |
| 3 | Stream AI responses | **N/A** | No AI/LLM integration. | -- |
| 4 | WebP/AVIF images | **Pass** | UI is SVG-only; no raster assets in app chrome. | L |
| 5 | Lazy below-fold images | **Pass** | No `<img>` in app UI. | L |
| 6 | Unused dependencies | **Pass** | Every Cargo dep used (`tokio` sync/time in `live.rs`, `zip`/`sha2`/`base64` in `store.rs`, `reqwest` CRX, plugins in `main.rs`); vanilla JS frontend. | L |
| 7 | Split JS by page | **Pass** | `index.html`->`app.js`, `settings.html`->`settings.js`, `extensions.html`->`ext.js`; no shared bundle. | M |
| 8 | Defer third-party scripts | **Pass** | Zero third-party scripts/trackers; CSP `default-src 'self'`. | L |
| 9 | Self-host fonts | **Pass** | Windows system stack (Segoe UI Variable / Cascadia Mono); no web fonts. | L |
| 10 | SSR/SSG | **Pass/N/A** | Desktop: assets embedded in binary, served via Tauri local protocol. | L |
| 11 | Prevent unnecessary re-renders | **Fixed** | In-place tab updates + `.sel` toggle (R2); R3: `syncMax` skips DOM writes when maximize state unchanged (`src/app.js`). | H |
| 12 | Virtualize lists >100 rows | **Fixed** | R3: `content-visibility: auto` on `.hrow` (`src/style.css`) = engine-native off-screen skipping; DocumentFragment batch insert; 1,000-item hard cap. Full JS windowing remains an optional decision (section 5). | H |
| 13 | Debounce inputs + cancel stale | **Fixed** | Suggest 150 ms + `suggestSeq` (R2); history search 200 ms; R3: `historySeq` stale guard + 150 ms `history-updated` coalescing (`src/settings.js`). | M |
| 14 | Prevent duplicate calls | **Fixed** | Install button+input disabled while pending (`ext.js`, `app.js`); stale IPC answers dropped by sequence guards. | M |
| 15 | Paginate / bound queries | **Pass** | `history()` caps 1,000; `suggest()` limit 6; `MAX_HISTORY` 5,000 with drain. | M |
| 16 | Parallel independent requests | **Fixed** | `Promise.all` startup in `app.js`/`settings.js` (R1/R2); sequential session-tab restore kept deliberately (parallel webview creation risky). | H |
| 17 | Timeouts on external calls | **Fixed** | `reqwest` 30 s request / 10 s connect (`store.rs:20-26`); WebView2 COM calls 15 s (`live.rs:48`). | M |
| 18 | Brotli/Gzip | **N/A** | No HTTP server; local webview protocol. | -- |
| 19 | CDN for static files | **N/A** | Desktop binary embeds assets. | -- |
| 20 | Browser caching headers | **Pass/N/A** | Assets served from the Tauri bundle, not HTTP. | L |
| 21 | Background slow I/O | **Fixed** | Disk writes on background threads (R1/R2); R3: serialization moved outside the history mutex in add/remove/clear. | H |
| 22 | Fix N+1 queries/locks | **Fixed** | `store.rs::list` single mutex lock (R1); R3: per-keystroke URL re-parsing eliminated (parse once per entry). | M |
| 23 | Indexes / loop optimization | **Fixed** | No DB; R3 `Entry.sq` cache is the equivalent precomputed index for the hot suggest loop; fast-reject filters kept. | H |
| 24 | Select only needed fields | **Pass** | `Suggestion` = url/title/query; `Entry` JSON unchanged (`sq` is `#[serde(skip)]`, shape test added). | L |
| 25 | Connection pooling / client reuse | **Fixed** | `LazyLock<reqwest::Client>` (`store.rs:20`). | M |
| 26 | Cache hot data | **Pass** | Settings/history in memory (`data::Data`); R3 adds per-entry query cache. | M |
| 27 | App/DB region proximity | **N/A** | Local desktop app. | -- |
| 28 | Load balancing | **N/A** | Desktop client. | -- |
| 29 | OpenTelemetry tracing | **Recommend** | Optional `tracing` spans around IPC/CRX install; no vendor needed. | L |
| 30 | Real-user perf monitoring | **Recommend** | Optional local-only timing log (privacy-safe); no RUM vendor. | L |
| 31 | Slow-device/network testing | **Pass** | Benched on a low-end i5-8265U (real slow hardware); local IPC has no network to throttle; external CRX fetch bounded by timeouts. Lighthouse not applicable (section 7). | L |

---

## 3. Changes Made (Round 3)

### `src-tauri/src/data.rs`
- `Entry` gains `#[serde(skip)] sq: Option<String>` -- `search_query(url)` computed once in `Data::load` and `add_history`; `suggest()` reads the cache instead of re-parsing URLs per keystroke. Persisted JSON and IPC shape unchanged.
- `suggest()`: two dedup sets (`seen_urls`, `seen_queries`) replace `format!("q:{...}")` keys; lowercased query moved into the set (no extra clone); match order/semantics preserved.
- `history()`: output entries built field-by-field with `sq: None` (avoids cloning the cached String per row); filter keeps std `to_lowercase` (see section 7 -- buffer-reuse variant measured slower and was reverted).
- `encode()`: hex lookup table instead of `format!("%{b:02X}")` per byte (identical uppercase output; new test).
- `add_history` / `remove` / `clear`: snapshot clone -> `drop(mutex)` -> `save()`; JSON encoding no longer holds the history lock.
- `save()`: atomic `SEQ` counter -> unique `{name}.{ms}.{seq}.tmp` names (same-millisecond collision race removed).
- New tests: `test_entry_json_shape_unchanged`, `test_sq_cached_on_add`, `test_encode_percent_hex`. Benchmark workload unchanged (setup now computes `sq`, untimed).

### `src-tauri/src/main.rs`
- `persist()`: same atomic-seq unique temp names for `session.*.tmp`.
- `delete_session`: `persist_sync` instead of `persist` -- the async writer could previously land *after* `remove_file` and resurrect `session.json`; now deterministic write-then-delete.

### `src/app.js`
- `MAX_ICON_NODE` / `RESTORE_ICON_NODE` pre-parsed; `syncMax()` keeps `lastMax` and skips all DOM writes when the maximize state is unchanged (resize ticks no longer re-parse SVG).

### `src/settings.js`
- `dayLabel(ms, todayStart)`: one `Date` + `setHours(0,0,0,0)` per row instead of 3 Dates + closure; "today" computed once per render. Same local-midnight/DST semantics.
- `loadHistory()`: `historySeq` guard drops out-of-order IPC answers.
- `history-updated` listener: 150 ms debounce (coalesces bursts; visibility check kept).

### `src/style.css`
- `.hrow { content-visibility: auto; contain-intrinsic-size: auto 49px; }` -- off-screen history rows skip layout/paint; `auto` remembers real sizes after first paint, keeping the scrollbar stable (49px = 1px border + 12px padding + 36px text).

---

## 4. Fingerprint Comparison (behavior unchanged)

Generated by the unchanged `bench_and_fingerprint` test before and after Round 3, identical inputs:

| Output | Before SHA-256 | After SHA-256 | Status |
|---|---|---|---|
| `SEARCH_URL` | `1aa2709c990e896cf2c9b3b8d856da1ab73a48d98e49d1583d3e623854c4fcc5` | `1aa2709c990e896cf2c9b3b8d856da1ab73a48d98e49d1583d3e623854c4fcc5` | **MATCH** |
| `SUGGEST_RUST` | `afed46928a13b3c7c297cc7effa7f1166863c10c6402f5c743a106b49128e96a` | `afed46928a13b3c7c297cc7effa7f1166863c10c6402f5c743a106b49128e96a` | **MATCH** |
| `HISTORY_CRATES` | `7c1ef793cf9b7935c1ad8d3517075c2a594fe20473834d6bbadb0d95542fb4b3` | `7c1ef793cf9b7935c1ad8d3517075c2a594fe20473834d6bbadb0d95542fb4b3` | **MATCH** |
| `SETTINGS` | `68c5436df6803e031f1e0532c812a17f2c3ad383b3a4a2b5652bf274c351bade` | `68c5436df6803e031f1e0532c812a17f2c3ad383b3a4a2b5652bf274c351bade` | **MATCH** |

All four also match the Round 1/Round 2 values. 11/11 unit tests pass; clippy: no new warnings.

---

## 5. Items Needing Your Decision

| Item | Options | Rough cost / tradeoff |
|---|---|---|
| **Commit / merge perf work** | Rounds 2+3 are uncommitted on `perf-optimize-round3`. A: review then commit/merge to main. B: keep iterating. | Free; nothing is committed until you say so. |
| **History storage: JSON vs SQLite** | A: keep JSON + background saves (fine <=5k entries). B: `rusqlite` + FTS5 for 100k+ entries, indexed search, less RAM. | B: ~1-2 days dev, adds native SQLite build dep; changes persistence format (migration needed). |
| **Full virtual scrolling** | A: keep R3 `content-visibility` (no JS, already skips off-screen work). B: JS windowing (~15 DOM rows total). | B: ~50-100 lines JS; only worth it if DOM node count itself is a problem on low-RAM machines. |
| **Session file when "Save session" is off** | `exit_app` still writes `session.json` (window bounds) even with the setting off; `settings.js` deletes it on the next settings change. A: keep. B: skip persist entirely when `save_session=false` (stronger privacy). | B is a small behavior change -> needs your approval; ~5 lines. |
| **Telemetry** | A: none (best privacy). B: local-only `tracing` file log. | B: free, no data leaves the device. |

---

## 6. Remaining Recommendations (priority order)

1. **Review + commit/merge** `perf-optimize-round3` (and the carried-over Round 2 changes) -- your call.
2. **Evaluate full LTO (`lto = true`) in CI**, not locally: smaller/faster binary vs longer compile. `build.yml` already builds release on `windows-latest`.
3. **Startup cleanup of stray `*.tmp` files** in AppData after a crash (unique temp names mean crash leftovers no longer overwrite each other, but they can accumulate). Small, optional.
4. **SQLite migration** if history should grow past 5,000 entries (decision above).
5. **Optional local tracing** for IPC/extension-install timings (items 29/30).

---

## 7. What I Could Not Measure or Got Wrong

1. **Release build and release benchmarks: not measured.** You asked not to run builds on your machine ("ko chay build tren may t"); the in-flight `cargo build --release` was stopped and no release exe exists. Round 2's release numbers (45,911 us suggest / 38,902 us history) are historical and were **not** re-verified for Round 3. All Round 3 numbers above are the debug profile, which is what `measure.ps1` runs. If you want release verification, add a bench step to `build.yml` (CI) rather than building locally.
2. **First lowercase attempt was wrong and was reverted.** A buffer-reuse `lower_into` (ASCII fast path) looked allocation-free but measured ~2x *slower* than std `to_lowercase` in debug (199,694 us vs 102,486 us per 100k ops; `to_ascii_lowercase` and table-push variants were 4-6x slower). History median had regressed 226,551 -> 672,709 us; after reverting to std and building output entries without the cached String, history is flat (+1.3%, inside the baseline's 135,401 us spread). Lesson: std's ASCII path is a tight single pass; per-byte `make_ascii_lowercase`/`push` loops are not inlined in debug.
3. **Lighthouse / web-vitals: not applicable.** Tauri serves UI over a local webview protocol, not HTTP; LCP/TBT/transfer-size have no meaning here. No browser-driven UI run of the desktop app happened this session -- UI-level effects (content-visibility, debounce) are verified by code review + CSS semantics, not by interacting with the running app.
4. **`content-visibility` scrollbar estimate:** 49px row height is computed from CSS; `contain-intrinsic-size: auto` self-corrects after first paint, but total scroll height can differ by a few pixels until rows have rendered once. Find-in-page and focus-scroll are supported in WebView2's Chromium (>=130).
5. **Small intentional timing changes:** `history-updated` list refresh is now delayed up to 150 ms (debounce); `delete_session` does one synchronous small write before deleting (rare user action, fixes the resurrection race).
6. **Cross-day numbers are not comparable:** same code measured 397,060 us (Nov 17) vs 771,823 us (Nov 19 baseline) -- machine state differs. Only the same-session before/after pair in section 1 is a valid comparison.
7. **GUI behavior not exercised end-to-end:** correctness rests on the 4 SHA-256 fingerprints, 11 unit tests (including the JSON-shape test), and clippy -- not on a manual run of the browser.
