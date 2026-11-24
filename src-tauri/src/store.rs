//! Chrome Web Store installs: download CRX -> strip header -> unpack into
//! `<app data>/extensions/<id>/`, then load it into the running profile (see live.rs).
//! wry also re-adds every sub-folder of that directory at each startup.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tauri::{AppHandle, Manager};

use crate::{Ext, Loaded, live};

const MAX_CRX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 2048;
const MAX_UNPACK_BYTES: u64 = 128 * 1024 * 1024;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
});

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_dot_dir(root: &Path, id: &str, suffix: &str) -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    root.join(format!(".{id}.{}.{seq}.{suffix}", crate::data::now()))
}

fn is_safe_locale(loc: &str) -> bool {
    !loc.is_empty()
        && loc.len() <= 32
        && loc
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn sanitize_prodversion(raw: &str) -> String {
    let token = raw.split_whitespace().next().unwrap_or("");
    if !token.is_empty()
        && token.contains('.')
        && token.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    {
        token.to_string()
    } else {
        "130.0.0.0".to_string()
    }
}

#[derive(Serialize)]
pub struct ExtInfo {
    id: String,
    name: String,
    version: String,
    /// true if it is loaded in the running WebView2 profile; false means a restart is needed.
    active: bool,
    /// true if the manifest declares an options/settings page.
    options: bool,
}

fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

/// Accepts a 32-letter extension ID or a Chrome Web Store URL.
fn parse_id(input: &str) -> Option<String> {
    let s = input.trim();
    if is_id(s) {
        return Some(s.into());
    }
    let u = tauri::Url::parse(s).ok()?;
    if !matches!(
        u.host_str(),
        Some("chromewebstore.google.com" | "chrome.google.com")
    ) {
        return None;
    }
    u.path_segments()?.find(|p| is_id(p)).map(String::from)
}

fn root(app: &AppHandle) -> Result<PathBuf, String> {
    app.state::<Ext>()
        .0
        .clone()
        .ok_or_else(|| "Extensions folder unavailable".to_string())
}

fn read_json(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

/// Called at startup. WebView2 setup fails if the folder holds anything that
/// isn't a valid extension, so remove stray files, temp dirs and broken installs.
pub fn prepare(root: &Path) -> Vec<String> {
    let mut ids = vec![];
    for entry in fs::read_dir(root).into_iter().flatten().flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if p.is_dir() && !name.starts_with('.') && read_json(&p.join("manifest.json")).is_some() {
            ids.push(name);
        } else if p.is_dir() {
            let _ = fs::remove_dir_all(&p);
        } else {
            let _ = fs::remove_file(&p);
        }
    }
    ids
}

// --- Download ---------------------------------------------------------------

async fn download(id: &str) -> Result<Vec<u8>, String> {
    // Match the engine that actually runs the extension: the WebView2 version.
    let raw_ver = tauri::webview_version().unwrap_or_else(|_| "130.0.0.0".into());
    let ver = sanitize_prodversion(&raw_ver);
    let url = format!(
        "https://clients2.google.com/service/update2/crx?response=redirect&prodversion={ver}&acceptformat=crx2,crx3&x=id%3D{id}%26uc"
    );
    let mut res = HTTP_CLIENT
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Download failed: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("The Web Store answered {}", res.status()));
    }
    if let Some(len) = res.content_length()
        && len > MAX_CRX_BYTES as u64
    {
        return Err("The extension package exceeds the maximum allowed size.".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| format!("Download failed: {e}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_CRX_BYTES {
            return Err("The extension package exceeds the maximum allowed size.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() < 16 {
        return Err("The Web Store has no package for this ID. It may be delisted, unavailable in your region, or need a newer browser engine.".into());
    }
    Ok(bytes)
}

// --- CRX parsing ------------------------------------------------------------

fn u32le(b: &[u8], o: usize) -> Option<usize> {
    let end = o.checked_add(4)?;
    Some(u32::from_le_bytes(b.get(o..end)?.try_into().ok()?) as usize)
}

fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let (mut v, mut shift) = (0u64, 0);
    loop {
        let x = *b.get(*i)?;
        *i += 1;
        v |= ((x & 0x7f) as u64) << shift;
        if x & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Minimal protobuf walker: returns (field number, bytes) for length-delimited fields.
fn fields(b: &[u8]) -> Vec<(u64, &[u8])> {
    let (mut i, mut out) = (0, vec![]);
    while i < b.len() {
        let Some(tag) = varint(b, &mut i) else { break };
        match tag & 7 {
            0 => {
                if varint(b, &mut i).is_none() {
                    break;
                }
            }
            1 => {
                if i + 8 > b.len() {
                    break;
                }
                i += 8;
            }
            5 => {
                if i + 4 > b.len() {
                    break;
                }
                i += 4;
            }
            2 => {
                let Some(len) = varint(b, &mut i) else { break };
                let end = i.saturating_add(len as usize);
                if end > b.len() {
                    break;
                }
                out.push((tag >> 3, &b[i..end]));
                i = end;
            }
            _ => break,
        }
    }
    out
}

/// Chrome extension ID: first 16 bytes of SHA-256(public key), nibbles mapped to a-p.
fn crx_id(key: &[u8]) -> String {
    Sha256::digest(key)[..16]
        .iter()
        .flat_map(|b| [b >> 4, b & 15])
        .map(|n| (b'a' + n) as char)
        .collect()
}

/// Returns the ZIP payload and the DER public key whose hash equals the extension ID.
fn parse_crx<'a>(b: &'a [u8], id: &str) -> Result<(&'a [u8], Vec<u8>), String> {
    let bad = || "The downloaded file is not a valid CRX package".to_string();
    if b.get(..4) != Some(&b"Cr24"[..]) {
        return Err(bad());
    }
    match u32le(b, 4).ok_or_else(bad)? {
        2 => {
            let (pk, sig) = (u32le(b, 8).ok_or_else(bad)?, u32le(b, 12).ok_or_else(bad)?);
            let key_end = 16usize.checked_add(pk).ok_or_else(bad)?;
            let sig_end = key_end.checked_add(sig).ok_or_else(bad)?;
            let key = b.get(16..key_end).ok_or_else(bad)?;
            if key.is_empty() || sig == 0 || crx_id(key) != id {
                return Err(bad());
            }
            Ok((b.get(sig_end..).ok_or_else(bad)?, key.to_vec()))
        }
        3 => {
            let hl = u32le(b, 8).ok_or_else(bad)?;
            let hdr_end = 12usize.checked_add(hl).ok_or_else(bad)?;
            let header = b.get(12..hdr_end).ok_or_else(bad)?;
            let header_fields = fields(header);
            // CrxFileHeader: field 2 = RSA proofs, field 3 = ECDSA proofs; each holds public_key (field 1) and signature (field 2).
            let key = header_fields
                .iter()
                .filter(|(f, _)| *f == 2 || *f == 3)
                .filter_map(|(_, m)| {
                    let pf = fields(m);
                    let k = pf.iter().find(|(g, _)| *g == 1).map(|(_, k)| *k)?;
                    let s = pf.iter().find(|(g, _)| *g == 2).map(|(_, s)| *s)?;
                    if s.is_empty() || crx_id(k) != id {
                        return None;
                    }
                    Some(k)
                })
                .next()
                .ok_or_else(bad)?;
            if let Some((_, signed_hdr)) = header_fields.iter().find(|(f, _)| *f == 10000) {
                let expected_raw_id = &Sha256::digest(key)[..16];
                if let Some((_, hdr_id)) = fields(signed_hdr).into_iter().find(|(g, _)| *g == 1)
                    && hdr_id != expected_raw_id
                {
                    return Err(bad());
                }
            }
            Ok((b.get(hdr_end..).ok_or_else(bad)?, key.to_vec()))
        }
        _ => Err(bad()),
    }
}

// --- Install / list / remove --------------------------------------------------

fn unpack_bounded(
    zip: &[u8],
    key: Vec<u8>,
    dir: &Path,
    max_entries: usize,
    max_bytes: u64,
) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(zip)).map_err(|e| e.to_string())?;
    if archive.len() > max_entries {
        return Err("Extension archive contains too many files".into());
    }
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;

    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| e.to_string())?;
        if entry.is_symlink() {
            return Err("Extension archive contains unsupported symlinks".into());
        }
        let rel = entry
            .enclosed_name()
            .ok_or("Extension archive contains an unsafe path")?;
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            return Err("Extension archive contains an unsafe path".into());
        }
        let out_path = dir.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let remaining = max_bytes.saturating_sub(total);
        let mut out_file = fs::File::create(&out_path).map_err(|e| e.to_string())?;
        let copied = std::io::copy(&mut entry.take(remaining.saturating_add(1)), &mut out_file)
            .map_err(|e| e.to_string())?;
        total = total.saturating_add(copied);
        if total > max_bytes {
            return Err("Extension archive exceeds maximum extracted size".into());
        }
    }

    let mf = dir.join("manifest.json");
    let mut json = read_json(&mf).ok_or("The extension has no readable manifest.json")?;
    let obj = json
        .as_object_mut()
        .ok_or("manifest.json is not an object")?;
    if !obj.contains_key("manifest_version") {
        return Err("manifest.json has no manifest_version".into());
    }
    // Pin the Web Store ID: unpacked extensions otherwise get an ID derived from their path.
    obj.insert("key".into(), STANDARD.encode(key).into());
    fs::write(
        &mf,
        serde_json::to_vec_pretty(&json).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn manifest_fallback_names(dir: &Path) -> Vec<String> {
    let mut names = vec![];
    if let Some(m) = read_json(&dir.join("manifest.json"))
        && let Some(raw) = m["name"].as_str()
        && !raw.is_empty()
    {
        names.push(raw.to_string());
    }
    if let Some(info) = read_info(dir, false)
        && !info.name.is_empty()
        && !names.contains(&info.name)
    {
        names.push(info.name);
    }
    names
}

pub async fn install(app: &AppHandle, input: &str) -> Result<ExtInfo, String> {
    let id = parse_id(input).ok_or("Enter a Chrome Web Store link or a 32-letter extension ID.")?;
    let crx = download(&id).await?;
    let root = root(app)?;
    let tmp = unique_dot_dir(&root, &id, "tmp");
    let dest = root.join(&id);

    let (id_c, tmp_c) = (id.clone(), tmp.clone());
    tokio::task::spawn_blocking(move || {
        let (zip, key) = parse_crx(&crx, &id_c)?;
        if let Err(e) = unpack_bounded(zip, key, &tmp_c, MAX_ZIP_ENTRIES, MAX_UNPACK_BYTES) {
            let _ = fs::remove_dir_all(&tmp_c);
            return Err(e);
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;

    // Unload any previously installed copy so Windows releases file locks before replacing `dest`.
    if dest.exists() {
        let dest_c = dest.clone();
        let fallback_names = tokio::task::spawn_blocking(move || manifest_fallback_names(&dest_c))
            .await
            .unwrap_or_default();
        let _ = live::remove(app, &id, fallback_names).await;
    }

    let (root_c, id_c, tmp_c, dest_c) = (root.clone(), id.clone(), tmp.clone(), dest.clone());
    tokio::task::spawn_blocking(move || {
        if dest_c.exists() && fs::remove_dir_all(&dest_c).is_err() {
            let parked = unique_dot_dir(&root_c, &id_c, "del");
            if let Err(e) = fs::rename(&dest_c, &parked) {
                let _ = fs::remove_dir_all(&tmp_c);
                return Err(e.to_string());
            }
        }
        if let Err(e) = fs::rename(&tmp_c, &dest_c) {
            let _ = fs::remove_dir_all(&tmp_c);
            return Err(e.to_string());
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;

    // Load it into the running profile. If that fails it still loads at next start.
    if live::add(app, &dest).await.is_ok() {
        let state = app.state::<Loaded>();
        let mut loaded = state.0.lock().unwrap();
        if !loaded.contains(&id) {
            loaded.push(id.clone());
        }
    }
    let active = {
        let state = app.state::<Loaded>();
        let loaded = state.0.lock().unwrap();
        loaded.iter().any(|x| x == &id)
    };
    tokio::task::spawn_blocking(move || read_info(&dest, active))
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Installed, but the manifest could not be read".to_string())
}

fn read_info(dir: &Path, active: bool) -> Option<ExtInfo> {
    let id = dir.file_name()?.to_str()?.to_string();
    let m = read_json(&dir.join("manifest.json"))?;
    let mut name = m["name"].as_str().unwrap_or(&id).to_string();
    // Resolve "__MSG_key__" names from the default locale (rejecting unsafe locale names).
    let msg_key = name
        .strip_prefix("__MSG_")
        .and_then(|n| n.strip_suffix("__"))
        .map(str::to_string);
    if let Some(k) = msg_key {
        let loc = m["default_locale"].as_str().unwrap_or("en");
        if is_safe_locale(loc) {
            let msgs = read_json(&dir.join("_locales").join(loc).join("messages.json"));
            let found = msgs
                .as_ref()
                .and_then(|v| v.as_object())
                .and_then(|o| o.iter().find(|(key, _)| key.eq_ignore_ascii_case(&k)))
                .and_then(|(_, e)| e["message"].as_str());
            if let Some(text) = found {
                name = text.to_string();
            }
        }
    }
    Some(ExtInfo {
        version: m["version"].as_str().unwrap_or("").to_string(),
        active,
        options: options_page(&m).is_some(),
        id,
        name,
    })
}

fn info(app: &AppHandle, dir: &Path) -> Option<ExtInfo> {
    let id = dir.file_name()?.to_str()?;
    let active = {
        let state = app.state::<Loaded>();
        let loaded = state.0.lock().unwrap();
        loaded.iter().any(|x| x == id)
    };
    read_info(dir, active)
}

/// Relative path of the extension's settings page (MV3 `options_ui`, or MV2 `options_page`).
fn options_page(m: &Value) -> Option<String> {
    let p = m["options_ui"]["page"]
        .as_str()
        .or_else(|| m["options_page"].as_str())?;
    let p = p.trim_start_matches('/');
    (!p.is_empty()).then(|| p.to_string())
}

/// `chrome-extension://<id>/<options page>` for an extension that WebView2 has loaded.
/// The ID comes from WebView2 itself, so it is right even if it differs from the folder name.
pub async fn options_url(app: &AppHandle, id: &str) -> Result<String, String> {
    if !is_id(id) {
        return Err("Invalid extension ID".into());
    }
    let dir = root(app)?.join(id);
    let m = read_json(&dir.join("manifest.json")).ok_or("Extension not found")?;
    let page = options_page(&m).ok_or("This extension has no settings page")?;
    let raw = m["name"].as_str().unwrap_or("").to_string();
    let resolved = info(app, &dir).map(|i| i.name).unwrap_or_default();

    let loaded = live::list(app)
        .await
        .map_err(|e| format!("Couldn’t read the loaded extensions: {e}"))?;
    let real = loaded
        .iter()
        .find(|(lid, _)| lid == id)
        .or_else(|| loaded.iter().find(|(_, n)| *n == raw || *n == resolved))
        .map(|(lid, _)| lid.clone())
        .ok_or("WebView2 hasn’t loaded this extension. Restart the browser and try again.")?;
    Ok(format!("chrome-extension://{real}/{page}"))
}

pub async fn list(app: &AppHandle) -> Vec<ExtInfo> {
    let Ok(root) = root(app) else { return vec![] };
    let loaded: std::collections::HashSet<String> = {
        let state = app.state::<Loaded>();
        state.0.lock().unwrap().iter().cloned().collect()
    };
    tokio::task::spawn_blocking(move || {
        let mut out: Vec<ExtInfo> = fs::read_dir(root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir()
                    && !p
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .filter_map(|p| {
                let id = p.file_name()?.to_str()?;
                let active = loaded.contains(id);
                read_info(&p, active)
            })
            .collect();
        out.sort_by_key(|e| e.name.to_lowercase());
        out
    })
    .await
    .unwrap_or_default()
}

pub async fn remove(app: &AppHandle, id: &str) -> Result<(), String> {
    if !is_id(id) {
        return Err("Invalid extension ID".into());
    }
    let root = root(app)?;
    let dir = root.join(id);
    let dir_c = dir.clone();
    let fallback_names = tokio::task::spawn_blocking(move || manifest_fallback_names(&dir_c))
        .await
        .unwrap_or_default();

    // WebView2 keeps added extensions in the profile, so unload it there first.
    live::remove(app, id, fallback_names)
        .await
        .map_err(|e| format!("Couldn’t unload the extension: {e}"))?;

    let id_c = id.to_string();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        if dir.exists() && fs::remove_dir_all(&dir).is_err() {
            // Files may still be locked; park the folder where startup cleanup deletes it.
            fs::rename(&dir, unique_dot_dir(&root, &id_c, "del")).map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;

    let state = app.state::<Loaded>();
    state.0.lock().unwrap().retain(|x| x != id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            for (name, data) in entries {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(data).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    fn make_crx2(key: &[u8], sig: &[u8], zip: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"Cr24");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(key.len() as u32).to_le_bytes());
        out.extend_from_slice(&(sig.len() as u32).to_le_bytes());
        out.extend_from_slice(key);
        out.extend_from_slice(sig);
        out.extend_from_slice(zip);
        out
    }

    fn pb_field(num: u64, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut tag = (num << 3) | 2;
        while tag >= 0x80 {
            out.push(((tag as u8) & 0x7f) | 0x80);
            tag >>= 7;
        }
        out.push(tag as u8);
        let mut len = bytes.len() as u64;
        while len >= 0x80 {
            out.push(((len as u8) & 0x7f) | 0x80);
            len >>= 7;
        }
        out.push(len as u8);
        out.extend_from_slice(bytes);
        out
    }

    fn make_crx3(key: &[u8], sig: &[u8], zip: &[u8]) -> Vec<u8> {
        let mut proof = Vec::new();
        proof.extend_from_slice(&pb_field(1, key));
        proof.extend_from_slice(&pb_field(2, sig));
        let mut signed_data = Vec::new();
        signed_data.extend_from_slice(&pb_field(1, &Sha256::digest(key)[..16]));
        let mut header = Vec::new();
        header.extend_from_slice(&pb_field(2, &proof));
        header.extend_from_slice(&pb_field(10000, &signed_data));

        let mut out = Vec::new();
        out.extend_from_slice(b"Cr24");
        out.extend_from_slice(&3u32.to_le_bytes());
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(zip);
        out
    }

    #[test]
    fn test_parse_id() {
        let valid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(parse_id(valid).as_deref(), Some(valid));
        assert_eq!(
            parse_id(&format!("https://chromewebstore.google.com/detail/foo/{valid}")).as_deref(),
            Some(valid)
        );
        assert_eq!(
            parse_id(&format!("https://chrome.google.com/webstore/detail/foo/{valid}")).as_deref(),
            Some(valid)
        );
        assert_eq!(parse_id(&format!("https://evil.com/detail/foo/{valid}")), None);
        assert_eq!(parse_id("not-an-id"), None);
    }

    #[test]
    fn test_is_safe_locale() {
        assert!(is_safe_locale("en"));
        assert!(is_safe_locale("en_US"));
        assert!(is_safe_locale("zh-CN"));
        assert!(!is_safe_locale(""));
        assert!(!is_safe_locale("../evil"));
        assert!(!is_safe_locale("..\\evil"));
        assert!(!is_safe_locale("C:\\Windows"));
        assert!(!is_safe_locale("\\\\attacker\\share"));
        assert!(!is_safe_locale("a/b"));
    }

    #[test]
    fn test_read_info_blocks_locale_traversal() {
        let base = std::env::temp_dir().join(format!(
            "kbrowser_locale_test_{}_{}",
            std::process::id(),
            crate::data::now()
        ));
        let ext_dir = base.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let outside_dir = base.join("outside");
        fs::create_dir_all(&ext_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(
            outside_dir.join("messages.json"),
            br#"{"app":{"message":"PWNED"}}"#,
        )
        .unwrap();
        fs::write(
            ext_dir.join("manifest.json"),
            br#"{"manifest_version":3,"name":"__MSG_app__","version":"1.0","default_locale":"../../outside"}"#,
        )
        .unwrap();

        let info = read_info(&ext_dir, true).unwrap();
        assert_ne!(info.name, "PWNED");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_parse_crx_key_validation() {
        let key = b"test-public-key-bytes";
        let id = crx_id(key);
        let wrong_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        let crx2 = make_crx2(key, b"sig", b"PK\x05\x06");
        assert!(parse_crx(&crx2, wrong_id).is_err());
        let (z2, k2) = parse_crx(&crx2, &id).unwrap();
        assert_eq!(z2, b"PK\x05\x06");
        assert_eq!(k2, key);

        let crx3_no_key = {
            let mut out = Vec::new();
            out.extend_from_slice(b"Cr24");
            out.extend_from_slice(&3u32.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(b"PK\x05\x06");
            out
        };
        assert!(parse_crx(&crx3_no_key, &id).is_err());

        let crx3 = make_crx3(key, b"sig", b"PK\x05\x06");
        let (z3, k3) = parse_crx(&crx3, &id).unwrap();
        assert_eq!(z3, b"PK\x05\x06");
        assert_eq!(k3, key);
    }

    #[test]
    fn test_unpack_bounds_and_zip_slip() {
        let base = std::env::temp_dir().join(format!(
            "kbrowser_zip_test_{}_{}",
            std::process::id(),
            crate::data::now()
        ));
        let _ = fs::remove_dir_all(&base);

        // 1. Valid extension
        let valid_zip = make_zip(&[(
            "manifest.json",
            br#"{"manifest_version":3,"name":"Test","version":"1.0"}"#,
        )]);
        let d1 = base.join("valid");
        unpack_bounded(&valid_zip, b"key123".to_vec(), &d1, 10, 1024).unwrap();
        let mf = read_json(&d1.join("manifest.json")).unwrap();
        assert_eq!(mf["key"].as_str(), Some(STANDARD.encode(b"key123").as_str()));

        // 2. Zip slip
        let slip_zip = make_zip(&[("../evil.txt", b"boom")]);
        let d2 = base.join("slip");
        assert!(unpack_bounded(&slip_zip, b"key123".to_vec(), &d2, 10, 1024).is_err());
        assert!(!base.join("evil.txt").exists());

        // 3. Too many entries
        let many_zip = make_zip(&[("a.txt", b"1"), ("b.txt", b"2"), ("c.txt", b"3")]);
        let d3 = base.join("many");
        assert!(unpack_bounded(&many_zip, b"key123".to_vec(), &d3, 2, 1024).is_err());

        // 4. Decompressed byte cap exceeded
        let big_zip = make_zip(&[("big.bin", &[b'x'; 200])]);
        let d4 = base.join("big");
        assert!(unpack_bounded(&big_zip, b"key123".to_vec(), &d4, 10, 100).is_err());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn test_sanitize_prodversion() {
        assert_eq!(sanitize_prodversion("130.0.2849.46 beta"), "130.0.2849.46");
        assert_eq!(sanitize_prodversion("131.0.0.0"), "131.0.0.0");
        assert_eq!(sanitize_prodversion(""), "130.0.0.0");
        assert_eq!(sanitize_prodversion("bad version!"), "130.0.0.0");
    }
}
