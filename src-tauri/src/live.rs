//! Add/remove extensions on the *running* WebView2 profile, so no restart is needed.
//! Uses ICoreWebView2Profile7 (AddBrowserExtension / GetBrowserExtensions) through
//! `Webview::with_webview`, which runs the closure on the main (UI) thread.

use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};

use tauri::{AppHandle, Manager, webview::PlatformWebview};
use tokio::sync::oneshot;
use webview2_com::{
    BrowserExtensionRemoveCompletedHandler,
    Microsoft::Web::WebView2::Win32::{ICoreWebView2_13, ICoreWebView2Profile7},
    ProfileAddBrowserExtensionCompletedHandler, ProfileGetBrowserExtensionsCompletedHandler,
    take_pwstr,
};
use windows::core::{HSTRING, Interface, PWSTR};

/// One-shot reply slot, shared between the call site and the COM completion handler.
type Reply<T> = Rc<RefCell<Option<oneshot::Sender<Result<T, String>>>>>;

fn finish<T>(reply: &Reply<T>, value: Result<T, String>) {
    if let Some(tx) = reply.borrow_mut().take() {
        let _ = tx.send(value);
    }
}

fn profile(pw: &PlatformWebview) -> windows::core::Result<ICoreWebView2Profile7> {
    let core = unsafe { pw.controller().CoreWebView2()? };
    let core13: ICoreWebView2_13 = core.cast()?;
    unsafe { core13.Profile()? }.cast()
}

/// Runs `f` on the UI thread with the profile, then waits for `f` to call `finish`.
async fn on_profile<T, F>(app: &AppHandle, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(ICoreWebView2Profile7, Reply<T>) + Send + 'static,
{
    // Target a tab webview rather than "ui", since "ui" is isolated in its own profile without extensions.
    let tab = app
        .webviews()
        .into_iter()
        .find(|(name, _)| name.starts_with("tab-"))
        .map(|(_, wv)| wv)
        .ok_or("No active tab webview available")?;
    let (tx, rx) = oneshot::channel();
    tab.with_webview(move |pw| {
        let reply: Reply<T> = Rc::new(RefCell::new(Some(tx)));
        match profile(&pw) {
            Ok(p) => f(p, reply),
            Err(e) => finish(&reply, Err(e.to_string())),
        }
    })
    .map_err(|e| e.to_string())?;
    match tokio::time::timeout(Duration::from_secs(15), rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("WebView2 dropped the request".into()),
        Err(_) => Err("WebView2 did not answer in time".into()),
    }
}

/// Loads the unpacked extension in `dir` into the running profile.
/// Returns the extension ID assigned by WebView2 (or empty string if unavailable).
pub async fn add(app: &AppHandle, dir: &Path) -> Result<String, String> {
    let path = HSTRING::from(dir.as_os_str());
    on_profile(app, move |profile, reply| {
        let done = reply.clone();
        let handler =
            ProfileAddBrowserExtensionCompletedHandler::create(Box::new(move |res, ext| {
                let outcome = (|| -> windows::core::Result<String> {
                    res?;
                    if let Some(ext) = ext {
                        let mut raw = PWSTR::null();
                        unsafe { ext.Id(&mut raw)? };
                        Ok(take_pwstr(raw))
                    } else {
                        Ok(String::new())
                    }
                })();
                finish(&done, outcome.map_err(|e| e.to_string()));
                Ok(())
            }));
        if let Err(e) = unsafe { profile.AddBrowserExtension(&path, &handler) } {
            finish(&reply, Err(e.to_string()));
        }
    })
    .await
}

/// Removes the extension matching `id` (or any fallback manifest name) from the running profile.
pub async fn remove(
    app: &AppHandle,
    id: &str,
    fallback_names: Vec<String>,
) -> Result<(), String> {
    let id = id.to_string();
    on_profile(app, move |profile, reply| {
        let r = reply.clone();
        let handler =
            ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |res, list| {
                // Ok(true) = removal started (its own handler will reply); Ok(false) = not loaded.
                let outcome = (|| -> windows::core::Result<bool> {
                    res?;
                    // This webview2-com version hands over an Option, not a Result.
                    let list = list.ok_or_else(|| {
                        windows::core::Error::from(windows::core::HRESULT(0x8000_4003_u32 as i32))
                    })?; // E_POINTER
                    let mut count = 0u32;
                    unsafe { list.Count(&mut count)? };
                    for i in 0..count {
                        let ext = unsafe { list.GetValueAtIndex(i)? };
                        let mut raw_id = PWSTR::null();
                        unsafe { ext.Id(&mut raw_id)? };
                        let ext_id = take_pwstr(raw_id);
                        let mut matches = ext_id == id;
                        if !matches && !fallback_names.is_empty() {
                            let mut raw_name = PWSTR::null();
                            unsafe { ext.Name(&mut raw_name)? };
                            let ext_name = take_pwstr(raw_name);
                            matches = fallback_names.iter().any(|n| n == &ext_name);
                        }
                        if matches {
                            let done = r.clone();
                            let rm = BrowserExtensionRemoveCompletedHandler::create(Box::new(
                                move |res| {
                                    finish(&done, res.map_err(|e| e.to_string()));
                                    Ok(())
                                },
                            ));
                            unsafe { ext.Remove(&rm)? };
                            return Ok(true);
                        }
                    }
                    Ok(false)
                })();
                match outcome {
                    Ok(true) => {}
                    Ok(false) => finish(&r, Ok(())),
                    Err(e) => finish(&r, Err(e.to_string())),
                }
                Ok(())
            }));
        if let Err(e) = unsafe { profile.GetBrowserExtensions(&handler) } {
            finish(&reply, Err(e.to_string()));
        }
    })
    .await
}

/// `(id, name)` of every extension loaded in the running profile. The id here is the one
/// WebView2 actually uses in `chrome-extension://<id>/` URLs.
pub async fn list(app: &AppHandle) -> Result<Vec<(String, String)>, String> {
    on_profile(app, move |profile, reply| {
        let r = reply.clone();
        let handler =
            ProfileGetBrowserExtensionsCompletedHandler::create(Box::new(move |res, list| {
                let outcome = (|| -> windows::core::Result<Vec<(String, String)>> {
                    res?;
                    let list = list.ok_or_else(|| {
                        windows::core::Error::from(windows::core::HRESULT(0x8000_4003_u32 as i32))
                    })?; // E_POINTER
                    let mut count = 0u32;
                    unsafe { list.Count(&mut count)? };
                    let mut out = vec![];
                    for i in 0..count {
                        let ext = unsafe { list.GetValueAtIndex(i)? };
                        let mut raw_id = PWSTR::null();
                        unsafe { ext.Id(&mut raw_id)? };
                        let id = take_pwstr(raw_id);
                        let mut raw_name = PWSTR::null();
                        unsafe { ext.Name(&mut raw_name)? };
                        let name = take_pwstr(raw_name);
                        out.push((id, name));
                    }
                    Ok(out)
                })();
                finish(&r, outcome.map_err(|e| e.to_string()));
                Ok(())
            }));
        if let Err(e) = unsafe { profile.GetBrowserExtensions(&handler) } {
            finish(&reply, Err(e.to_string()));
        }
    })
    .await
}
