//! Named mutex ownership. A busy mutex is a live owner, never a stale session.
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

pub struct InstanceGuard(HANDLE);

impl InstanceGuard {
    pub fn acquire(name: &str) -> Result<Option<Self>, u32> {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle.is_null() {
                return Err(GetLastError());
            }
            match WaitForSingleObject(handle, 0) {
                WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Some(Self(handle))),
                WAIT_TIMEOUT => {
                    CloseHandle(handle);
                    Ok(None)
                }
                _ => {
                    let err = GetLastError();
                    CloseHandle(handle);
                    Err(err)
                }
            }
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_owner_is_exclusive_and_released_on_drop() {
        let name = format!("Local\\Tramoing-test-{}", std::process::id());
        let guard = InstanceGuard::acquire(&name).unwrap().unwrap();
        let other = name.clone();
        assert!(
            std::thread::spawn(move || InstanceGuard::acquire(&other).unwrap().is_none())
                .join()
                .unwrap()
        );
        drop(guard);
        assert!(InstanceGuard::acquire(&name).unwrap().is_some());
    }
}
