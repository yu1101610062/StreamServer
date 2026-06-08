use crate::{
    dispatch_json,
    models::{NativeEnvelope, NativeErrorEnvelope},
};
use serde_json::Value;
use std::{
    ffi::{CStr, CString, c_char},
    ptr,
    sync::OnceLock,
};
use tokio::runtime::Runtime;

#[unsafe(no_mangle)]
pub extern "C" fn streamserver_desktop_json_call(input: *const c_char) -> *mut c_char {
    let response = match read_input(input) {
        Ok(input) => runtime().block_on(async move { dispatch_json(&input).await }),
        Err(error) => Err(error),
    };
    let json = match response {
        Ok(data) => serde_json::to_string(&NativeEnvelope {
            ok: true,
            data: Some(&data),
            error: None,
        }),
        Err(error) => {
            let error = NativeErrorEnvelope::from(error);
            serde_json::to_string(&NativeEnvelope {
                ok: false,
                data: None,
                error: Some(error),
            })
        }
    }
    .unwrap_or_else(|error| {
        serde_json::to_string(&serde_json::json!({
            "ok": false,
            "error": { "kind": "serialization", "message": error.to_string() }
        }))
        .unwrap()
    });

    CString::new(json)
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn streamserver_desktop_string_free(value: *mut c_char) {
    if value.is_null() {
        return;
    }
    unsafe {
        drop(CString::from_raw(value));
    }
}

fn read_input(input: *const c_char) -> Result<String, crate::models::NativeError> {
    if input.is_null() {
        return Err(crate::models::NativeError::InvalidRequest(
            "input pointer is null".to_string(),
        ));
    }
    let value = unsafe { CStr::from_ptr(input) };
    value
        .to_str()
        .map(ToString::to_string)
        .map_err(|error| crate::models::NativeError::InvalidRequest(error.to_string()))
}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME
        .get_or_init(|| Runtime::new().expect("failed to initialize StreamServer native runtime"))
}

#[allow(dead_code)]
fn _assert_json_send_sync(_: Value) {}
