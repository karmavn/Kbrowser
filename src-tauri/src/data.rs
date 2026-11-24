//! Settings and browsing history, stored as JSON in the app data directory.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::Url;

const MAX_HISTORY: usize = 5000;
/// Re-visits of the same URL within this window update the last entry instead of adding one.
const DEDUPE_MS: u64 = 60_000;
const DEFAULT_HOME: &str = "about:new";

/// A user-defined search engine. `url` contains `%s` where the search terms go.
#[derive(Clone, Serialize, Deserialize)]
pub struct Engine {
    pub id: String,
    pub name: String,
    pub url: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "system" | "light" | "dark"
    pub theme: String,
    /// "duckduckgo" | "google" | "bing" | "brave" | "ecosia", or the id of a custom engine.
    pub search: String,
    pub home: String,
    pub save_history: bool,
    pub show_suggestions: bool,
    pub save_session: bool,
    pub custom_engines: Vec<Engine>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            search: "duckduckgo".into(),
            home: DEFAULT_HOME.into(),
            save_history: true,
            show_suggestions: true,
            save_session: false,
            custom_engines: vec![],
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub url: String,
    pub title: String,
    /// Milliseconds since the Unix epoch.
    pub time: u64,
    /// Cached `search_query(url)`, computed once when the entry is created or loaded
    /// instead of on every address-bar keystroke. Skipped by serde, so the persisted
    /// JSON and the `history_list` IPC response keep exactly the same shape.
    #[serde(skip)]
    sq: Option<String>,
}

/// Minimal serializable projection of `Entry` for background disk persistence,
/// avoiding cloning `sq` under the history mutex.
#[derive(Serialize)]
struct DiskEntry {
    url: String,
    title: String,
    time: u64,
}

fn snapshot_disk(entries: &[Entry]) -> Vec<DiskEntry> {
    entries
        .iter()
        .map(|e| DiskEntry {
            url: e.url.clone(),
            title: e.title.clone(),
            time: e.time,
        })
        .collect()
}

enum WriteOp {
    WriteJson {
        path: PathBuf,
        serialize: Box<dyn FnOnce() -> Option<Vec<u8>> + Send>,
    },
    Delete {
        path: PathBuf,
    },
    Flush(mpsc::SyncSender<()>),
}

enum FileAction {
    Write(Box<dyn FnOnce() -> Option<Vec<u8>> + Send>),
    Delete,
}

pub(crate) struct DiskWriter {
    tx: Option<mpsc::Sender<WriteOp>>,
}

impl DiskWriter {
    fn new(enabled: bool) -> Self {
        if !enabled {
            return Self { tx: None };
        }
        let (tx, rx) = mpsc::channel::<WriteOp>();
        std::thread::spawn(move || {
            let mut seq: u64 = 0;
            while let Ok(first) = rx.recv() {
                let mut pending = vec![first];
                while let Ok(next) = rx.try_recv() {
                    let is_flush = matches!(next, WriteOp::Flush(_));
                    pending.push(next);
                    if is_flush {
                        break;
                    }
                }

                let mut batch: Vec<(PathBuf, FileAction)> = Vec::new();
                let mut flushes: Vec<mpsc::SyncSender<()>> = Vec::new();
                for op in pending {
                    match op {
                        WriteOp::WriteJson { path, serialize } => {
                            if let Some(pos) = batch.iter().position(|(p, _)| p == &path) {
                                batch[pos].1 = FileAction::Write(serialize);
                            } else {
                                batch.push((path, FileAction::Write(serialize)));
                            }
                        }
                        WriteOp::Delete { path } => {
                            if let Some(pos) = batch.iter().position(|(p, _)| p == &path) {
                                batch[pos].1 = FileAction::Delete;
                            } else {
                                batch.push((path, FileAction::Delete));
                            }
                        }
                        WriteOp::Flush(ack) => flushes.push(ack),
                    }
                }

                for (path, action) in batch {
                    match action {
                        FileAction::Write(serialize) => {
                            if let Some(bytes) = serialize() {
                                write_atomic(&path, &bytes, &mut seq);
                            }
                        }
                        FileAction::Delete => {
                            let _ = fs::remove_file(&path);
                        }
                    }
                }

                for ack in flushes {
                    let _ = ack.send(());
                }
            }
        });
        Self { tx: Some(tx) }
    }

    pub(crate) fn write_json<T: Serialize + Send + 'static>(&self, path: PathBuf, value: T) {
        let Some(tx) = &self.tx else { return };
        let _ = tx.send(WriteOp::WriteJson {
            path,
            serialize: Box::new(move || serde_json::to_vec(&value).ok()),
        });
    }

    pub(crate) fn delete_file(&self, path: PathBuf) {
        let Some(tx) = &self.tx else { return };
        let _ = tx.send(WriteOp::Delete { path });
    }

    pub(crate) fn flush(&self) {
        let Some(tx) = &self.tx else { return };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if tx.send(WriteOp::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(5));
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8], seq: &mut u64) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    *seq = seq.wrapping_add(1);
    let fname = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("data.json");
    let tmp = path.with_file_name(format!("{fname}.{}.{}.tmp", now(), *seq));
    if fs::write(&tmp, bytes).is_err() {
        let _ = fs::remove_file(&tmp);
        return;
    }
    for attempt in 0..3 {
        if fs::rename(&tmp, path).is_ok() {
            return;
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let _ = fs::remove_file(&tmp);
}

pub(crate) fn backup_corrupt(path: &Path) {
    if path.exists() {
        let fname = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("data.json");
        let _ = fs::rename(path, path.with_file_name(format!("{fname}.corrupt")));
    }
}

pub struct Data {
    dir: Option<PathBuf>,
    writer: DiskWriter,
    settings: Mutex<Settings>,
    history: Mutex<Vec<Entry>>,
}

/// Percent-encodes search terms for a query string (spaces become `+`).
fn encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 15) as usize] as char);
            }
        }
    }
    out
}

fn is_google_tld(tld: &str) -> bool {
    let mut parts = tld.split('.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(a), None, None) => {
            (2..=3).contains(&a.len()) && a.bytes().all(|b| b.is_ascii_lowercase())
        }
        (Some(a), Some(b), None) => {
            matches!(a, "co" | "com" | "ne" | "off")
                && (2..=3).contains(&b.len())
                && b.bytes().all(|c| c.is_ascii_lowercase())
        }
        _ => false,
    }
}

/// Fast check for well-known search engine host names without heap allocations.
fn is_known_search_host(host: &str) -> bool {
    const KNOWN: &[&str] = &["duckduckgo.com", "bing.com", "brave.com", "ecosia.org"];
    for &d in KNOWN {
        if host == d
            || (host.ends_with(d) && host.as_bytes().get(host.len() - d.len() - 1) == Some(&b'.'))
        {
            return true;
        }
    }
    let g = host.strip_prefix("www.").unwrap_or(host);
    g.strip_prefix("google.").is_some_and(is_google_tld)
}

/// The search terms if `url` is a results page of a well-known search engine.
fn search_query(url: &str) -> Option<String> {
    // Fast rejection checks on raw string before expensive Url::parse
    if !url.contains("q=") {
        return None;
    }
    if !url.contains("duckduckgo.com")
        && !url.contains("google.")
        && !url.contains("bing.com")
        && !url.contains("brave.com")
        && !url.contains("ecosia.org")
    {
        return None;
    }
    let u = Url::parse(url).ok()?;
    let host = u.host_str()?;
    if !is_known_search_host(host) {
        return None;
    }
    u.query_pairs()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

/// One address-bar suggestion drawn from the history.
#[derive(Serialize)]
pub struct Suggestion {
    pub url: String,
    pub title: String,
    /// Set when the page is a search results page: the search terms.
    pub query: Option<String>,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Data {
    pub fn load(dir: Option<PathBuf>) -> Self {
        // Clean up any stray *.tmp files left in AppData after an abrupt crash.
        if let Some(d) = &dir {
            for entry in fs::read_dir(d).into_iter().flatten().flatten() {
                let p = entry.path();
                if p.is_file()
                    && let Some(name) = p.file_name().and_then(|n| n.to_str())
                    && name.ends_with(".tmp")
                    && (name.starts_with("history.json.")
                        || name.starts_with("settings.json.")
                        || name.starts_with("session."))
                {
                    let _ = fs::remove_file(&p);
                }
            }
        }
        let load_json = |name: &str| -> Option<String> {
            dir.as_ref().and_then(|d| fs::read_to_string(d.join(name)).ok())
        };
        let settings: Settings = match load_json("settings.json") {
            Some(t) if !t.trim().is_empty() => match serde_json::from_str(&t) {
                Ok(v) => v,
                Err(_) => {
                    if let Some(d) = &dir {
                        backup_corrupt(&d.join("settings.json"));
                    }
                    Settings::default()
                }
            },
            _ => Settings::default(),
        };
        let mut history: Vec<Entry> = match load_json("history.json") {
            Some(t) if !t.trim().is_empty() => match serde_json::from_str(&t) {
                Ok(v) => v,
                Err(_) => {
                    if let Some(d) = &dir {
                        backup_corrupt(&d.join("history.json"));
                    }
                    vec![]
                }
            },
            _ => vec![],
        };
        // Parse each URL once at load time; suggest() then never re-parses.
        for e in &mut history {
            e.sq = search_query(&e.url);
        }
        let writer = DiskWriter::new(dir.is_some());
        Self {
            dir,
            writer,
            settings: Mutex::new(settings),
            history: Mutex::new(history),
        }
    }

    pub(crate) fn write_json<T: Serialize + Send + 'static>(&self, path: PathBuf, value: T) {
        self.writer.write_json(path, value);
    }

    pub(crate) fn delete_file(&self, path: PathBuf) {
        self.writer.delete_file(path);
    }

    pub(crate) fn flush(&self) {
        self.writer.flush();
    }

    fn save<T: Serialize + Send + 'static>(&self, name: &str, value: T) {
        let Some(dir) = &self.dir else { return };
        self.writer.write_json(dir.join(name), value);
    }

    // --- Settings -------------------------------------------------------------

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Validates, stores and returns the settings actually saved.
    pub fn set_settings(&self, mut s: Settings) -> Settings {
        const BUILTIN: &[&str] = &["duckduckgo", "google", "bing", "brave", "ecosia"];
        s.home = s.home.trim().to_string();
        if s.home.is_empty() {
            s.home = DEFAULT_HOME.into();
        }
        if !matches!(s.theme.as_str(), "light" | "dark") {
            s.theme = "system".into();
        }
        // Keep only well-formed custom engines: non-builtin ID, name, valid http(s) URL (non-tauri.localhost), %s.
        s.custom_engines.retain(|e| {
            let id = e.id.trim();
            !id.is_empty()
                && !BUILTIN.contains(&id)
                && !e.name.trim().is_empty()
                && e.url.contains("%s")
                && Url::parse(&e.url.replace("%s", "test")).is_ok_and(|u| {
                    matches!(u.scheme(), "http" | "https")
                        && u.host_str().is_some_and(|h| h != "tauri.localhost")
                })
        });
        let known = BUILTIN.contains(&s.search.as_str())
            || s.custom_engines.iter().any(|e| e.id == s.search);
        if !known {
            s.search = "duckduckgo".into();
        }
        *self.settings.lock().unwrap() = s.clone();
        self.save("settings.json", s.clone());
        s
    }

    /// URL of a web search for `query` on the chosen search engine.
    pub fn search_url(&self, query: &str) -> String {
        let (engine, custom) = {
            let s = self.settings.lock().unwrap();
            (
                s.search.clone(),
                s.custom_engines.iter().find(|e| e.id == s.search).cloned(),
            )
        };
        if let Some(c) = custom {
            return c.url.replace("%s", &encode(query));
        }
        let base = match engine.as_str() {
            "google" => "https://www.google.com/search",
            "bing" => "https://www.bing.com/search",
            "brave" => "https://search.brave.com/search",
            "ecosia" => "https://www.ecosia.org/search",
            _ => "https://duckduckgo.com/",
        };
        Url::parse_with_params(base, &[("q", query)])
            .map(|u| u.to_string())
            .unwrap_or_else(|_| base.to_string())
    }

    // --- History --------------------------------------------------------------

    /// Records a visit. Returns true if the history changed.
    pub fn add_history(&self, url: &str, title: &str) -> bool {
        if !self.settings.lock().unwrap().save_history {
            return false;
        }
        if url.len() > 4096 {
            return false;
        }
        let Ok(u) = Url::parse(url) else { return false };
        if !matches!(u.scheme(), "http" | "https") || u.host_str() == Some("tauri.localhost") {
            return false;
        }
        let t = now();
        let title_trimmed = title.trim();
        let title: String = if title_trimmed.chars().count() > 512 {
            title_trimmed.chars().take(512).collect()
        } else {
            title_trimmed.to_string()
        };
        let mut h = self.history.lock().unwrap();
        let dup =
            matches!(h.last(), Some(l) if l.url == url && t.saturating_sub(l.time) < DEDUPE_MS);
        if dup {
            if title.is_empty() || h.last().is_some_and(|l| l.title == title) {
                return false;
            }
            if let Some(l) = h.last_mut() {
                l.title = title;
            }
        } else {
            h.push(Entry {
                url: url.to_string(),
                title,
                time: t,
                sq: search_query(url),
            });
            let extra = h.len().saturating_sub(MAX_HISTORY);
            h.drain(..extra);
        }
        // Serialize on the background writer thread so suggest()/history() and the UI
        // thread are never stalled by encoding up to 5,000 entries on every page visit.
        let snapshot = snapshot_disk(&h);
        drop(h);
        self.save("history.json", snapshot);
        true
    }

    /// Newest first, optionally filtered by title or URL.
    pub fn history(&self, query: &str) -> Vec<Entry> {
        let q = query.trim().to_lowercase();
        let h = self.history.lock().unwrap();
        // Output entries are built field-by-field: the cached query is serde-skipped
        // anyway, and skipping it here avoids cloning its String for every row.
        let out = |e: &Entry| Entry {
            url: e.url.clone(),
            title: e.title.clone(),
            time: e.time,
            sq: None,
        };
        if q.is_empty() {
            return h.iter().rev().take(1000).map(out).collect();
        }
        // std's to_lowercase has a tight ASCII fast path; measured faster than
        // multi-field caches and SoA index scans in debug.
        h.iter()
            .rev()
            .filter(|e| e.title.to_lowercase().contains(&q) || e.url.to_lowercase().contains(&q))
            .take(1000)
            .map(out)
            .collect()
    }

    /// History matches for the address bar: newest first, one row per page or per search.
    pub fn suggest(&self, query: &str, limit: usize) -> Vec<Suggestion> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        // Lock before borrowing &e.url into seen_urls so drop order matches borrow lifetime.
        let history = self.history.lock().unwrap();
        let mut seen_urls: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut seen_queries: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out = vec![];

        for e in history.iter().rev() {
            // e.sq was parsed once when the entry was created/loaded: no Url::parse here.
            match e.sq.as_ref() {
                Some(t) => {
                    let t_lower = t.to_lowercase();
                    if !t_lower.contains(&q) {
                        continue;
                    }
                    if seen_queries.contains(t_lower.as_str()) {
                        continue;
                    }
                    seen_queries.insert(t_lower);
                    out.push(Suggestion {
                        url: e.url.clone(),
                        title: e.title.clone(),
                        query: Some(t.clone()),
                    });
                }
                None => {
                    if seen_urls.contains(e.url.as_str()) {
                        continue;
                    }
                    let title_lower = e.title.to_lowercase();
                    let url_lower = e.url.to_lowercase();
                    let matches = title_lower.contains(&q)
                        || url_lower.contains(&q)
                        || (q.contains(' ') && format!("{title_lower} {url_lower}").contains(&q));
                    if !matches {
                        continue;
                    }
                    seen_urls.insert(e.url.as_str());
                    out.push(Suggestion {
                        url: e.url.clone(),
                        title: e.title.clone(),
                        query: None,
                    });
                }
            }
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    /// Up to `limit` distinct recently visited non-search sites, newest first.
    pub fn recent_sites(&self, limit: usize) -> Vec<Entry> {
        let limit = limit.min(24);
        if limit == 0 {
            return vec![];
        }
        let h = self.history.lock().unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(limit);
        for e in h.iter().rev() {
            if e.sq.is_some() {
                continue;
            }
            let Ok(u) = Url::parse(&e.url) else { continue };
            if !matches!(u.scheme(), "http" | "https") {
                continue;
            }
            let Some(raw_host) = u.host_str() else { continue };
            if raw_host == "tauri.localhost" {
                continue;
            }
            let host = raw_host.strip_prefix("www.").unwrap_or(raw_host).to_string();
            if host.is_empty() || !seen.insert(host) {
                continue;
            }
            out.push(Entry {
                url: e.url.clone(),
                title: e.title.clone(),
                time: e.time,
                sq: None,
            });
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    pub fn remove(&self, time: u64, url: &str) {
        let mut h = self.history.lock().unwrap();
        h.retain(|e| !(e.time == time && e.url == url));
        let snapshot = snapshot_disk(&h);
        drop(h);
        self.save("history.json", snapshot);
    }

    /// range: "hour" | "day" | "week" | "all". Unknown values are ignored.
    pub fn clear(&self, range: &str) {
        let span: Option<u64> = match range {
            "hour" => Some(3_600_000),
            "day" => Some(86_400_000),
            "week" => Some(604_800_000),
            "all" => None,
            _ => return,
        };
        let mut h = self.history.lock().unwrap();
        match span {
            None => h.clear(),
            Some(ms) => {
                let cutoff = now().saturating_sub(ms);
                h.retain(|e| e.time < cutoff);
            }
        }
        let snapshot = snapshot_disk(&h);
        drop(h);
        self.save("history.json", snapshot);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_settings_default() {
        let s = Settings::default();
        assert_eq!(s.theme, "system");
        assert_eq!(s.search, "duckduckgo");
        assert_eq!(s.home, "about:new");
        assert!(s.save_history);
        assert!(s.show_suggestions);
        assert!(!s.save_session);
    }

    #[test]
    fn test_settings_validation() {
        let data = Data::load(None);
        let mut s = Settings {
            theme: "invalid_theme".into(),
            home: "   ".into(),
            search: "nonexistent".into(),
            ..Default::default()
        };
        s.custom_engines.push(Engine {
            id: "".into(),
            name: "bad".into(),
            url: "invalid".into(),
        });
        s.custom_engines.push(Engine {
            id: "gh".into(),
            name: "GitHub".into(),
            url: "https://github.com/search?q=%s".into(),
        });

        let saved = data.set_settings(s);
        assert_eq!(saved.theme, "system");
        assert_eq!(saved.home, DEFAULT_HOME);
        assert_eq!(saved.search, "duckduckgo");
        assert_eq!(saved.custom_engines.len(), 1);
        assert_eq!(saved.custom_engines[0].id, "gh");
    }

    #[test]
    fn test_search_url() {
        let data = Data::load(None);
        let u1 = data.search_url("hello world");
        assert_eq!(u1, "https://duckduckgo.com/?q=hello+world");

        let s = Settings {
            search: "google".into(),
            ..Default::default()
        };
        data.set_settings(s);
        let u2 = data.search_url("rust async");
        assert_eq!(u2, "https://www.google.com/search?q=rust+async");

        let s = Settings {
            search: "yt".into(),
            custom_engines: vec![Engine {
                id: "yt".into(),
                name: "YouTube".into(),
                url: "https://www.youtube.com/results?search_query=%s".into(),
            }],
            ..Default::default()
        };
        data.set_settings(s);
        let u3 = data.search_url("tauri v2");
        assert_eq!(u3, "https://www.youtube.com/results?search_query=tauri+v2");
    }

    #[test]
    fn test_search_query_extraction() {
        assert_eq!(
            search_query("https://duckduckgo.com/?q=weather+today"),
            Some("weather today".into())
        );
        assert_eq!(
            search_query("https://www.google.com/search?q=rust+lang&hl=en"),
            Some("rust lang".into())
        );
        assert_eq!(
            search_query("https://www.bing.com/search?q=tauri"),
            Some("tauri".into())
        );
        assert_eq!(
            search_query("https://search.brave.com/search?q=privacy"),
            Some("privacy".into())
        );
        assert_eq!(
            search_query("https://www.ecosia.org/search?q=trees"),
            Some("trees".into())
        );
        assert_eq!(search_query("https://example.com/?q=test"), None);
    }

    #[test]
    fn test_history_dedupe_and_suggest() {
        let data = Data::load(None);
        assert!(data.add_history("https://duckduckgo.com/?q=rust", "DuckDuckGo"));
        // Second visit within dedupe window updates title
        assert!(data.add_history("https://duckduckgo.com/?q=rust", "DuckDuckGo - rust"));
        assert_eq!(data.history("").len(), 1);
        assert_eq!(data.history("")[0].title, "DuckDuckGo - rust");

        data.add_history("https://github.com/tauri-apps/tauri", "GitHub - tauri-apps/tauri");
        data.add_history("https://crates.io/crates/tauri", "tauri - crates.io");

        let suggestions = data.suggest("tauri", 5);
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].url, "https://crates.io/crates/tauri");
        assert_eq!(suggestions[1].url, "https://github.com/tauri-apps/tauri");

        let search_suggestions = data.suggest("rust", 5);
        assert_eq!(search_suggestions.len(), 1);
        assert_eq!(search_suggestions[0].query, Some("rust".into()));
    }

    #[test]
    fn test_entry_json_shape_unchanged() {
        // The cached query must never leak into persisted JSON or IPC responses.
        let e = Entry {
            url: "https://example.com/".into(),
            title: "Example".into(),
            time: 1700000000000,
            sq: Some("ignored".into()),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"url":"https://example.com/","title":"Example","time":1700000000000}"#
        );
        let back: Entry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sq, None);
    }

    #[test]
    fn test_dup_noop_and_title_update() {
        let data = Data::load(None);
        assert!(data.add_history("https://example.com/docs", "Initial"));
        // Empty title or exact same title within dedupe window changes nothing and skips save/emit.
        assert!(!data.add_history("https://example.com/docs", ""));
        assert_eq!(data.history("")[0].title, "Initial");
        assert!(!data.add_history("https://example.com/docs", "Initial"));
        // New title within dedupe window updates title and stays searchable.
        assert!(data.add_history("https://example.com/docs", "Updated Guide"));
        let list = data.history("UPDATED");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "Updated Guide");
        let sugg = data.suggest("guide", 5);
        assert_eq!(sugg.len(), 1);
        assert_eq!(sugg[0].title, "Updated Guide");
    }

    #[test]
    fn test_load_cleans_stray_tmp_files() {
        let dir = std::env::temp_dir().join(format!(
            "kbrowser_tmp_cleanup_test_{}_{}",
            std::process::id(),
            now()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let stray = [
            "history.json.123.0.tmp",
            "settings.json.123.1.tmp",
            "session.123.2.tmp",
        ];
        let keep = ["history.json", "settings.json", "unrelated.tmp"];
        for f in stray.iter().chain(keep.iter()) {
            fs::write(dir.join(f), b"[]").unwrap();
        }

        let _ = Data::load(Some(dir.clone()));

        for f in stray {
            assert!(!dir.join(f).exists(), "stray tmp should be removed: {f}");
        }
        for f in keep {
            assert!(dir.join(f).exists(), "non-matching file should remain: {f}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sq_cached_on_add() {
        let data = Data::load(None);
        data.add_history("https://duckduckgo.com/?q=rust", "DuckDuckGo");
        data.add_history("https://example.com/", "Example");
        {
            let h = data.history.lock().unwrap();
            assert_eq!(h[0].sq.as_deref(), Some("rust"));
            assert_eq!(h[1].sq, None);
        }
        let s = data.suggest("rust", 6);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].query.as_deref(), Some("rust"));
    }

    #[test]
    fn test_encode_percent_hex() {
        let s = Settings {
            search: "x".into(),
            custom_engines: vec![Engine {
                id: "x".into(),
                name: "X".into(),
                url: "https://x.test/?q=%s".into(),
            }],
            ..Default::default()
        };
        let data = Data::load(None);
        data.set_settings(s);
        assert_eq!(data.search_url("a&b c"), "https://x.test/?q=a%26b+c");
        assert_eq!(data.search_url("é"), "https://x.test/?q=%C3%A9");
    }

    #[test]
    fn test_is_known_search_host_rejects_lookalikes() {
        assert!(is_known_search_host("google.com"));
        assert!(is_known_search_host("www.google.co.uk"));
        assert!(is_known_search_host("www.google.com.vn"));
        assert!(is_known_search_host("google.de"));
        assert!(is_known_search_host("duckduckgo.com"));
        assert!(is_known_search_host("search.brave.com"));
        assert!(!is_known_search_host("google.evil.com"));
        assert!(!is_known_search_host("www.google.evil.com"));
        assert!(!is_known_search_host("evilduckduckgo.com"));
        assert!(!is_known_search_host("google.123"));
    }

    #[test]
    fn test_clear_requires_explicit_all() {
        let data = Data::load(None);
        assert!(data.add_history("https://example.com/1", "One"));
        assert_eq!(data.history("").len(), 1);
        data.clear("invalid_range");
        assert_eq!(data.history("").len(), 1);
        data.clear("all");
        assert!(data.history("").is_empty());
    }

    #[test]
    fn test_custom_engine_validation() {
        let data = Data::load(None);
        let s = Settings {
            custom_engines: vec![
                Engine {
                    id: "google".into(),
                    name: "Shadow".into(),
                    url: "https://evil.test/?q=%s".into(),
                },
                Engine {
                    id: "local".into(),
                    name: "Local".into(),
                    url: "http://tauri.localhost/?q=%s".into(),
                },
                Engine {
                    id: "badurl".into(),
                    name: "Bad".into(),
                    url: "https://exa mple.com/?q=%s".into(),
                },
                Engine {
                    id: "ok".into(),
                    name: "Valid".into(),
                    url: "https://example.org/search?q=%s".into(),
                },
            ],
            ..Default::default()
        };
        let saved = data.set_settings(s);
        assert_eq!(saved.custom_engines.len(), 1);
        assert_eq!(saved.custom_engines[0].id, "ok");
    }

    #[test]
    fn test_ordered_writer_no_resurrection() {
        let dir = std::env::temp_dir().join(format!(
            "kbrowser_writer_order_{}_{}",
            std::process::id(),
            now()
        ));
        let _ = fs::remove_dir_all(&dir);
        let data = Data::load(Some(dir.clone()));
        for i in 0..500 {
            data.add_history(&format!("https://example.com/{i}"), &format!("Title {i}"));
        }
        data.clear("all");
        data.flush();

        let disk = fs::read_to_string(dir.join("history.json")).unwrap();
        let entries: Vec<Entry> = serde_json::from_str(&disk).unwrap();
        assert!(entries.is_empty(), "cleared history must not be resurrected");
        let stray_tmp = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray_tmp, "no .tmp files should remain after flush");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_recent_sites() {
        let data = Data::load(None);
        data.add_history("https://duckduckgo.com/?q=rust", "DuckDuckGo");
        for i in 0..8 {
            data.add_history(&format!("https://site{i}.example.org/page"), &format!("Site {i}"));
        }
        data.add_history("https://www.site7.example.org/other", "Site 7 again");
        let recent = data.recent_sites(6);
        assert_eq!(recent.len(), 6);
        assert!(recent[0].url.contains("site7.example.org"));
        assert!(!recent.iter().any(|e| e.url.contains("duckduckgo.com")));
    }

    #[test]
    fn test_history_length_caps() {
        let data = Data::load(None);
        let long_url = format!("https://example.com/{}", "a".repeat(5000));
        assert!(!data.add_history(&long_url, "Too long"));
        let long_title = "Việt Nam 🚀 ".repeat(1000);
        assert!(data.add_history("https://example.com/ok", &long_title));
        let h = data.history("");
        assert_eq!(h.len(), 1);
        assert!(h[0].title.chars().count() <= 512);
    }

    #[test]
    #[ignore]
    fn bench_and_fingerprint() {
        use sha2::{Sha256, Digest};
        use std::time::Instant;

        let data = Data::load(None);

        // Populate realistic 5000 history entries
        {
            let mut h = data.history.lock().unwrap();
            for i in 0..5000 {
                let entry = match i % 5 {
                    0 => Entry {
                        url: format!("https://duckduckgo.com/?q=query+term+{}", i % 50),
                        title: format!("Search results for query term {}", i % 50),
                        time: 1700000000000 + i as u64 * 1000,
                        sq: None,
                    },
                    1 => Entry {
                        url: format!("https://www.google.com/search?q=rust+language+topic+{}", i % 30),
                        title: format!("Google Search - rust topic {}", i % 30),
                        time: 1700000000000 + i as u64 * 1000,
                        sq: None,
                    },
                    2 => Entry {
                        url: format!("https://crates.io/crates/package-{}", i % 200),
                        title: format!("package-{} - crates.io: Rust Package Registry", i % 200),
                        time: 1700000000000 + i as u64 * 1000,
                        sq: None,
                    },
                    3 => Entry {
                        url: format!("https://github.com/developer/repo-{}", i % 100),
                        title: format!("developer/repo-{} on GitHub", i % 100),
                        time: 1700000000000 + i as u64 * 1000,
                        sq: None,
                    },
                    _ => Entry {
                        url: format!("https://docs.rs/library-{}/0.1.0/library/index.html", i % 80),
                        title: format!("library-{} - Rust docs", i % 80),
                        time: 1700000000000 + i as u64 * 1000,
                        sq: None,
                    },
                };
                h.push(entry);
            }
            for e in h.iter_mut() {
                e.sq = search_query(&e.url);
            }
        }

        // Benchmark suggest across 100 queries
        let query_list = [
            "rust", "query", "google", "crates", "repo", "library", "package", "search", "topic", "index"
        ];

        let start_suggest = Instant::now();
        let mut total_suggestions = 0;
        for _ in 0..10 {
            for q in &query_list {
                let res = data.suggest(q, 6);
                total_suggestions += res.len();
            }
        }
        let elapsed_suggest = start_suggest.elapsed();

        // Benchmark history filter across 50 queries
        let start_history = Instant::now();
        let mut total_history = 0;
        for _ in 0..5 {
            for q in &query_list {
                let res = data.history(q);
                total_history += res.len();
            }
        }
        let elapsed_history = start_history.elapsed();

        // Compute SHA-256 fingerprints
        let fp_search = {
            let res = data.search_url("how to optimize rust");
            let mut hasher = Sha256::new();
            hasher.update(res.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        let fp_suggest = {
            let res = data.suggest("rust", 6);
            let json = serde_json::to_string(&res).unwrap();
            let mut hasher = Sha256::new();
            hasher.update(json.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        let fp_history = {
            let res = data.history("crates");
            let json = serde_json::to_string(&res).unwrap();
            let mut hasher = Sha256::new();
            hasher.update(json.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        let fp_settings = {
            let res = data.settings();
            let json = serde_json::to_string(&res).unwrap();
            let mut hasher = Sha256::new();
            hasher.update(json.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        println!("\n=== BENCHMARK_RESULTS ===");
        println!("SUGGEST_TIME_MICROS: {}", elapsed_suggest.as_micros());
        println!("SUGGEST_COUNT: {}", total_suggestions);
        println!("HISTORY_TIME_MICROS: {}", elapsed_history.as_micros());
        println!("HISTORY_COUNT: {}", total_history);
        println!("FP_SEARCH_URL: {}", fp_search);
        println!("FP_SUGGEST_RUST: {}", fp_suggest);
        println!("FP_HISTORY_CRATES: {}", fp_history);
        println!("FP_SETTINGS: {}", fp_settings);
        println!("=== END_BENCHMARK_RESULTS ===\n");
    }
}


