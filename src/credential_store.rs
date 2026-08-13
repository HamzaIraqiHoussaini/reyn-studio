//! Native secure storage for the revocable preview-session token.
//!
//! Credentials are never written to settings or project files. Official macOS,
//! Windows, and Linux builds use their platform credential vault; other targets
//! fail closed without a plaintext fallback.

#[cfg(target_os = "windows")]
use zeroize::Zeroize;

const SERVICE: &str = "com.reyn.studio.preview-access";
const ACCOUNT: &str = "session-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    Unavailable(String),
    #[cfg(target_os = "windows")]
    Malformed,
}

pub trait SessionCredentialStore {
    fn load(&self) -> Result<Option<Vec<u8>>, StoreError>;
    fn save(&self, secret: &[u8]) -> Result<(), StoreError>;
    fn delete(&self) -> Result<(), StoreError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeSessionCredentialStore;

#[cfg(target_os = "macos")]
impl NativeSessionCredentialStore {
    fn query() -> security_framework::passwords::PasswordOptions {
        let mut options =
            security_framework::passwords::PasswordOptions::new_generic_password(SERVICE, ACCOUNT);
        options.set_access_synchronized(Some(false));
        options.use_protected_keychain();
        options
    }
}

#[cfg(target_os = "macos")]
impl SessionCredentialStore for NativeSessionCredentialStore {
    fn load(&self) -> Result<Option<Vec<u8>>, StoreError> {
        match security_framework::passwords::generic_password(Self::query()) {
            Ok(secret) => Ok(Some(secret)),
            Err(error) if error.code() == -25300 => Ok(None),
            Err(error) => Err(StoreError::Unavailable(format!(
                "macOS Keychain read failed (status {})",
                error.code()
            ))),
        }
    }

    fn save(&self, secret: &[u8]) -> Result<(), StoreError> {
        let mut options = Self::query();
        options.set_label("Reyn Studio preview session");
        options.set_description(
            "Revocable Reyn Studio preview session token; credentials are not stored",
        );
        security_framework::passwords::set_generic_password_options(secret, options).map_err(
            |error| {
                StoreError::Unavailable(format!(
                    "macOS Keychain write failed (status {})",
                    error.code()
                ))
            },
        )
    }

    fn delete(&self) -> Result<(), StoreError> {
        match security_framework::passwords::delete_generic_password_options(Self::query()) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == -25300 => Ok(()),
            Err(error) => Err(StoreError::Unavailable(format!(
                "macOS Keychain delete failed (status {})",
                error.code()
            ))),
        }
    }
}

#[cfg(target_os = "windows")]
impl SessionCredentialStore for NativeSessionCredentialStore {
    fn load(&self) -> Result<Option<Vec<u8>>, StoreError> {
        use std::{ffi::c_void, ptr, slice};
        use windows_sys::Win32::{
            Foundation::{GetLastError, ERROR_NOT_FOUND},
            Security::Credentials::{CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC},
        };

        let target = wide(SERVICE);
        let mut credential: *mut CREDENTIALW = ptr::null_mut();
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            return if code == ERROR_NOT_FOUND {
                Ok(None)
            } else {
                Err(StoreError::Unavailable(format!(
                    "Windows Credential Manager read failed (status {code})"
                )))
            };
        }
        if credential.is_null() {
            return Err(StoreError::Malformed);
        }
        let secret = unsafe {
            let record = &*credential;
            if record.CredentialBlobSize == 0 || record.CredentialBlob.is_null() {
                CredFree(credential.cast::<c_void>());
                return Err(StoreError::Malformed);
            }
            let value =
                slice::from_raw_parts(record.CredentialBlob, record.CredentialBlobSize as usize)
                    .to_vec();
            CredFree(credential.cast::<c_void>());
            value
        };
        Ok(Some(secret))
    }

    fn save(&self, secret: &[u8]) -> Result<(), StoreError> {
        use std::mem;
        use windows_sys::Win32::Security::Credentials::{
            CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
        };

        let mut target = wide(SERVICE);
        let mut account = wide(ACCOUNT);
        let mut secret = secret.to_vec();
        let mut credential: CREDENTIALW = unsafe { mem::zeroed() };
        credential.Type = CRED_TYPE_GENERIC;
        credential.TargetName = target.as_mut_ptr();
        credential.CredentialBlobSize =
            secret.len().try_into().map_err(|_| StoreError::Malformed)?;
        credential.CredentialBlob = secret.as_mut_ptr();
        credential.Persist = CRED_PERSIST_LOCAL_MACHINE;
        credential.UserName = account.as_mut_ptr();
        let ok = unsafe { CredWriteW(&credential, 0) };
        secret.zeroize();
        if ok == 0 {
            let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            return Err(StoreError::Unavailable(format!(
                "Windows Credential Manager write failed (status {code})"
            )));
        }
        Ok(())
    }

    fn delete(&self) -> Result<(), StoreError> {
        use windows_sys::Win32::{
            Foundation::{GetLastError, ERROR_NOT_FOUND},
            Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC},
        };

        let target = wide(SERVICE);
        let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok != 0 {
            return Ok(());
        }
        let code = unsafe { GetLastError() };
        if code == ERROR_NOT_FOUND {
            Ok(())
        } else {
            Err(StoreError::Unavailable(format!(
                "Windows Credential Manager delete failed (status {code})"
            )))
        }
    }
}

#[cfg(target_os = "windows")]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "linux")]
impl NativeSessionCredentialStore {
    fn entry() -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(SERVICE, ACCOUNT).map_err(|error| {
            StoreError::Unavailable(format!("Linux Secret Service entry failed: {error}"))
        })
    }
}

#[cfg(target_os = "linux")]
impl SessionCredentialStore for NativeSessionCredentialStore {
    fn load(&self) -> Result<Option<Vec<u8>>, StoreError> {
        match Self::entry()?.get_secret() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(StoreError::Unavailable(format!(
                "Linux Secret Service read failed: {error}"
            ))),
        }
    }

    fn save(&self, secret: &[u8]) -> Result<(), StoreError> {
        Self::entry()?.set_secret(secret).map_err(|error| {
            StoreError::Unavailable(format!("Linux Secret Service write failed: {error}"))
        })
    }

    fn delete(&self) -> Result<(), StoreError> {
        match Self::entry()?.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(StoreError::Unavailable(format!(
                "Linux Secret Service delete failed: {error}"
            ))),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl SessionCredentialStore for NativeSessionCredentialStore {
    fn load(&self) -> Result<Option<Vec<u8>>, StoreError> {
        Err(StoreError::Unavailable(
            "No native credential store is available on this platform".into(),
        ))
    }

    fn save(&self, _secret: &[u8]) -> Result<(), StoreError> {
        Err(StoreError::Unavailable(
            "No native credential store is available on this platform".into(),
        ))
    }

    fn delete(&self) -> Result<(), StoreError> {
        Ok(())
    }
}
