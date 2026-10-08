use crate::{media::Controller, mic_monitor, win32::Apartment};
use std::{
    sync::mpsc,
    thread::sleep,
    time::{Duration, Instant},
};
use windows::{
    Foundation::TypedEventHandler,
    Media::{
        Control::GlobalSystemMediaTransportControlsSessionManager as Manager,
        MediaPlaybackStatus as Status, MediaPlaybackType, Playback::MediaPlayer,
        SystemMediaTransportControls, SystemMediaTransportControlsButton,
        SystemMediaTransportControlsButtonPressedEventArgs,
    },
    core::{HSTRING, Result},
};

#[test]
#[ignore = "briefly opens real microphone streams; requires a free microphone"]
fn capture_monitor_handles_overlapping_streams() -> Result<()> {
    let _apartment = Apartment::new()?;
    assert!(
        !mic_monitor::read_in_use()?,
        "close other microphone users first"
    );
    let (send, receive) = mpsc::channel();
    let monitor = mic_monitor::Monitor::start(move |active| {
        let _ = send.send(active);
    })?;
    assert!(receive.recv_timeout(Duration::from_millis(300)).is_err());
    let first = Microphone::start()?;
    assert!(
        mic_monitor::read_in_use()?,
        "real capture must be detected without ConsentStore"
    );
    assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
    let second = Microphone::start()?;
    drop(first);
    assert!(
        mic_monitor::read_in_use()?,
        "second stream still holds microphone"
    );
    assert!(receive.recv_timeout(Duration::from_millis(400)).is_err());
    monitor.restart();
    assert!(receive.recv_timeout(Duration::from_millis(300)).is_err());
    let reopened = Microphone::open()?;
    drop(second);
    assert!(receive.recv_timeout(Duration::from_millis(100)).is_err());
    reopened.activate()?;
    assert!(receive.recv_timeout(Duration::from_millis(400)).is_err());
    drop(reopened);
    assert!(receive.recv_timeout(Duration::from_millis(150)).is_err());
    assert!(!receive.recv_timeout(Duration::from_secs(2)).unwrap());
    monitor.restart();
    assert!(receive.recv_timeout(Duration::from_millis(300)).is_err());
    let fresh = Microphone::start()?;
    assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
    drop(fresh);
    assert!(!receive.recv_timeout(Duration::from_secs(2)).unwrap());
    drop(monitor);
    Ok(())
}

#[test]
#[ignore = "requires a running Pausic and a free microphone; briefly pauses/restores live media"]
fn running_binary_handles_real_microphone() -> Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    use windows::core::w;
    let _apartment = Apartment::new()?;
    unsafe {
        FindWindowW(w!("Pausic.TrayWindow"), None)?;
    }
    assert!(
        !mic_monitor::read_in_use()?,
        "close other microphone users first"
    );
    let player = Player::new("Pausic running binary diagnostic", true)?;
    let paused = Player::new("Pausic initially paused check", false)?;
    sleep(Duration::from_secs(1));
    let started = Instant::now();
    let microphone = Microphone::start()?;
    eventually(|| player.controls.PlaybackStatus().ok() == Some(Status::Paused));
    println!(
        "Actual microphone open -> media Paused observed after {} ms",
        started.elapsed().as_millis()
    );
    let released = Instant::now();
    drop(microphone);
    eventually(|| player.controls.PlaybackStatus().ok() == Some(Status::Playing));
    assert_eq!(paused.controls.PlaybackStatus()?, Status::Paused);
    println!(
        "Actual microphone release -> media Playing observed after {} ms; initially paused media stayed paused",
        released.elapsed().as_millis()
    );
    Ok(())
}

struct Player {
    player: MediaPlayer,
    controls: SystemMediaTransportControls,
    token: i64,
}

// Test-only WASAPI stream: Start/Stop exercises real capture session state.
// No capture buffers are requested, read, or saved by this test or the production app.
struct Microphone(windows::Win32::Media::Audio::IAudioClient);
impl Microphone {
    fn start() -> Result<Self> {
        let microphone = Self::open()?;
        microphone.activate()?;
        Ok(microphone)
    }
    fn activate(&self) -> Result<()> {
        unsafe { self.0.Start() }
    }
    fn open() -> Result<Self> {
        use windows::Win32::{
            Media::Audio::{
                AUDCLNT_SHAREMODE_SHARED, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
                eCapture, eConsole,
            },
            System::Com::{CLSCTX_ALL, CoCreateGuid, CoCreateInstance, CoTaskMemFree},
        };
        unsafe {
            let devices: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = devices.GetDefaultAudioEndpoint(eCapture, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let session = CoCreateGuid()?;
            let result = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                1000000,
                0,
                format,
                Some(&session),
            );
            CoTaskMemFree(Some(format.cast()));
            result?;
            Ok(Self(client))
        }
    }
}
impl Drop for Microphone {
    fn drop(&mut self) {
        unsafe {
            let _ = self.0.Stop();
        }
    }
}
impl Player {
    fn new(title: &str, playing: bool) -> Result<Self> {
        let player = MediaPlayer::new()?;
        player.CommandManager()?.SetIsEnabled(false)?;
        let controls = player.SystemMediaTransportControls()?;
        controls.SetIsEnabled(true)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        let display = controls.DisplayUpdater()?;
        display.SetType(MediaPlaybackType::Music)?;
        display.MusicProperties()?.SetTitle(&HSTRING::from(title))?;
        display.Update()?;
        let token = controls.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(|sender, args| {
            match args.ok()?.Button()? {
                SystemMediaTransportControlsButton::Pause => {
                    sender.ok()?.SetPlaybackStatus(Status::Paused)?
                }
                SystemMediaTransportControlsButton::Play => {
                    sender.ok()?.SetPlaybackStatus(Status::Playing)?
                }
                _ => {}
            }
            Ok(())
        }))?;
        controls.SetPlaybackStatus(if playing {
            Status::Playing
        } else {
            Status::Paused
        })?;
        Ok(Self {
            player,
            controls,
            token,
        })
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.controls.RemoveButtonPressed(self.token);
        let _ = self.player.Close();
    }
}

fn eventually(mut check: impl FnMut() -> bool) {
    for _ in 0..50 {
        if check() {
            return;
        }
        sleep(Duration::from_millis(50));
    }
    assert!(check(), "GSMTC state did not settle");
}

#[test]
#[ignore = "creates temporary native SMTC providers in the interactive Windows session"]
fn gsmtc_restores_only_original_sessions() -> Result<()> {
    let _apartment = Apartment::new()?;
    let prefix = format!("Pausic check {}", std::process::id());
    let first = Player::new(&format!("{prefix} first"), true)?;
    let second = Player::new(&format!("{prefix} second"), true)?;
    let paused = Player::new(&format!("{prefix} paused"), false)?;
    let manager = Manager::RequestAsync()?.join()?;
    let mut own = Vec::new();
    for _ in 0..30 {
        own.clear();
        for session in manager.GetSessions()? {
            if session
                .TryGetMediaPropertiesAsync()?
                .join()?
                .Title()?
                .to_string_lossy()
                .starts_with(&prefix)
            {
                own.push(session);
            }
        }
        if own.len() == 3 {
            break;
        }
        sleep(Duration::from_millis(100));
    }
    assert_eq!(own.len(), 3, "never command unrelated media sessions");
    let mut controller = Controller::default();
    controller.pause_sessions(own, || true);
    eventually(|| {
        first.controls.PlaybackStatus().ok() == Some(Status::Paused)
            && second.controls.PlaybackStatus().ok() == Some(Status::Paused)
    });
    drop(second);
    let replacement = Player::new(&format!("{prefix} replacement"), false)?;
    sleep(Duration::from_millis(350));
    controller.resume();
    eventually(|| first.controls.PlaybackStatus().ok() == Some(Status::Playing));
    assert_eq!(paused.controls.PlaybackStatus()?, Status::Paused);
    assert_eq!(replacement.controls.PlaybackStatus()?, Status::Paused);
    first.controls.SetPlaybackStatus(Status::Paused)?;
    controller.resume();
    assert_eq!(
        first.controls.PlaybackStatus()?,
        Status::Paused,
        "ownership cleared after resume"
    );
    Ok(())
}

#[test]
#[ignore = "opens real microphone streams to check the active startup baseline"]
fn capture_monitor_preserves_active_startup() -> Result<()> {
    let _apartment = Apartment::new()?;
    assert!(
        !mic_monitor::read_in_use()?,
        "close other microphone users first"
    );
    let microphone = Microphone::start()?;
    let (send, receive) = mpsc::channel();
    let monitor = mic_monitor::Monitor::start(move |active| {
        let _ = send.send(active);
    })?;
    assert!(receive.recv_timeout(Duration::from_millis(400)).is_err());
    drop(microphone);
    assert!(!receive.recv_timeout(Duration::from_secs(2)).unwrap());
    let next = Microphone::start()?;
    assert!(receive.recv_timeout(Duration::from_secs(2)).unwrap());
    drop(next);
    assert!(!receive.recv_timeout(Duration::from_secs(2)).unwrap());
    drop(monitor);
    Ok(())
}

#[test]
#[ignore = "holds a real capture stream for 30 seconds to measure Pausic CPU usage"]
fn hold_real_capture_for_measurement() -> Result<()> {
    let _apartment = Apartment::new()?;
    assert!(
        !mic_monitor::read_in_use()?,
        "close other microphone users first"
    );
    let _microphone = Microphone::start()?;
    println!("CAPTURE_READY");
    sleep(Duration::from_secs(30));
    Ok(())
}
