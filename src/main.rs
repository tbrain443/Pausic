#![cfg_attr(not(test), windows_subsystem = "windows")]
#![allow(non_snake_case, reason = "Windows executable name: Pausic.exe")]

mod media;
mod mic_monitor;
mod settings;
mod tray;
mod win32;

#[cfg(test)]
mod checks;

use windows::{
    Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
        System::Threading::CreateMutexW,
    },
    core::{Result, w},
};

fn run() -> Result<()> {
    // Holding the named kernel object enforces one instance per interactive Windows session.
    let _instance = win32::Handle(unsafe { CreateMutexW(None, false, w!("Local\\Pausic"))? });
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(());
    }
    tray::run()
}

fn main() {
    if let Err(error) = run() {
        tray::show_error(&error.to_string());
    }
}
