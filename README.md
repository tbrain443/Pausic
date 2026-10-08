<img src="assets/pausic.png" width="80" alt="Pausic microphone and pause icon">

# Pausic

A lightweight Windows tray app written in Rust. Pausic pauses your media when an application uses a microphone, then resumes only the media it paused when the microphone is released.

## Download

Download **[Pausic.exe from the latest release](https://github.com/tbrain443/Pausic/releases/latest)** and run it. A single portable executable: no installer, administrator privileges, or separate runtime required.

Designed for Windows 10 1809+ and Windows 11, x64. Tested on Windows **26H2, build 26300.9550**, x64.

Right-click the tray icon for **Enabled**, **Start with Windows**, **About Pausic**, and **Exit**. On first launch, Pausic enables startup with Windows. Turn this off in the tray menu if you prefer. Keep the EXE in a permanent location; after moving it, toggle Start with Windows off and on to update its path.

## How it works

- Uses **Windows Core Audio callbacks** to monitor capture sessions across all active input devices. Subscribes to new sessions, session state changes, and device changes; it does not depend on microphone history in the registry or the taskbar indicator.
- **No periodic microphone polling.** The monitor sleeps until a notification arrives. Pauses when activity is observed; waits for **250 ms** of inactivity before resuming to avoid interruptions from brief microphone reconnects. If Core Audio becomes unavailable, retries subscriptions once per second and preserves the last known state.
- Controls media through **Global System Media Transport Controls (GSMTC)**. Only sessions that were playing and accepted Pausic's pause request are eligible for resume. Already paused, closed, and replacement sessions stay untouched.
- Does not record audio, read capture buffers, change microphone permissions, or send telemetry. Settings and startup registration are stored in the current user's registry.

Startup establishes a baseline. If a microphone is already active when Pausic starts, or when you enable it, media waits for the next microphone activation. New playback during an active microphone session and manual Play commands are not overridden.

## Small footprint

Measured locally on October 8, 2026, on the Windows build above, with an Intel Core i9-13980HX:

| CPU over a 20-second sample | v1.0.0 (polling) | v1.1.0 (callbacks) |
| --- | --- | --- |
| Idle, two paired samples | 203–234 ms (1.01–1.17% of one logical CPU) | 0 ms reported in both samples |
| Microphone held open | 250 ms (1.25% of one logical CPU) | 0 ms reported |
| Idle private memory | 2.35–2.40 MiB | 2.11–2.12 MiB |

Measurements use the same installed EXE path, a fresh process, two seconds of warm-up, and Windows process CPU counters. A reported zero means below the counter's resolution for these samples, not that event handling has no cost. The active test holds a real shared-mode WASAPI capture stream without reading audio; only Pausic's process is measured.

The local release EXE is about 525 KiB, including the icon and static C runtime. End-to-end tests observed approximately 280 ms from microphone creation to media pause and 270 ms from release to resume, compared with 274 ms / 421 ms for v1.0.0. These are sample measurements, not latency guarantees: they include stream initialization, Windows media commands, and a 50 ms sampling interval. No async runtime, web UI, or bundled .NET runtime.

## Compatibility

Media players must expose GSMTC sessions and accept play/pause commands. Microphone detection has been verified with real shared-mode WASAPI streams, including independent overlapping sessions, rapid reopen, active startup, subscription restart, and end-to-end pause/resume. Individual browser/messenger interfaces, exclusive-mode capture, ASIO, physical hot-plug, audio-service restart, and physical sleep/resume have not been separately verified. Core Audio session notifications do not provide a general ASIO or exclusive-mode detection guarantee.

Exit and disabling Pausic attempt to restore its paused sessions. Force termination or a power loss cannot guarantee restoration.

## Build

Install Rust (MSVC toolchain), Visual Studio Build Tools with **Desktop development with C++**, and a Windows SDK. Use an **x64 Native Tools command prompt**:

```powershell
cargo build --release --locked
```

The executable is `target/release/Pausic.exe`. The C runtime is linked statically. `rc.exe` embeds the icon and manifest; the `RC` environment variable can override its path.

## Checks

```powershell
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Interactive checks need a free microphone and Windows desktop session. They briefly open capture streams without reading audio. Run one at a time:

```powershell
cargo test --locked capture_monitor_handles_overlapping_streams -- --ignored --nocapture --test-threads=1
cargo test --locked capture_monitor_preserves_active_startup -- --ignored --nocapture --test-threads=1
cargo test --locked gsmtc_restores_only_original_sessions -- --ignored --nocapture --test-threads=1
# Start the release EXE first:
cargo test --locked running_binary_handles_real_microphone -- --ignored --nocapture --test-threads=1
```

## License

[MIT](LICENSE).
