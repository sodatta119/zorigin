//! JNI bridge for the Android app.
//!
//! The Android app is a thin Kotlin shell; the actual file-transfer server is
//! [`znet_core::web`], the same code the desktop CLI runs. On Android the phone
//! *is* the server, so a foreground service calls these functions to start the
//! server (bound to all interfaces so devices on the home Wi-Fi can reach it),
//! query its URL for display, and stop it.
//!
//! The exported symbol names must match a Kotlin class exactly. They map to:
//!
//! ```kotlin
//! package com.zap.transfer
//! object NativeBridge {
//!     external fun nativeStart(dir: String, port: Int): Long  // 0 on failure
//!     external fun nativeUrl(handle: Long): String?
//!     external fun nativeStop(handle: Long)
//! }
//! ```
//!
//! `nativeStart` returns an opaque handle (a raw pointer as a `jlong`) that the
//! Kotlin side stores and passes back to `nativeUrl` / `nativeStop`. It owns the
//! running server; `nativeStop` frees it and shuts the server down.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use jni::objects::{JClass, JString};
use jni::sys::{jint, jlong, jstring};
use jni::JNIEnv;

use znet_core::web::fast_client::{self, GetOptions, Progress, Report};
use znet_core::web::{self, Credentials, Direction, ServeConfig, ServerHandle, ServerInfo};

/// Owns a running server plus its connection details, boxed and handed to Kotlin
/// as an opaque `jlong` handle.
struct Running {
    info: ServerInfo,
    // Kept alive for the server's lifetime; dropping it stops the server.
    handle: ServerHandle,
}

/// Read a Java string that may be null or empty, returning `None` in those cases.
fn read_opt(env: &mut JNIEnv, s: JString) -> Option<String> {
    if s.is_null() {
        return None;
    }
    match env.get_string(&s) {
        Ok(js) => {
            let v: String = js.into();
            if v.is_empty() {
                None
            } else {
                Some(v)
            }
        }
        Err(_) => None,
    }
}

/// Start the server sharing `dir` on `port`, bound to all interfaces.
/// `user`/`pass` may be null/empty for no authentication; if both are present,
/// the server requires HTTP Basic auth. Returns an opaque handle, or 0 on error.
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeStart<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    dir: JString<'local>,
    port: jint,
    user: JString<'local>,
    pass: JString<'local>,
    history: JString<'local>,
) -> jlong {
    let dir: String = match env.get_string(&dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };

    let auth = match (read_opt(&mut env, user), read_opt(&mut env, pass)) {
        (Some(user), Some(pass)) => Some(Credentials { user, pass }),
        _ => None,
    };

    let config = ServeConfig {
        dir: PathBuf::from(dir),
        port: port as u16,
        bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED), // 0.0.0.0 - reachable on the LAN
        auth,
        history: read_opt(&mut env, history).map(PathBuf::from),
        index_html: None,
        tls: None,
    };

    match web::spawn(config) {
        Ok((info, handle)) => {
            let running = Box::new(Running {
                info,
                handle,
            });
            Box::into_raw(running) as jlong
        }
        Err(_) => 0,
    }
}

/// Return the URL another device should open, or null for an invalid handle.
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeUrl<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    if handle == 0 {
        return std::ptr::null_mut();
    }
    // Safety: `handle` is a pointer produced by `nativeStart` and not yet freed.
    let running = unsafe { &*(handle as *const Running) };
    match env.new_string(running.info.url()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Return the number of HTTP requests any client has made since start, or 0 for
/// an invalid handle. While this stays 0 the UI can warn that no device has been
/// able to reach the phone (wrong Wi-Fi / AP-client isolation / firewall).
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeRequests(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jlong {
    if handle == 0 {
        return 0;
    }
    // Safety: `handle` is a pointer produced by `nativeStart` and not yet freed.
    let running = unsafe { &*(handle as *const Running) };
    running.handle.requests_seen() as jlong
}

/// Remove one transfer (by id) from the activity list + persisted history.
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeRemoveTransfer(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    id: jlong,
) {
    if handle == 0 {
        return;
    }
    // Safety: `handle` is a pointer produced by `nativeStart` and not yet freed.
    let running = unsafe { &*(handle as *const Running) };
    running.handle.remove_transfer(id as u64);
}

/// Clear finished (past) transfers, keeping any still in progress.
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeClearTransfers(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // Safety: `handle` is a pointer produced by `nativeStart` and not yet freed.
    let running = unsafe { &*(handle as *const Running) };
    running.handle.clear_transfers();
}

/// Return the share URL (includes the pairing key when secured, so the
/// recipient is signed in on open), or null for an invalid handle.
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeShareUrl<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    if handle == 0 {
        return std::ptr::null_mut();
    }
    // Safety: `handle` is a pointer produced by `nativeStart` and not yet freed.
    let running = unsafe { &*(handle as *const Running) };
    match env.new_string(running.info.url_with_key()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Return recent transfers as a JSON array, or "[]" for an invalid handle.
/// Each item: `{"id","name","path","dir":"up"|"down","done","total"|null,
/// "finished","ok","verified","fast"}` (`fast` = went over the native fast lane).
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeTransfers<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    let json = if handle == 0 {
        "[]".to_string()
    } else {
        // Safety: `handle` is a pointer produced by `nativeStart` and not yet freed.
        let running = unsafe { &*(handle as *const Running) };
        transfers_json(&running.handle.transfers())
    };
    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn transfers_json(items: &[znet_core::web::TransferInfo]) -> String {
    let mut s = String::from("[");
    for (i, t) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let dir = match t.direction {
            Direction::Upload => "up",
            Direction::Download => "down",
        };
        let total = t.total.map(|n| n.to_string()).unwrap_or_else(|| "null".to_string());
        s.push_str(&format!(
            "{{\"id\":{},\"name\":{},\"path\":{},\"dir\":\"{}\",\"done\":{},\"total\":{},\"finished\":{},\"ok\":{},\"verified\":{},\"fast\":{}}}",
            t.id,
            json_string(&t.name),
            json_string(&t.path),
            dir,
            t.done,
            total,
            t.finished,
            t.ok,
            t.verified,
            t.fast
        ));
    }
    s.push(']');
    s
}

/// Minimal JSON string escaping (quotes + backslash + control chars).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- Native fast-lane client (Receive): download from another Zap ----

/// A background download started by [`nativeGet`]. The Rust worker thread runs
/// the blocking fast-lane client (with HTTP fallback) and posts its result into
/// `done`; Kotlin polls [`nativeGetStatus`] for the progress bar + completion.
struct GetJob {
    progress: Arc<Progress>,
    done: Arc<Mutex<Option<std::result::Result<Report, String>>>>,
}

/// Start downloading `url` into `dest_dir` over the fast lane (HTTP fallback).
/// Returns an opaque handle for [`nativeGetStatus`] / [`nativeGetFree`], or 0 if
/// the arguments couldn't be read.
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeGet<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    url: JString<'local>,
    dest_dir: JString<'local>,
) -> jlong {
    let url: String = match env.get_string(&url) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let dest: String = match env.get_string(&dest_dir) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };

    let progress = Arc::new(Progress::default());
    let done = Arc::new(Mutex::new(None));
    let job = Box::new(GetJob {
        progress: Arc::clone(&progress),
        done: Arc::clone(&done),
    });
    std::thread::spawn(move || {
        let out = fast_client::get_with_progress(&url, Path::new(&dest), GetOptions::default(), progress)
            .map_err(|e| format!("{e:#}"));
        if let Ok(mut slot) = done.lock() {
            *slot = Some(out);
        }
    });
    Box::into_raw(job) as jlong
}

/// Poll a download's status as JSON:
/// `{"done","total","running","ok","fast","verified","name","error"}`.
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeGetStatus<'local>(
    env: JNIEnv<'local>,
    _class: JClass<'local>,
    handle: jlong,
) -> jstring {
    let json = if handle == 0 {
        "{\"running\":false,\"ok\":false,\"error\":\"bad handle\"}".to_string()
    } else {
        // Safety: `handle` came from `nativeGet` and is not yet freed.
        let job = unsafe { &*(handle as *const GetJob) };
        let done = job.progress.done.load(Ordering::Relaxed);
        let total = job.progress.total.load(Ordering::Relaxed);
        let finished = job.done.lock().ok().map(|s| s.is_some()).unwrap_or(false);
        if !finished {
            format!("{{\"running\":true,\"done\":{done},\"total\":{total}}}")
        } else {
            match job.done.lock().ok().and_then(|s| s.clone()) {
                Some(Ok(r)) => {
                    let name = r
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    format!(
                        "{{\"running\":false,\"ok\":true,\"done\":{},\"total\":{},\"fast\":{},\"verified\":{},\"name\":{}}}",
                        r.total, r.total, r.used_fast, r.verified, json_string(&name)
                    )
                }
                Some(Err(e)) => {
                    format!("{{\"running\":false,\"ok\":false,\"error\":{}}}", json_string(&e))
                }
                None => "{\"running\":true}".to_string(),
            }
        }
    };
    match env.new_string(json) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a download handle from [`nativeGet`]. Safe with 0 (no-op).
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeGetFree(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // Safety: `handle` came from `nativeGet` and is freed exactly once here.
    unsafe {
        drop(Box::from_raw(handle as *mut GetJob));
    }
}

/// Stop the server and free the handle. Safe to call with 0 (no-op).
#[no_mangle]
pub extern "system" fn Java_com_zap_transfer_NativeBridge_nativeStop(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    if handle == 0 {
        return;
    }
    // Safety: `handle` came from `nativeStart` and is freed exactly once here.
    // Dropping the box drops the `ServerHandle`, which stops the server.
    unsafe {
        drop(Box::from_raw(handle as *mut Running));
    }
}
