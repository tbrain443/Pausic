use crate::win32::Apartment;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use windows::{
    Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    },
    core::{Error, HRESULT, Result},
};

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct Controller {
    manager: Option<Manager>,
    paused: Vec<Session>,
}

impl Controller {
    fn manager(&mut self) -> Result<&Manager> {
        if self.manager.is_none() {
            let operation = Manager::RequestAsync()?;
            let (send, receive) = mpsc::sync_channel(1);
            operation.when(move |result| {
                let _ = send.send(result);
            })?;
            self.manager = Some(receive.recv_timeout(TIMEOUT).unwrap_or_else(|_| {
                let _ = operation.Cancel();
                Err(Error::from(HRESULT::from_win32(1460)))
            })?);
        }
        Ok(self.manager.as_ref().expect("manager initialized above"))
    }

    pub fn pause(&mut self, requested: impl Fn() -> bool) {
        let sessions = self.manager().and_then(|manager| manager.GetSessions());
        let Ok(sessions) = sessions else {
            self.manager = None;
            return;
        };
        let playing: Vec<_> = sessions
            .into_iter()
            .filter(|s| status(s) == Some(Status::Playing))
            .collect();
        self.pause_sessions(playing, requested);
    }

    pub fn pause_sessions(&mut self, sessions: Vec<Session>, requested: impl Fn() -> bool) {
        for session in sessions {
            if !requested() {
                break;
            }
            if status(&session) == Some(Status::Playing)
                && send_command(&session, true).unwrap_or(false)
            {
                // Retain this exact object: GetSessions() may replace wrappers after list changes.
                // Never substitute another session with the same application ID.
                self.paused.push(session);
            }
        }
    }

    pub fn resume(&mut self) {
        for session in self.paused.drain(..) {
            if status(&session) == Some(Status::Paused) {
                let _ = send_command(&session, false);
            }
        }
    }
}

fn status(session: &Session) -> Option<Status> {
    session
        .GetPlaybackInfo()
        .and_then(|info| info.PlaybackStatus())
        .ok()
}

fn send_command(session: &Session, pause: bool) -> Result<bool> {
    let operation = if pause {
        session.TryPauseAsync()?
    } else {
        session.TryPlayAsync()?
    };
    let (send, receive) = mpsc::sync_channel(1);
    operation.when(move |result| {
        let _ = send.send(result);
    })?;
    receive.recv_timeout(TIMEOUT).unwrap_or_else(|_| {
        let _ = operation.Cancel();
        Err(Error::from(HRESULT::from_win32(1460)))
    })
}

pub struct Worker {
    desired: Arc<AtomicU8>, // 0 = released, 1 = microphone active, 2 = exit.
    wake: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(finished: impl FnOnce() + Send + 'static) -> std::io::Result<Self> {
        let desired = Arc::new(AtomicU8::new(0));
        let state = desired.clone();
        let (wake, receive) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("Pausic media".into())
            .spawn(move || {
                // Initialization failures skip the affected cycle; the next one retries.
                let mut apartment = None;
                let mut controller = Controller::default();
                let mut applied = false;
                while receive.recv().is_ok() {
                    loop {
                        let desired = state.load(Ordering::Acquire);
                        if desired == 2 {
                            controller.resume();
                            finished();
                            return;
                        }
                        let active = desired == 1;
                        if active == applied {
                            break;
                        }
                        applied = active;
                        if apartment.is_none() {
                            apartment = Apartment::new().ok();
                        }
                        if apartment.is_some() {
                            if active {
                                controller.pause(|| state.load(Ordering::Acquire) == 1);
                            } else {
                                controller.resume();
                            }
                        }
                    }
                }
                controller.resume();
            })?;
        Ok(Self {
            desired,
            wake,
            thread: Some(thread),
        })
    }

    pub fn request(&self, active: bool) {
        self.desired.store(u8::from(active), Ordering::Release);
        let _ = self.wake.send(());
    }

    pub fn exit(&self) {
        self.desired.store(2, Ordering::Release);
        let _ = self.wake.send(());
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.exit();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
