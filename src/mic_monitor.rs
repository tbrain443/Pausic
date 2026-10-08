use crate::win32::{Apartment, Handle};
use std::{
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::WAIT_TIMEOUT,
        Media::Audio::{
            AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionManager2,
            IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
        },
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance},
            Threading::WaitForMultipleObjects,
        },
    },
    core::Result,
};

const RELEASE_DEBOUNCE: Duration = Duration::from_millis(250);
const RESCAN_MS: u32 = 100;

pub struct Monitor {
    stop: Arc<Handle>,
    restart: Arc<Handle>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start(changed: impl Fn(bool) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(Handle::event(true)?);
        let restart = Arc::new(Handle::event(false)?);
        let signals = (stop.clone(), restart.clone());
        let thread = thread::Builder::new()
            .name("Pausic microphone".into())
            .spawn(move || {
                let (stop, restart) = signals;
                let mut transition = Transition::default();
                let mut apartment = None;
                loop {
                    if apartment.is_none() {
                        apartment = Apartment::new().ok();
                    }
                    if apartment.is_some() {
                        match read_in_use() {
                            Ok(active) => {
                                if let Some(active) = transition.observe(active, Instant::now()) {
                                    changed(active);
                                }
                            }
                            // An unknown state must never resume media. Require a new, complete
                            // release debounce after recovery from a device/service error.
                            Err(_) => transition.since = Instant::now(),
                        }
                    }
                    match unsafe { WaitForMultipleObjects(&[stop.0, restart.0], false, RESCAN_MS) }
                        .0
                    {
                        0 => break,
                        1 => {} // Next scan recreates the device/session snapshot after resume.
                        n if n == WAIT_TIMEOUT.0 => {}
                        _ => break,
                    }
                }
            })?;
        Ok(Self {
            stop,
            restart,
            thread: Some(thread),
        })
    }

    pub fn restart(&self) {
        self.restart.signal();
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Transition {
    stable: Option<bool>,
    candidate: bool,
    since: Instant,
}

impl Default for Transition {
    fn default() -> Self {
        Self {
            stable: None,
            candidate: false,
            since: Instant::now(),
        }
    }
}

impl Transition {
    fn observe(&mut self, active: bool, now: Instant) -> Option<bool> {
        self.stable.get_or_insert(active); // Preserve silent startup baseline.
        if active != self.candidate {
            self.candidate = active;
            self.since = now;
        }
        if Some(active) != self.stable
            && (active || now.duration_since(self.since) >= RELEASE_DEBOUNCE)
        {
            self.stable = Some(active);
            Some(active)
        } else {
            None
        }
    }
}

pub fn read_in_use() -> Result<bool> {
    // Query capture endpoints, never render/loopback endpoints. No recording stream is opened.
    // Recreate the snapshot each scan to discover new sessions and hot-plugged devices without
    // retaining a potentially stale session enumerator or device after an audio-service restart.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        let mut failure = None;
        for index in 0..devices.GetCount()? {
            let result = (|| -> Result<bool> {
                let device = devices.Item(index)?;
                let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
                let sessions = manager.GetSessionEnumerator()?;
                for index in 0..sessions.GetCount()? {
                    if sessions.GetSession(index)?.GetState()? == AudioSessionStateActive {
                        return Ok(true);
                    }
                }
                Ok(false)
            })();
            match result {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(error) => failure = Some(error),
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediate_pause_debounced_release_and_silent_startup() {
        let mut state = Transition::default();
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        assert_eq!(state.observe(false, at(0)), None);
        assert_eq!(state.observe(true, at(1)), Some(true));
        assert_eq!(state.observe(true, at(2)), None); // Overlapping users stay active.
        assert_eq!(state.observe(false, at(10)), None);
        assert_eq!(state.observe(false, at(259)), None);
        assert_eq!(state.observe(true, at(260)), None); // Rapid reopen cancels release.
        assert_eq!(state.observe(false, at(300)), None);
        assert_eq!(state.observe(false, at(550)), Some(false));
        let mut active_start = Transition::default();
        assert_eq!(active_start.observe(true, at(0)), None);
        assert_eq!(active_start.observe(false, at(1)), None);
        assert_eq!(active_start.observe(false, at(251)), Some(false));
    }
}
