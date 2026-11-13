/// Win32 strings are NUL-terminated; embedded NUL must never silently truncate
/// a service identity, filesystem path, or Event Log insertion string.
pub(crate) fn to_wide(value: &str) -> Result<Vec<u16>, String> {
    if value.contains('\0') {
        return Err("native string contains an embedded NUL".to_owned());
    }
    Ok(value.encode_utf16().chain(Some(0)).collect())
}
