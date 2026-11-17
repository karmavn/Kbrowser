//! Settings and browsing history, stored as JSON in the app data directory.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
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
}

pub struct Data {
    dir: Option<PathBuf>,
    settings: Mutex<Settings>,
    history: Mutex<Vec<Entry>>,
}

/// Percent-encodes search terms for a query string (spaces become `+`).
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
    host.starts_with("www.google.") || host.starts_with("google.")
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

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Data {
    pub fn load(dir: Option<PathBuf>) -> Self {
        let read = |name: &str| {
            dir.as_ref()
                .and_then(|d| fs::read_to_string(d.join(name)).ok())
        };
        let settings: Settings = read("settings.json")
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let history: Vec<Entry> = read("history.json")
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            dir,
            settings: Mutex::new(settings),
            history: Mutex::new(history),
        }
    }

    fn save<T: Serialize>(&self, name: &str, value: &T) {
        let Some(dir) = &self.dir else { return };
        let _ = fs::create_dir_all(dir);
        let Ok(text) = serde_json::to_vec(value) else {
            return;
        };
        let dir = dir.clone();
        let name = name.to_string();
        std::thread::spawn(move || {
            let tmp = dir.join(format!("{name}.{}.tmp", now()));
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, dir.join(name));
            }
        });
    }

    // --- Settings -------------------------------------------------------------

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Validates, stores and returns the settings actually saved.
    pub fn set_settings(&self, mut s: Settings) -> Settings {
        s.home = s.home.trim().to_string();
        if s.home.is_empty() {
            s.home = DEFAULT_HOME.into();
        }
        if !matches!(s.theme.as_str(), "light" | "dark") {
            s.theme = "system".into();
        }
        // Keep only well-formed custom engines: a name, an http(s) URL and a %s placeholder.
        s.custom_engines.retain(|e| {
            !e.id.trim().is_empty()
                && !e.name.trim().is_empty()
                && (e.url.starts_with("http://") || e.url.starts_with("https://"))
                && e.url.contains("%s")
        });
        let known = matches!(
            s.search.as_str(),
            "duckduckgo" | "google" | "bing" | "brave" | "ecosia"
        ) || s.custom_engines.iter().any(|e| e.id == s.search);
        if !known {
            s.search = "duckduckgo".into();
        }
        *self.settings.lock().unwrap() = s.clone();
        self.save("settings.json", &s);
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
        let Ok(u) = Url::parse(url) else { return false };
        if !matches!(u.scheme(), "http" | "https") || u.host_str() == Some("tauri.localhost") {
            return false;
        }
        let (t, title) = (now(), title.trim());
        let mut h = self.history.lock().unwrap();
        let dup =
            matches!(h.last(), Some(l) if l.url == url && t.saturating_sub(l.time) < DEDUPE_MS);
        if dup {
            if !title.is_empty() {
                if let Some(l) = h.last_mut() {
                    l.title = title.to_string();
                }
            }
        } else {
            h.push(Entry {
                url: url.to_string(),
                title: title.to_string(),
                time: t,
            });
            let extra = h.len().saturating_sub(MAX_HISTORY);
            h.drain(..extra);
        }
        self.save("history.json", &*h);
        true
    }

    /// Newest first, optionally filtered by title or URL.
    pub fn history(&self, query: &str) -> Vec<Entry> {
        let q = query.trim().to_lowercase();
        let h = self.history.lock().unwrap();
        if q.is_empty() {
            return h.iter().rev().take(1000).cloned().collect();
        }
        h.iter()
            .rev()
            .filter(|e| {
                e.title.to_lowercase().contains(&q) || e.url.to_lowercase().contains(&q)
            })
            .take(1000)
            .cloned()
            .collect()
    }

    /// History matches for the address bar: newest first, one row per page or per search.
    pub fn suggest(&self, query: &str, limit: usize) -> Vec<Suggestion> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        let mut seen = std::collections::HashSet::new();
        let mut out = vec![];
        let history = self.history.lock().unwrap();

        for e in history.iter().rev() {
            let sq = search_query(&e.url);
            match sq {
                Some(ref t) => {
                    let t_lower = t.to_lowercase();
                    if !t_lower.contains(&q) {
                        continue;
                    }
                    let key = format!("q:{t_lower}");
                    if !seen.insert(key) {
                        continue;
                    }
                    out.push(Suggestion {
                        url: e.url.clone(),
                        title: e.title.clone(),
                        query: sq,
                    });
                }
                None => {
                    if seen.contains(&e.url) {
                        continue;
                    }
                    let title_lower = e.title.to_lowercase();
                    let url_lower = e.url.to_lowercase();
                    let matches = if title_lower.contains(&q) || url_lower.contains(&q) {
                        true
                    } else if q.contains(' ') {
                        format!("{title_lower} {url_lower}").contains(&q)
                    } else {
                        false
                    };

                    if !matches || !seen.insert(e.url.clone()) {
                        continue;
                    }

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

    pub fn remove(&self, time: u64, url: &str) {
        let mut h = self.history.lock().unwrap();
        h.retain(|e| !(e.time == time && e.url == url));
        self.save("history.json", &*h);
    }

    /// range: "hour" | "day" | "week" | anything else = everything.
    pub fn clear(&self, range: &str) {
        let span: u64 = match range {
            "hour" => 3_600_000,
            "day" => 86_400_000,
            "week" => 604_800_000,
            _ => 0,
        };
        let mut h = self.history.lock().unwrap();
        if span == 0 {
            h.clear();
        } else {
            let cutoff = now().saturating_sub(span);
            h.retain(|e| e.time < cutoff);
        }
        self.save("history.json", &*h);
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
        let mut s = Settings::default();
        s.theme = "invalid_theme".into();
        s.home = "   ".into();
        s.search = "nonexistent".into();
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

        let mut s = Settings::default();
        s.search = "google".into();
        data.set_settings(s);
        let u2 = data.search_url("rust async");
        assert_eq!(u2, "https://www.google.com/search?q=rust+async");

        let mut s = Settings::default();
        s.custom_engines.push(Engine {
            id: "yt".into(),
            name: "YouTube".into(),
            url: "https://www.youtube.com/results?search_query=%s".into(),
        });
        s.search = "yt".into();
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
                    },
                    1 => Entry {
                        url: format!("https://www.google.com/search?q=rust+language+topic+{}", i % 30),
                        title: format!("Google Search - rust topic {}", i % 30),
                        time: 1700000000000 + i as u64 * 1000,
                    },
                    2 => Entry {
                        url: format!("https://crates.io/crates/package-{}", i % 200),
                        title: format!("package-{} - crates.io: Rust Package Registry", i % 200),
                        time: 1700000000000 + i as u64 * 1000,
                    },
                    3 => Entry {
                        url: format!("https://github.com/developer/repo-{}", i % 100),
                        title: format!("developer/repo-{} on GitHub", i % 100),
                        time: 1700000000000 + i as u64 * 1000,
                    },
                    _ => Entry {
                        url: format!("https://docs.rs/library-{}/0.1.0/library/index.html", i % 80),
                        title: format!("library-{} - Rust docs", i % 80),
                        time: 1700000000000 + i as u64 * 1000,
                    },
                };
                h.push(entry);
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


