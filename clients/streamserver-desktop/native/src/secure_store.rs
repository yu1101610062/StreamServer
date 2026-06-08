use crate::models::NativeError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use keyring::Entry;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    path::{Path, PathBuf},
};

const APP_DIR: &str = "StreamServerDesktop";
const KEYRING_SERVICE: &str = "StreamServerDesktop";

pub fn read(key: &str) -> Result<Option<String>, NativeError> {
    validate_key(key)?;
    if env::var("STREAMSERVER_DESKTOP_STORE_DIR").is_err() {
        return read_native(key);
    }
    read_fallback(key)
}

pub fn write(key: &str, value: &str) -> Result<(), NativeError> {
    validate_key(key)?;
    if env::var("STREAMSERVER_DESKTOP_STORE_DIR").is_err() {
        return write_native(key, value);
    }
    write_fallback(key, value)
}

pub fn delete(key: &str) -> Result<(), NativeError> {
    validate_key(key)?;
    if env::var("STREAMSERVER_DESKTOP_STORE_DIR").is_err() {
        return delete_native(key);
    }
    delete_fallback(key)
}

pub fn probe() -> Value {
    let fallback_dir = env::var("STREAMSERVER_DESKTOP_STORE_DIR").ok();
    let backend = if fallback_dir.is_some() {
        "file_fallback"
    } else if cfg!(target_os = "macos") {
        "macos_keychain"
    } else if cfg!(target_os = "windows") {
        "windows_credential_manager"
    } else {
        "linux_secret_service"
    };
    let writable = write("__probe__", "ok")
        .and_then(|_| read("__probe__"))
        .map(|value| value.as_deref() == Some("ok"))
        .unwrap_or(false);
    let _ = delete("__probe__");
    json!({
        "backend": backend,
        "native": fallback_dir.is_none(),
        "writable": writable,
        "fallback_dir": fallback_dir,
    })
}

fn read_native(key: &str) -> Result<Option<String>, NativeError> {
    let entry = native_entry(key)?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(NativeError::SecureStore(error.to_string())),
    }
}

fn write_native(key: &str, value: &str) -> Result<(), NativeError> {
    native_entry(key)?
        .set_password(value)
        .map_err(|error| NativeError::SecureStore(error.to_string()))
}

fn delete_native(key: &str) -> Result<(), NativeError> {
    match native_entry(key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(NativeError::SecureStore(error.to_string())),
    }
}

fn native_entry(key: &str) -> Result<Entry, NativeError> {
    Entry::new(KEYRING_SERVICE, key).map_err(|error| NativeError::SecureStore(error.to_string()))
}

fn read_fallback(key: &str) -> Result<Option<String>, NativeError> {
    let store = read_store()?;
    let Some(Value::String(encoded)) = store.get(key) else {
        return Ok(None);
    };
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| NativeError::SecureStore(error.to_string()))?;
    let decoded = xor_with_machine_key(&bytes);
    String::from_utf8(decoded)
        .map(Some)
        .map_err(|error| NativeError::SecureStore(error.to_string()))
}

fn write_fallback(key: &str, value: &str) -> Result<(), NativeError> {
    let mut store = read_store()?;
    let encoded = STANDARD.encode(xor_with_machine_key(value.as_bytes()));
    store.insert(key.to_string(), Value::String(encoded));
    write_store(&store)
}

fn delete_fallback(key: &str) -> Result<(), NativeError> {
    let mut store = read_store()?;
    store.remove(key);
    write_store(&store)
}

fn validate_key(key: &str) -> Result<(), NativeError> {
    if key.trim().is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(NativeError::SecureStore(
            "secure store key must be non-empty and contain only alnum, dot, dash or underscore"
                .to_string(),
        ));
    }
    Ok(())
}

fn read_store() -> Result<Map<String, Value>, NativeError> {
    let path = store_path()?;
    if !path.exists() {
        return Ok(Map::new());
    }
    let text = fs::read_to_string(&path)?;
    let value = serde_json::from_str::<Value>(&text)?;
    Ok(value.as_object().cloned().unwrap_or_default())
}

fn write_store(store: &Map<String, Value>) -> Result<(), NativeError> {
    let path = store_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(&Value::Object(store.clone()))?,
    )?;
    set_private_permissions(&tmp)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn store_path() -> Result<PathBuf, NativeError> {
    if let Ok(path) = env::var("STREAMSERVER_DESKTOP_STORE_DIR") {
        return Ok(PathBuf::from(path).join("secure-store.json"));
    }
    Ok(app_config_dir()?.join("secure-store.json"))
}

fn app_config_dir() -> Result<PathBuf, NativeError> {
    if cfg!(target_os = "windows") {
        if let Ok(appdata) = env::var("APPDATA") {
            return Ok(PathBuf::from(appdata).join(APP_DIR));
        }
    }
    if cfg!(target_os = "macos") {
        if let Ok(home) = env::var("HOME") {
            return Ok(PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(APP_DIR));
        }
    }
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("streamserver-desktop"));
    }
    if let Ok(home) = env::var("HOME") {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("streamserver-desktop"));
    }
    Err(NativeError::SecureStore(
        "could not determine user config directory".to_string(),
    ))
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), NativeError> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), NativeError> {
    Ok(())
}

fn xor_with_machine_key(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(env::var("USER").unwrap_or_default());
    hasher.update(env::var("USERNAME").unwrap_or_default());
    hasher.update(env::var("HOME").unwrap_or_default());
    hasher.update(env::var("COMPUTERNAME").unwrap_or_default());
    let key = hasher.finalize();
    bytes
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ key[index % key.len()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_store_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            env::set_var("STREAMSERVER_DESKTOP_STORE_DIR", tmp.path());
        }
        write("refresh_token", "secret-value").unwrap();
        assert_eq!(
            read("refresh_token").unwrap(),
            Some("secret-value".to_string())
        );
        delete("refresh_token").unwrap();
        assert_eq!(read("refresh_token").unwrap(), None);
        unsafe {
            env::remove_var("STREAMSERVER_DESKTOP_STORE_DIR");
        }
    }
}
