//! C ABI. Every call takes and returns JSON strings, so bindings stay tiny:
//!
//! ```c
//! OntoHandle *h = onto_open("onto-intelli.toml");      // or onto_open_memory()
//! char *out = onto_call(h, "intelli.ask", "{\"question\":\"Who owns Project Alpha?\"}");
//! // out = {"ok":true,"result":{...}}  or  {"ok":false,"error":{"code":"...","message":"..."}}
//! onto_string_free(out);
//! onto_close(h);
//! ```
//!
//! Handles are thread-safe; strings returned by the library must be released
//! with `onto_string_free`.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use serde_json::{Value, json};

use onto_intelli::blocking::BlockingIntelli;
use onto_intelli::{Config, Error, Principal};

pub struct OntoHandle {
    engine: BlockingIntelli,
    principal: Principal,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_error(msg: impl Into<String>) {
    let c = CString::new(msg.into().replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(c));
}

fn into_c(s: String) -> *mut c_char {
    CString::new(s.replace('\0', " ")).map(CString::into_raw).unwrap_or(ptr::null_mut())
}

/// # Safety
/// `p` must be null or a valid NUL-terminated string.
unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: caller guarantees a valid C string.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

fn open_with(cfg: Config) -> *mut OntoHandle {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let principal = cfg.local.principal.clone();
        BlockingIntelli::open(cfg).map(|engine| OntoHandle { engine, principal })
    }));
    match result {
        Ok(Ok(h)) => Box::into_raw(Box::new(h)),
        Ok(Err(e)) => {
            set_error(e.to_string());
            ptr::null_mut()
        }
        Err(_) => {
            set_error("panic while opening onto-intelli");
            ptr::null_mut()
        }
    }
}

/// Open the engine from a config file (null: default discovery).
/// Returns null on failure; see `onto_last_error`.
///
/// # Safety
/// `config_path` must be null or a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn onto_open(config_path: *const c_char) -> *mut OntoHandle {
    // SAFETY: forwarded caller guarantee.
    let path = unsafe { str_arg(config_path) };
    match Config::discover(path.map(std::path::Path::new)) {
        Ok(cfg) => open_with(cfg),
        Err(e) => {
            set_error(e.to_string());
            ptr::null_mut()
        }
    }
}

/// Open a volatile in-memory engine with the offline mock LLM.
#[unsafe(no_mangle)]
pub extern "C" fn onto_open_memory() -> *mut OntoHandle {
    open_with(Config::in_memory())
}

/// Run an engine action (see `system.actions`). Never returns null for a
/// valid handle: errors come back as `{"ok":false,"error":{...}}`.
///
/// # Safety
/// `handle` must come from `onto_open*`; strings must be NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn onto_call(
    handle: *mut OntoHandle,
    action: *const c_char,
    input_json: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        set_error("null handle");
        return ptr::null_mut();
    }
    // SAFETY: non-null handle created by onto_open; shared access is thread-safe.
    let h = unsafe { &*handle };
    // SAFETY: forwarded caller guarantee.
    let (action, input) = unsafe { (str_arg(action), str_arg(input_json)) };
    let out = catch_unwind(AssertUnwindSafe(|| -> Result<Value, Error> {
        let action = action.ok_or_else(|| Error::invalid("action is required"))?;
        let input: Value = match input {
            Some(s) if !s.trim().is_empty() => serde_json::from_str(s)?,
            _ => json!({}),
        };
        h.engine.call(&h.principal, action, input)
    }));
    let body = match out {
        Ok(Ok(result)) => json!({ "ok": true, "result": result }),
        Ok(Err(e)) => json!({ "ok": false, "error": { "code": e.code(), "message": e.to_string() } }),
        Err(_) => json!({ "ok": false, "error": { "code": "internal", "message": "panic inside onto-intelli" } }),
    };
    into_c(body.to_string())
}

/// Like `onto_call`, as an explicit principal: `principal_json` is
/// `{"id": "user:42", "roles": ["member"], "workspaces": ["*"], "entity": "person:42"}`,
/// so per-user roles and ACLs apply instead of the configured `[local]` principal.
///
/// # Safety
/// Same as `onto_call`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn onto_call_as(
    handle: *mut OntoHandle,
    principal_json: *const c_char,
    action: *const c_char,
    input_json: *const c_char,
) -> *mut c_char {
    if handle.is_null() {
        set_error("null handle");
        return ptr::null_mut();
    }
    // SAFETY: non-null handle created by onto_open; shared access is thread-safe.
    let h = unsafe { &*handle };
    // SAFETY: forwarded caller guarantee.
    let (who, action, input) = unsafe { (str_arg(principal_json), str_arg(action), str_arg(input_json)) };
    let out = catch_unwind(AssertUnwindSafe(|| -> Result<Value, Error> {
        let who: Principal = serde_json::from_str(who.ok_or_else(|| Error::invalid("principal_json is required"))?)?;
        let action = action.ok_or_else(|| Error::invalid("action is required"))?;
        let input: Value = match input {
            Some(s) if !s.trim().is_empty() => serde_json::from_str(s)?,
            _ => json!({}),
        };
        h.engine.call(&who, action, input)
    }));
    let body = match out {
        Ok(Ok(result)) => json!({ "ok": true, "result": result }),
        Ok(Err(e)) => json!({ "ok": false, "error": { "code": e.code(), "message": e.to_string() } }),
        Err(_) => json!({ "ok": false, "error": { "code": "internal", "message": "panic inside onto-intelli" } }),
    };
    into_c(body.to_string())
}

/// Convenience: ask a question. Same return shape as `onto_call`.
///
/// # Safety
/// Same as `onto_call`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn onto_ask(handle: *mut OntoHandle, question: *const c_char) -> *mut c_char {
    // SAFETY: forwarded caller guarantee.
    let q = unsafe { str_arg(question) }.unwrap_or_default();
    let input = CString::new(json!({ "question": q }).to_string()).unwrap_or_default();
    // SAFETY: valid strings built above.
    unsafe { onto_call(handle, c"intelli.ask".as_ptr(), input.as_ptr()) }
}

/// Release a string returned by this library.
///
/// # Safety
/// `s` must be null or a pointer returned by this library, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn onto_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: allocated by CString::into_raw in this library.
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Close the engine and release the handle.
///
/// # Safety
/// `handle` must be null or come from `onto_open*`, closed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn onto_close(handle: *mut OntoHandle) {
    if !handle.is_null() {
        // SAFETY: allocated by Box::into_raw in open_with.
        drop(unsafe { Box::from_raw(handle) });
    }
}

/// Last error on this thread (valid until the next failing call), or null.
#[unsafe(no_mangle)]
pub extern "C" fn onto_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ref().map(|c| c.as_ptr()).unwrap_or(ptr::null()))
}

/// Library version, static string.
#[unsafe(no_mangle)]
pub extern "C" fn onto_version() -> *const c_char {
    static VERSION: &CStr = match CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes()) {
        Ok(v) => v,
        Err(_) => c"unknown",
    };
    VERSION.as_ptr()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(h: *mut OntoHandle, action: &str, input: &str) -> Value {
        let a = CString::new(action).unwrap();
        let i = CString::new(input).unwrap();
        unsafe {
            let out = onto_call(h, a.as_ptr(), i.as_ptr());
            let v: Value = serde_json::from_str(CStr::from_ptr(out).to_str().unwrap()).unwrap();
            onto_string_free(out);
            v
        }
    }

    #[test]
    fn round_trip_through_the_c_abi() {
        let h = onto_open_memory();
        assert!(!h.is_null());
        let r = call(h, "entity.upsert", r#"{"kind":"team","key":"core","title":"Core Team"}"#);
        assert_eq!(r["ok"], true, "{r}");
        call(
            h,
            "entity.upsert",
            r#"{"kind":"project","key":"x","title":"Project X","links":[{"rel":"owned_by","to":"team:core"}]}"#,
        );
        let q = CString::new("Who owns Project X?").unwrap();
        let out = unsafe { onto_ask(h, q.as_ptr()) };
        let v: Value = serde_json::from_str(unsafe { CStr::from_ptr(out) }.to_str().unwrap()).unwrap();
        unsafe { onto_string_free(out) };
        assert!(v["result"]["answer"].as_str().unwrap().contains("Core Team"), "{v}");
        let err = call(h, "entity.get", r#"{"id":"nope:1"}"#);
        assert_eq!(err["ok"], false);
        assert_eq!(err["error"]["code"], "not_found");
        unsafe { onto_close(h) };
        assert!(!onto_version().is_null());
    }
}
