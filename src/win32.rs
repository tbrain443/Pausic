use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE},
        System::{
            Registry::*,
            WinRT::{RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize},
        },
    },
    core::{PCWSTR, Result},
};

pub struct Apartment;

impl Apartment {
    pub fn new() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        Ok(Self)
    }

    pub fn ui() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_SINGLETHREADED)?;
        }
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

pub fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

pub struct Handle(pub HANDLE);

// Kernel events/mutexes are thread-safe; ownership/Arc keeps the handle alive during every wait.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub struct Key(pub HKEY);

impl Key {
    pub fn open(path: &str) -> Result<Option<Self>> {
        Self::open_under(HKEY_CURRENT_USER, &wide(path))
    }

    fn open_under(parent: HKEY, name: &[u16]) -> Result<Option<Self>> {
        let mut key = HKEY::default();
        // All callers supply a terminated UTF-16 name, valid for the duration of this call.
        let status =
            unsafe { RegOpenKeyExW(parent, PCWSTR(name.as_ptr()), None, KEY_READ, &mut key) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        status.ok()?;
        Ok(Some(Self(key)))
    }

    pub fn create(path: &str) -> Result<Self> {
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(wide(path).as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                None,
                &mut key,
                None,
            )
            .ok()?;
        }
        Ok(Self(key))
    }

    pub fn value(&self, name: &str) -> Result<Option<(REG_VALUE_TYPE, Vec<u8>)>> {
        let name = wide(name);
        let mut kind = REG_VALUE_TYPE::default();
        let mut size = 0;
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                None,
                Some(&mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        status.ok()?;
        if size > 65536 {
            return Ok(None);
        }
        let mut data = vec![0; size as usize];
        let status = unsafe {
            RegQueryValueExW(
                self.0,
                PCWSTR(name.as_ptr()),
                None,
                Some(&mut kind),
                Some(data.as_mut_ptr()),
                Some(&mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        status.ok()?; // A concurrent change is an error, not a missing setting.
        data.truncate(size as usize);
        Ok(Some((kind, data)))
    }

    pub fn set(&self, name: &str, kind: REG_VALUE_TYPE, bytes: &[u8]) -> Result<()> {
        unsafe { RegSetValueExW(self.0, PCWSTR(wide(name).as_ptr()), None, kind, Some(bytes)).ok() }
    }

    pub fn delete_value(&self, name: &str) -> Result<()> {
        let status = unsafe { RegDeleteValueW(self.0, PCWSTR(wide(name).as_ptr())) };
        if status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            status.ok()
        }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
