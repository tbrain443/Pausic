use crate::win32::Apartment;
use std::{
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::PROPERTYKEY,
        Media::Audio::*,
        System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree},
    },
    core::{BOOL, GUID, Interface, PCWSTR, Ref, Result, implement},
};

const RELEASE_DEBOUNCE: Duration = Duration::from_millis(250);
const RECOVERY_DELAY: Duration = Duration::from_secs(1);

pub struct Monitor {
    send: Sender<Event>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start(changed: impl Fn(bool) + Send + 'static) -> Result<Self> {
        let (send, receive) = mpsc::channel();
        let callbacks = send.clone();
        let thread = thread::Builder::new()
            .name("Pausic microphone".into())
            .spawn(move || run(receive, callbacks, changed))?;
        Ok(Self {
            send,
            thread: Some(thread),
        })
    }

    pub fn restart(&self) {
        let _ = self.send.send(Event::Restart);
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.send.send(Event::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum Event {
    Stop,
    Restart,
    Rebuild(u64),
    State(u64),
    Session(u64, usize, CaptureSession),
}

// Session notifications and our worker share the MTA; no apartment boundary is crossed.
// Keep the provided session alive until the worker can register its state listener.
struct CaptureSession(IAudioSessionControl);
unsafe impl Send for CaptureSession {}

fn run(receive: Receiver<Event>, send: Sender<Event>, changed: impl Fn(bool)) {
    let mut apartment = None;
    let mut snapshot = None;
    let mut generation = 0;
    let mut transition = Transition::default();
    let mut retry = Some(Instant::now());
    loop {
        if retry.is_some_and(|deadline| Instant::now() >= deadline) {
            // Drop/unregister on the worker, never inside a Core Audio callback.
            snapshot = None;
            generation += 1;
            if apartment.is_none() {
                apartment = Apartment::new().ok();
            }
            if apartment.is_some() {
                snapshot = Snapshot::new(send.clone(), generation).ok();
            }
            retry = snapshot.is_none().then(|| Instant::now() + RECOVERY_DELAY);
        }
        if let Some(current) = &mut snapshot {
            match current.in_use() {
                Ok(active) => {
                    if let Some(active) = transition.observe(active, Instant::now()) {
                        changed(active);
                    }
                }
                Err(_) => {
                    // Unknown activity must not resume media, even across service recovery.
                    transition.since = Instant::now();
                    retry = Some(Instant::now() + RECOVERY_DELAY);
                    snapshot = None;
                }
            }
        } else {
            transition.since = Instant::now();
        }
        let release = (retry.is_none() && transition.stable == Some(true) && !transition.candidate)
            .then_some(transition.since + RELEASE_DEBOUNCE);
        let deadline = retry.or(release);
        let event = match deadline {
            Some(deadline) => {
                match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match receive.recv() {
                Ok(event) => event,
                Err(_) => break,
            },
        };
        match event {
            Event::Stop => break,
            Event::Restart => retry = Some(Instant::now()),
            Event::Rebuild(id) if id == generation => retry = Some(Instant::now()),
            Event::Session(id, endpoint, session) if id == generation => {
                if let Some(endpoint) = snapshot
                    .as_mut()
                    .and_then(|current| current.endpoints.get_mut(endpoint))
                    .and_then(Option::as_mut)
                {
                    if endpoint.add(session.0, send.clone(), generation).is_err() {
                        transition.since = Instant::now();
                        retry = Some(Instant::now() + RECOVERY_DELAY);
                        snapshot = None;
                    }
                }
            }
            Event::State(id) if id == generation => {}
            _ => continue, // Discard callbacks from an unregistered snapshot.
        }
    }
    // Keep the queue and MTA alive until every notification has been unregistered.
    drop(snapshot);
    drop(receive);
    drop(apartment);
}

struct Snapshot {
    devices: IMMDeviceEnumerator,
    listener: IMMNotificationClient,
    endpoints: Vec<Option<Endpoint>>,
    failure: Option<windows_core::Error>,
}

impl Snapshot {
    fn new(send: Sender<Event>, generation: u64) -> Result<Self> {
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let listener = Callbacks {
                send: send.clone(),
                generation,
                endpoint: 0,
            }
            .into();
            devices.RegisterEndpointNotificationCallback(&listener)?;
            let mut snapshot = Self {
                devices,
                listener,
                endpoints: Vec::new(),
                failure: None,
            };
            let devices = snapshot
                .devices
                .EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
            for index in 0..devices.GetCount()? {
                let result = (|| -> Result<Endpoint> {
                    let device = devices.Item(index)?;
                    let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;
                    let listener = Callbacks {
                        send: send.clone(),
                        generation,
                        endpoint: snapshot.endpoints.len(),
                    }
                    .into();
                    manager.RegisterSessionNotification(&listener)?;
                    let mut endpoint = Endpoint {
                        manager,
                        listener,
                        sessions: Vec::new(),
                    };
                    // Register before enumeration. GetCount also enables new-session notifications.
                    let sessions = endpoint.manager.GetSessionEnumerator()?;
                    for index in 0..sessions.GetCount()? {
                        endpoint.add(sessions.GetSession(index)?, send.clone(), generation)?;
                    }
                    Ok(endpoint)
                })();
                match result {
                    Ok(endpoint) => snapshot.endpoints.push(Some(endpoint)),
                    Err(error) => {
                        snapshot.failure = Some(error);
                        snapshot.endpoints.push(None);
                    }
                }
            }
            Ok(snapshot)
        }
    }

    fn in_use(&mut self) -> Result<bool> {
        let mut failure = self.failure.clone();
        let mut active = false;
        for endpoint in self.endpoints.iter_mut().flatten() {
            endpoint
                .sessions
                .retain(|session| match unsafe { session.control.GetState() } {
                    Ok(state) => {
                        active |= state == AudioSessionStateActive;
                        state != AudioSessionStateExpired
                    }
                    Err(error) => {
                        failure = Some(error);
                        true
                    }
                });
        }
        if active {
            Ok(true)
        } else if let Some(error) = failure {
            Err(error)
        } else {
            Ok(false)
        }
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .devices
                .UnregisterEndpointNotificationCallback(&self.listener);
        }
    }
}

struct Endpoint {
    manager: IAudioSessionManager2,
    listener: IAudioSessionNotification,
    sessions: Vec<Session>,
}

impl Endpoint {
    fn add(
        &mut self,
        control: IAudioSessionControl,
        send: Sender<Event>,
        generation: u64,
    ) -> Result<()> {
        unsafe {
            let identifier = control
                .cast::<IAudioSessionControl2>()?
                .GetSessionInstanceIdentifier()?;
            let id = identifier.to_string();
            CoTaskMemFree(Some(identifier.0.cast()));
            let id = id?;
            if self.sessions.iter().any(|session| session.id == id) {
                return Ok(());
            }
            let listener = Callbacks {
                send,
                generation,
                endpoint: 0,
            }
            .into();
            control.RegisterAudioSessionNotification(&listener)?;
            self.sessions.push(Session {
                id,
                control,
                listener,
            });
            Ok(())
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        unsafe {
            let _ = self.manager.UnregisterSessionNotification(&self.listener);
        }
    }
}

struct Session {
    id: String,
    control: IAudioSessionControl,
    listener: IAudioSessionEvents,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .control
                .UnregisterAudioSessionNotification(&self.listener);
        }
    }
}

// Notifications only enqueue work. COM calls and ownership changes happen on our MTA worker.
#[implement(IAudioSessionEvents, IAudioSessionNotification, IMMNotificationClient)]
struct Callbacks {
    send: Sender<Event>,
    generation: u64,
    endpoint: usize,
}

impl Callbacks_Impl {
    fn rebuild(&self) -> Result<()> {
        let _ = self.send.send(Event::Rebuild(self.generation));
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IAudioSessionNotification_Impl for Callbacks_Impl {
    fn OnSessionCreated(&self, session: Ref<IAudioSessionControl>) -> Result<()> {
        let _ = self.send.send(Event::Session(
            self.generation,
            self.endpoint,
            CaptureSession(session.ok()?.clone()),
        ));
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IAudioSessionEvents_Impl for Callbacks_Impl {
    fn OnStateChanged(&self, _: AudioSessionState) -> Result<()> {
        let _ = self.send.send(Event::State(self.generation));
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> Result<()> {
        self.rebuild()
    }
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, _: f32, _: BOOL, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> Result<()> {
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for Callbacks_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> Result<()> {
        self.rebuild()
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> {
        self.rebuild()
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> Result<()> {
        self.rebuild()
    }
    fn OnDefaultDeviceChanged(&self, _: EDataFlow, _: ERole, _: &PCWSTR) -> Result<()> {
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &PROPERTYKEY) -> Result<()> {
        Ok(())
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

#[cfg(test)]
pub fn read_in_use() -> Result<bool> {
    // Query capture endpoints, never render/loopback endpoints. No recording stream is opened.
    // Independent snapshot for real-device test assertions. Production uses subscriptions.
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
