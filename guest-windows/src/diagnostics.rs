//! Optional file metadata, sampled once when the worker starts.
#[cfg(windows)]
pub(crate) fn driver_version() -> Result<String, String> {
    use winapi::um::sysinfoapi::GetSystemDirectoryW;
    use winapi::um::winver::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    let mut directory = vec![0u16; 32768];
    // SAFETY: directory owns the writable capacity passed to Windows.
    let length =
        unsafe { GetSystemDirectoryW(directory.as_mut_ptr(), directory.len() as u32) } as usize;
    if length == 0 || length >= directory.len() {
        return Err("system directory unavailable".to_owned());
    }
    directory.truncate(length);
    directory.extend("\\drivers\\viomem.sys\0".encode_utf16());
    // SAFETY: the filename is terminated; the unused handle output is optional.
    let size = unsafe { GetFileVersionInfoSizeW(directory.as_ptr(), std::ptr::null_mut()) };
    if size == 0 || size > 1024 * 1024 {
        return Err(format!(
            "viomem.sys version size unavailable: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut storage = vec![0u32; (size as usize).div_ceil(4)];
    // SAFETY: storage is aligned and owns at least size writable bytes.
    if unsafe { GetFileVersionInfoW(directory.as_ptr(), 0, size, storage.as_mut_ptr().cast()) } == 0
    {
        return Err(format!(
            "read viomem.sys version: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut pointer = std::ptr::null_mut();
    let mut length = 0;
    let root = [b'\\' as u16, 0];
    // SAFETY: storage is a complete version block; outputs point into it.
    if unsafe {
        VerQueryValueW(
            storage.as_ptr().cast(),
            root.as_ptr(),
            &mut pointer,
            &mut length,
        )
    } == 0
    {
        return Err("viomem.sys has no fixed version information".to_owned());
    }
    let start = storage.as_ptr() as usize;
    let address = pointer as usize;
    if length < 16 || address < start || address.saturating_add(16) > start + size as usize {
        return Err("viomem.sys fixed version information is invalid".to_owned());
    }
    // SAFETY: the checked range owns the four DWORDs of the fixed-info prefix.
    let words = unsafe { std::ptr::read_unaligned(pointer.cast::<[u32; 4]>()) };
    if words[0] != 0xfeef04bd {
        return Err("viomem.sys fixed version signature is invalid".to_owned());
    }
    Ok(format!(
        "{}.{}.{}.{}",
        words[2] >> 16,
        words[2] & 0xffff,
        words[3] >> 16,
        words[3] & 0xffff
    ))
}

#[cfg(not(windows))]
pub(crate) fn driver_version() -> Result<String, String> {
    Err("Windows driver file metadata is unavailable on this platform".to_owned())
}
