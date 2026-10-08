use crate::{media, mic_monitor, settings, win32::wide};
use std::{
    cell::{Cell, RefCell},
    mem::size_of,
};
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::{PCWSTR, Result, w},
};

pub const MIC_CHANGED: u32 = WM_APP + 1;
pub const MEDIA_STOPPED: u32 = WM_APP + 2;
const TRAY_EVENT: u32 = WM_APP + 3;
const ENABLED: usize = 1;
const AUTORUN: usize = 2;
const ABOUT: usize = 3;
const EXIT: usize = 4;

struct Menu(HMENU);
impl Drop for Menu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

struct Icon(HICON);
impl Drop for Icon {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.0);
        }
    }
}

struct TrayIcon {
    hwnd: HWND,
    icon: Icon,
}

impl TrayIcon {
    fn new(hwnd: HWND) -> Result<Self> {
        // Load the resource variant matching the tray size at this monitor's DPI.
        let icon = unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None)?.0);
            let dpi = GetDpiForWindow(hwnd);
            HICON(
                LoadImageW(
                    Some(instance),
                    w!("PAUSIC_ICON"),
                    IMAGE_ICON,
                    GetSystemMetricsForDpi(SM_CXSMICON, dpi),
                    GetSystemMetricsForDpi(SM_CYSMICON, dpi),
                    LR_DEFAULTCOLOR,
                )?
                .0,
            )
        };
        Ok(Self {
            hwnd,
            icon: Icon(icon),
        })
    }

    fn data(&self, enabled: bool) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: TRAY_EVENT,
            hIcon: self.icon.0,
            ..Default::default()
        };
        let tip = wide(if enabled {
            "Pausic"
        } else {
            "Pausic (disabled)"
        });
        data.szTip[..tip.len()].copy_from_slice(&tip);
        data
    }

    fn add(&self, enabled: bool) {
        let mut data = self.data(enabled);
        unsafe {
            // Explorer may not yet exist at logon. TaskbarCreated retries registration.
            let _ = Shell_NotifyIconW(NIM_ADD, &data);
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
    }

    fn update(&self, enabled: bool) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.data(enabled));
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.data(false));
        }
    }
}

pub struct App {
    enabled: Cell<bool>,
    exiting: Cell<bool>,
    taskbar_created: u32,
    monitor: RefCell<Option<mic_monitor::Monitor>>,
    media: RefCell<Option<media::Worker>>,
    tray: RefCell<Option<TrayIcon>>,
}

impl App {
    fn new() -> Self {
        Self {
            enabled: Cell::new(true),
            exiting: Cell::new(false),
            taskbar_created: unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) },
            monitor: RefCell::new(None),
            media: RefCell::new(None),
            tray: RefCell::new(None),
        }
    }

    fn start(&self, hwnd: HWND) -> Result<()> {
        let (enabled, first_run) = settings::load().unwrap_or_else(|error| {
            show_error(&error.to_string());
            (true, false)
        });
        self.enabled.set(enabled);
        if first_run {
            if let Err(error) = settings::set_autorun(true).and_then(|_| settings::save(enabled)) {
                show_error(&error.to_string());
            }
        }
        let tray = TrayIcon::new(hwnd)?;
        tray.add(enabled);
        self.tray.replace(Some(tray));

        // Only PostMessage crosses threads. HWND stays alive until both workers have joined.
        let target = hwnd.0 as usize;
        let worker = media::Worker::start(move || post(target, MEDIA_STOPPED, false))?;
        self.media.replace(Some(worker));
        let monitor = mic_monitor::Monitor::start(move |active| post(target, MIC_CHANGED, active))?;
        self.monitor.replace(Some(monitor));
        Ok(())
    }

    fn stop(&self) {
        self.exiting.set(true);
        // Preserve Exit ordering: media restoration, then watcher, then icon/window resources.
        self.media.borrow_mut().take();
        self.monitor.borrow_mut().take();
        self.tray.borrow_mut().take();
    }

    fn command(&self, hwnd: HWND, command: usize) {
        if self.exiting.get() {
            return;
        }
        match command {
            ENABLED => {
                let enabled = !self.enabled.get();
                if let Err(error) = settings::save(enabled) {
                    show_error(&error.to_string());
                    return;
                }
                self.enabled.set(enabled);
                if let Some(tray) = self.tray.borrow().as_ref() {
                    tray.update(enabled);
                }
                if !enabled {
                    if let Some(media) = self.media.borrow().as_ref() {
                        media.request(false);
                    }
                }
                // Enabling waits for the next global microphone transition.
            }
            AUTORUN => {
                if let Err(error) =
                    settings::autorun_enabled().and_then(|active| settings::set_autorun(!active))
                {
                    show_error(&error.to_string());
                }
            }
            ABOUT => {
                if let Err(error) = self.about(hwnd) {
                    show_error(&error.to_string());
                }
            }
            EXIT => self.exit(),
            _ => {}
        }
    }

    fn exit(&self) {
        if self.exiting.replace(true) {
            return;
        }
        if let Some(media) = self.media.borrow().as_ref() {
            media.exit();
        } else {
            unsafe {
                PostQuitMessage(0);
            }
        }
    }

    fn about(&self, hwnd: HWND) -> Result<()> {
        let result = unsafe {
            DialogBoxParamW(
                Some(HINSTANCE(GetModuleHandleW(None)?.0)),
                w!("PAUSIC_ABOUT"),
                Some(hwnd),
                Some(about_proc),
                LPARAM(0),
            )
        };
        if result == -1 {
            Err(windows::core::Error::from_thread())
        } else {
            Ok(())
        }
    }

    fn popup(&self, hwnd: HWND) -> Result<()> {
        if self.exiting.get() {
            return Ok(());
        }
        let menu = Menu(unsafe { CreatePopupMenu()? });
        let checked = |value| if value { MF_CHECKED } else { MF_UNCHECKED };
        let autorun = settings::autorun_enabled();
        let autorun_flags = match autorun {
            Ok(enabled) => checked(enabled),
            Err(_) => MF_GRAYED,
        };
        unsafe {
            AppendMenuW(
                menu.0,
                MF_STRING | checked(self.enabled.get()),
                ENABLED,
                w!("Enabled"),
            )?;
            AppendMenuW(
                menu.0,
                MF_STRING | autorun_flags,
                AUTORUN,
                w!("Start with Windows"),
            )?;
            AppendMenuW(menu.0, MF_SEPARATOR, 0, None)?;
            AppendMenuW(menu.0, MF_STRING, ABOUT, w!("About Pausic"))?;
            AppendMenuW(menu.0, MF_STRING, EXIT, w!("Exit"))?;
            let mut point = POINT::default();
            GetCursorPos(&mut point)?;
            let _ = SetForegroundWindow(hwnd);
            // No mutable App borrow spans this nested Windows message loop.
            let command = TrackPopupMenu(
                menu.0,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                hwnd,
                None,
            )
            .0;
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            if command != 0 {
                let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(command as usize), LPARAM(0));
            }
        }
        Ok(())
    }
}

// The dialog resource lays out the icon, centered heading, and Close button.
unsafe extern "system" fn about_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    _lparam: LPARAM,
) -> isize {
    unsafe {
        match message {
            WM_INITDIALOG => {
                let title = wide(concat!("Pausic ", env!("CARGO_PKG_VERSION")));
                let _ = SetDlgItemTextW(hwnd, 102, PCWSTR(title.as_ptr()));
                1
            }
            WM_CLOSE => {
                let _ = EndDialog(hwnd, 0);
                1
            }
            WM_COMMAND if wparam.0 & 0xffff == IDCANCEL.0 as usize => {
                let _ = EndDialog(hwnd, 0);
                1
            }
            _ => 0,
        }
    }
}

fn post(hwnd: usize, message: u32, active: bool) {
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut _)),
            message,
            WPARAM(usize::from(active)),
            LPARAM(0),
        );
    }
}

// The boxed App outlives Window. Windows messages are dispatched only on the owning UI thread.
// We use a shared reference and interior mutability: menus/message boxes may reenter this procedure.
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if message == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App;
        if let Some(app) = pointer.as_ref() {
            if message == app.taskbar_created && app.taskbar_created != 0 {
                if let Some(tray) = app.tray.borrow().as_ref() {
                    tray.add(app.enabled.get());
                }
                return LRESULT(0);
            }
            match message {
                TRAY_EVENT if (lparam.0 as u32 & 0xffff) == WM_CONTEXTMENU => {
                    let _ = app.popup(hwnd);
                }
                WM_COMMAND => app.command(hwnd, wparam.0 & 0xffff),
                MIC_CHANGED if !app.exiting.get() && app.enabled.get() => {
                    if let Some(media) = app.media.borrow().as_ref() {
                        media.request(wparam.0 != 0);
                    }
                }
                MEDIA_STOPPED => PostQuitMessage(0),
                WM_CLOSE => app.exit(),
                WM_ENDSESSION if wparam.0 != 0 => app.exit(),
                WM_POWERBROADCAST
                    if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize
                        || wparam.0 == PBT_APMRESUMESUSPEND as usize =>
                {
                    if let Some(monitor) = app.monitor.borrow().as_ref() {
                        monitor.restart();
                    }
                }
                _ => return DefWindowProcW(hwnd, message, wparam, lparam),
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

struct Window<'a> {
    hwnd: HWND,
    instance: HINSTANCE,
    app: &'a App,
}
impl Drop for Window<'_> {
    fn drop(&mut self) {
        self.app.stop();
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(w!("Pausic.TrayWindow"), Some(self.instance));
        }
    }
}

impl<'a> Window<'a> {
    fn new(app: &'a App) -> Result<Self> {
        let instance = HINSTANCE(unsafe { GetModuleHandleW(None)? }.0);
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("Pausic.TrayWindow"),
            ..Default::default()
        };
        unsafe {
            if RegisterClassW(&class) == 0 {
                return Err(windows::core::Error::from_thread());
            }
            // Hidden top-level window receives power and Explorer broadcasts; HWND_MESSAGE would not.
            let hwnd = match CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                w!("Pausic"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                Some((app as *const App).cast()),
            ) {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    let _ = UnregisterClassW(class.lpszClassName, Some(instance));
                    return Err(error);
                }
            };
            Ok(Window {
                hwnd,
                instance,
                app,
            })
        }
    }
}

fn message_loop() -> Result<()> {
    unsafe {
        let mut message = MSG::default();
        loop {
            match GetMessageW(&mut message, None, 0, 0).0 {
                -1 => return Err(windows::core::Error::from_thread()),
                0 => break,
                _ => {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
    }
    Ok(())
}

pub fn run() -> Result<()> {
    let _apartment = crate::win32::Apartment::ui()?;
    let app = Box::new(App::new());
    let window = Window::new(&app)?;
    app.start(window.hwnd)?;
    message_loop()
}

pub fn show_error(text: &str) {
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide(text).as_ptr()),
            w!("Pausic"),
            MB_OK | MB_ICONWARNING,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win32::Key;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_VALUE_TYPE, RegDeleteKeyW};

    #[test]
    #[ignore = "creates a temporary native window to verify the embedded DPI manifest"]
    fn native_window_uses_per_monitor_dpi() -> Result<()> {
        use windows::Win32::UI::HiDpi::{
            AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            GetWindowDpiAwarenessContext,
        };
        let app = App::new();
        let window = Window::new(&app)?;
        assert!(
            unsafe {
                AreDpiAwarenessContextsEqual(
                    GetWindowDpiAwarenessContext(window.hwnd),
                    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                )
                .as_bool()
            },
            "Windows must render the UI at the monitor DPI instead of stretching a bitmap"
        );
        Ok(())
    }

    struct SavedValue {
        path: &'static str,
        name: &'static str,
        existed: bool,
        value: Option<(REG_VALUE_TYPE, Vec<u8>)>,
    }
    impl SavedValue {
        fn new(path: &'static str, name: &'static str) -> Result<Self> {
            let key = Key::open(path)?;
            let value = key
                .as_ref()
                .map(|key| key.value(name))
                .transpose()?
                .flatten();
            Ok(Self {
                path,
                name,
                existed: key.is_some(),
                value,
            })
        }
    }
    impl Drop for SavedValue {
        fn drop(&mut self) {
            if let Ok(key) = Key::create(self.path) {
                if let Some((kind, bytes)) = &self.value {
                    let _ = key.set(self.name, *kind, bytes);
                } else {
                    let _ = key.delete_value(self.name);
                }
            }
            if !self.existed {
                unsafe {
                    let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide(self.path).as_ptr()));
                }
            }
        }
    }

    #[test]
    #[ignore = "native UI check; temporarily changes/restores Pausic settings and opens About"]
    fn native_tray_controls_and_exit() -> Result<()> {
        let _apartment = crate::win32::Apartment::ui()?;
        let _settings = SavedValue::new(settings::SETTINGS, "Enabled")?;
        let _autorun = SavedValue::new(settings::RUN, "Pausic")?;
        let app = Box::new(App::new());
        let window = Window::new(&app)?;
        app.start(window.hwnd)?;
        let identifier = NOTIFYICONIDENTIFIER {
            cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: window.hwnd,
            uID: 1,
            ..Default::default()
        };
        assert!(
            unsafe { Shell_NotifyIconGetRect(&identifier) }.is_ok(),
            "tray registered in Explorer"
        );
        let initial = app.enabled.get();
        unsafe {
            SendMessageW(window.hwnd, WM_COMMAND, Some(WPARAM(ENABLED)), None);
            assert_eq!(settings::load()?.0, !initial);
            SendMessageW(window.hwnd, WM_COMMAND, Some(WPARAM(ENABLED)), None);
            assert_eq!(settings::load()?.0, initial);
            settings::set_autorun(false)?;
            SendMessageW(window.hwnd, WM_COMMAND, Some(WPARAM(AUTORUN)), None);
            assert!(settings::autorun_enabled()?);
            SendMessageW(window.hwnd, WM_COMMAND, Some(WPARAM(AUTORUN)), None);
            assert!(!settings::autorun_enabled()?);
            SendMessageW(
                window.hwnd,
                WM_POWERBROADCAST,
                Some(WPARAM(PBT_APMRESUMEAUTOMATIC as usize)),
                None,
            );
            SendMessageW(window.hwnd, app.taskbar_created, None, None);
            assert!(Shell_NotifyIconGetRect(&identifier).is_ok());
            println!(
                "Tray registration, Enabled, autorun and recovery callbacks passed. Close About to finish."
            );
            SendMessageW(window.hwnd, WM_COMMAND, Some(WPARAM(ABOUT)), None);
            SendMessageW(window.hwnd, WM_COMMAND, Some(WPARAM(EXIT)), None);
        }
        message_loop()?;
        drop(window);
        assert!(
            app.monitor.borrow().is_none()
                && app.media.borrow().is_none()
                && app.tray.borrow().is_none()
        );
        assert!(unsafe { Shell_NotifyIconGetRect(&identifier) }.is_err());
        Ok(())
    }
}
