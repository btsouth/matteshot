//! Cross-process state serialization and crash-safe file replacement.

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use anyhow::{bail, Context, Result};
use windows::core::HSTRING;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};
use windows::Win32::System::Threading::{
    CreateMutexW, ReleaseMutex, WaitForSingleObject, INFINITE,
};

pub struct NamedMutex {
    handle: HANDLE,
}

pub struct ProcessMutex {
    handle: HANDLE,
}

impl Drop for ProcessMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Atomically create or open a named process-lifetime marker. Unlike the
/// short state locks below, this mutex is not acquired; its existence alone
/// owns the single-resident slot and cannot deadlock after a crash.
pub fn try_process_mutex(name: &str) -> Result<Option<ProcessMutex>> {
    let name = HSTRING::from(name);
    let handle = unsafe { CreateMutexW(None, false, &name) }.context("create resident mutex")?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        Ok(None)
    } else {
        Ok(Some(ProcessMutex { handle }))
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

pub fn lock(name: &str) -> Result<NamedMutex> {
    let name = HSTRING::from(name);
    let handle = unsafe { CreateMutexW(None, false, &name) }.context("create state mutex")?;
    let waited = unsafe { WaitForSingleObject(handle, INFINITE) };
    if waited != WAIT_OBJECT_0 && waited != WAIT_ABANDONED {
        unsafe {
            let _ = CloseHandle(handle);
        }
        bail!("wait for state mutex failed ({})", waited.0);
    }
    Ok(NamedMutex { handle })
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes).context("write temporary state")?;
    let temporary_wide: Vec<u16> = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let replaced = unsafe {
        MoveFileExW(
            windows::core::PCWSTR(temporary_wide.as_ptr()),
            windows::core::PCWSTR(path_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = replaced {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).context("replace state file");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_existing_state() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "matteshot-state-{}-{unique}.json",
            std::process::id()
        ));
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn process_mutex_allows_only_one_owner() {
        let name = format!(
            "Local\\Matteshot.Process.Test.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let first = try_process_mutex(&name).unwrap().unwrap();
        assert!(try_process_mutex(&name).unwrap().is_none());
        drop(first);
        assert!(try_process_mutex(&name).unwrap().is_some());
    }
}
