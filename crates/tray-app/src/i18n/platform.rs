#[cfg(target_os = "macos")]
pub fn preferred_languages() -> Vec<String> {
    tm_macos::locale::preferred_languages()
}

#[cfg(windows)]
pub fn preferred_languages() -> Vec<String> {
    use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};

    // The returned MULTI_SZ is sized in UTF-16 code units, including terminators.
    let mut count = 0;
    let mut length = 0;
    // SAFETY: A null buffer queries its required length; both output pointers are valid.
    let queried = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            std::ptr::null_mut(),
            &mut length,
        )
    };
    if queried == 0 || length == 0 || length > 65_536 {
        return Vec::new();
    }
    let mut buffer = vec![0u16; length as usize];
    // SAFETY: The buffer has the number of UTF-16 units supplied to the API.
    let read = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            buffer.as_mut_ptr(),
            &mut length,
        )
    };
    if read == 0 {
        return Vec::new();
    }
    buffer
        .split(|unit| *unit == 0)
        .take_while(|name| !name.is_empty())
        .filter_map(|name| String::from_utf16(name).ok())
        .collect()
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn preferred_languages() -> Vec<String> {
    Vec::new()
}
