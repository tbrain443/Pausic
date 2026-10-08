use crate::win32::{Key, wide};
use windows::{
    Win32::System::Registry::{REG_DWORD, REG_SZ},
    core::Result,
};

pub const SETTINGS: &str = r"Software\Pausic";
pub const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn load() -> Result<(bool, bool)> {
    let Some(key) = Key::open(SETTINGS)? else {
        return Ok((true, true));
    };
    let enabled = key
        .value("Enabled")?
        .and_then(|(kind, bytes)| {
            if kind == REG_DWORD {
                bytes.try_into().ok().map(u32::from_le_bytes)
            } else {
                None
            }
        })
        .unwrap_or(1)
        != 0;
    Ok((enabled, false))
}

pub fn save(enabled: bool) -> Result<()> {
    Key::create(SETTINGS)?.set("Enabled", REG_DWORD, &u32::from(enabled).to_le_bytes())
}

fn command() -> Result<Vec<u16>> {
    let path = std::env::current_exe().map_err(|e| {
        windows::core::Error::from(windows::core::HRESULT::from_win32(
            e.raw_os_error().unwrap_or(1) as u32,
        ))
    })?;
    let mut result = vec![b'"' as u16];
    let path = wide(path);
    result.extend_from_slice(&path[..path.len() - 1]);
    result.extend([b'"' as u16, 0]);
    Ok(result)
}

pub fn autorun_enabled() -> Result<bool> {
    let Some(key) = Key::open(RUN)? else {
        return Ok(false);
    };
    let Some((kind, bytes)) = key.value("Pausic")? else {
        return Ok(false);
    };
    if kind != REG_SZ || bytes.len() % 2 != 0 {
        return Ok(false);
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&units).to_lowercase()
        == String::from_utf16_lossy(&command()?).to_lowercase())
}

pub fn set_autorun(enabled: bool) -> Result<()> {
    let key = Key::create(RUN)?;
    if enabled {
        let bytes: Vec<u8> = command()?.iter().flat_map(|u| u.to_le_bytes()).collect();
        key.set("Pausic", REG_SZ, &bytes)
    } else {
        key.delete_value("Pausic")
    }
}
