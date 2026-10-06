#![windows_subsystem = "windows"]

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eframe::egui;
use microboost::{echo, noise_gate, RefRing, SpscRing, RING_SIZE};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// Device profiles — saved per microphone name
mod profiles {
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn profiles_path() -> PathBuf {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("Microboost").join("profiles.json")
    }

    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct Profile {
        pub boost: u32,
        #[serde(default)]
        pub noise_floor_rms: Option<f32>,
        #[serde(default)]
        pub noise_gate_enabled: bool,
    }

    /// Map from device name -> Profile
    pub type ProfileMap = HashMap<String, Profile>;

    pub fn load() -> ProfileMap {
        let path = profiles_path();
        if let Ok(data) = std::fs::read_to_string(&path) {
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            HashMap::new()
        }
    }

    pub fn save(map: &ProfileMap) {
        let path = profiles_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        if let Ok(json) = serde_json::to_string_pretty(map) {
            let _ = std::fs::write(&path, json);
        }
    }

    fn settings_path() -> PathBuf {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("Microboost").join("settings.json")
    }

    fn default_true() -> bool {
        true
    }

    fn default_echo_strength() -> u32 {
        50
    }

    #[derive(Serialize, Deserialize, Clone)]
    pub struct Settings {
        pub last_input_device: Option<String>,
        /// Start boosting immediately on launch (otherwise start in 1x passthrough)
        #[serde(default = "default_true")]
        pub auto_start_boost: bool,
        /// Minimise / close hides the window to the system tray instead of quitting
        #[serde(default = "default_true")]
        pub minimize_to_tray: bool,
        /// Launch with the window hidden (only meaningful with minimize_to_tray)
        #[serde(default)]
        pub start_in_tray: bool,
        /// Remove the PC's own playback (speakers) from the mic
        #[serde(default = "default_true")]
        pub echo_suppress: bool,
        /// Adaptive cancellation (talk over media) vs. plain mute-while-playing
        #[serde(default = "default_true")]
        pub echo_adaptive: bool,
        /// Maximum attenuation applied while the speakers play, in dB
        #[serde(default = "default_echo_strength")]
        pub echo_strength_db: u32,
        /// Output device to capture as the echo reference; None = Windows default
        #[serde(default)]
        pub echo_ref_device: Option<String>,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                last_input_device: None,
                auto_start_boost: true,
                minimize_to_tray: true,
                start_in_tray: false,
                echo_suppress: true,
                echo_adaptive: true,
                echo_strength_db: 50,
                echo_ref_device: None,
            }
        }
    }

    pub fn load_settings() -> Settings {
        let path = settings_path();
        if let Ok(data) = std::fs::read_to_string(&path) {
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Settings::default()
        }
    }

    pub fn save_settings(settings: &Settings) {
        let path = settings_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        if let Ok(json) = serde_json::to_string_pretty(settings) {
            let _ = std::fs::write(&path, json);
        }
    }
}

/// VB-CABLE auto-setup
mod vbcable {
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    fn app_dir() -> PathBuf {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("Microboost")
    }

    /// Check if VB-CABLE is installed by looking for its devices
    pub fn is_installed() -> bool {
        let host = cpal::default_host();
        use cpal::traits::HostTrait;
        if let Ok(devs) = host.output_devices() {
            use cpal::traits::DeviceTrait;
            for d in devs {
                if let Ok(name) = d.name() {
                    if name.to_lowercase().contains("cable input") {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Download VB-CABLE zip
    pub fn download(status: &Arc<Mutex<String>>) -> Result<PathBuf, String> {
        let dir = app_dir();
        let _ = std::fs::create_dir_all(&dir);
        let zip_path = dir.join("VBCABLE_Driver_Pack.zip");

        // If already downloaded, skip
        if zip_path.exists() {
            let meta = std::fs::metadata(&zip_path).ok();
            if meta.map(|m| m.len() > 100_000).unwrap_or(false) {
                return Ok(zip_path);
            }
        }

        *status.lock().unwrap() = "Downloading VB-CABLE...".to_string();

        // Use PowerShell to download (available on all modern Windows)
        let result = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &format!(
                    "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
                     Invoke-WebRequest -Uri 'https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack43.zip' \
                     -OutFile '{}'",
                    zip_path.display()
                ),
            ])
            .output();

        match result {
            Ok(output) if output.status.success() && zip_path.exists() => Ok(zip_path),
            Ok(output) => {
                let err = String::from_utf8_lossy(&output.stderr);
                Err(format!("Download failed: {}", err))
            }
            Err(e) => Err(format!("Could not run PowerShell: {}", e)),
        }
    }

    /// Extract the zip and run the installer with admin privileges
    pub fn install(zip_path: &PathBuf, status: &Arc<Mutex<String>>) -> Result<(), String> {
        let dir = app_dir().join("vbcable");
        let _ = std::fs::create_dir_all(&dir);

        *status.lock().unwrap() = "Extracting VB-CABLE...".to_string();

        // Extract using PowerShell
        let extract = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &format!(
                    "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                    zip_path.display(),
                    dir.display()
                ),
            ])
            .output();

        if extract.is_err() || !extract.as_ref().unwrap().status.success() {
            return Err("Failed to extract VB-CABLE".to_string());
        }

        // Find the 64-bit setup exe
        let setup_exe = dir.join("VBCABLE_Setup_x64.exe");
        if !setup_exe.exists() {
            return Err(format!(
                "Setup not found at {}. Check extraction.",
                setup_exe.display()
            ));
        }

        *status.lock().unwrap() =
            "Installing VB-CABLE (admin prompt)...".to_string();

        // Run installer elevated (will trigger UAC prompt)
        let install = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &format!(
                    "Start-Process -FilePath '{}' -ArgumentList '-i','-h' -Verb RunAs -Wait",
                    setup_exe.display()
                ),
            ])
            .output();

        match install {
            Ok(output) if output.status.success() => {
                // Give Windows a moment to register the device
                std::thread::sleep(std::time::Duration::from_secs(2));
                if is_installed() {
                    Ok(())
                } else {
                    // Device might need a restart of audio service
                    Err("Installed but device not yet visible. Try restarting the app.".to_string())
                }
            }
            Ok(output) => {
                let err = String::from_utf8_lossy(&output.stderr);
                Err(format!("Install failed: {}", err))
            }
            Err(e) => Err(format!("Could not launch installer: {}", e)),
        }
    }
}

/// System tray icon + minimise/close-to-tray.
///
/// Hiding is done at the Win32 level: we wrap the window procedure winit installed and
/// swallow WM_CLOSE / SC_MINIMIZE, calling ShowWindow(SW_HIDE) instead. This works even
/// when egui isn't painting (a hidden window gets no redraw events), which is why the
/// tray handlers also talk to Win32 directly rather than going through egui commands.
mod tray {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, GWLP_WNDPROC,
        SC_MINIMIZE, SW_HIDE, SW_RESTORE, WM_CLOSE, WM_SYSCOMMAND, WNDPROC,
    };

    static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
    static PREV_WNDPROC: AtomicIsize = AtomicIsize::new(0);
    static TO_TRAY_ENABLED: AtomicBool = AtomicBool::new(true);

    fn main_hwnd() -> HWND {
        HWND(MAIN_HWND.load(Ordering::SeqCst) as *mut core::ffi::c_void)
    }

    pub fn set_enabled(enabled: bool) {
        TO_TRAY_ENABLED.store(enabled, Ordering::SeqCst);
    }

    pub fn hide_window() {
        if MAIN_HWND.load(Ordering::SeqCst) != 0 {
            unsafe {
                let _ = ShowWindow(main_hwnd(), SW_HIDE);
            }
        }
    }

    pub fn show_window() {
        if MAIN_HWND.load(Ordering::SeqCst) != 0 {
            unsafe {
                let _ = ShowWindow(main_hwnd(), SW_RESTORE);
                let _ = SetForegroundWindow(main_hwnd());
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if TO_TRAY_ENABLED.load(Ordering::SeqCst) {
            let minimize = msg == WM_SYSCOMMAND && (wparam.0 & 0xFFF0) as u32 == SC_MINIMIZE;
            if minimize || msg == WM_CLOSE {
                let _ = ShowWindow(hwnd, SW_HIDE);
                return LRESULT(0);
            }
        }
        // Option<fn> is pointer-sized with a null niche, so this round-trip is sound.
        let prev: WNDPROC = std::mem::transmute(PREV_WNDPROC.load(Ordering::SeqCst));
        CallWindowProcW(prev, hwnd, msg, wparam, lparam)
    }

    /// Simple procedurally drawn microphone icon (RGBA).
    fn make_icon(size: u32) -> Vec<u8> {
        let mut px = vec![0u8; (size * size * 4) as usize];
        let s = size as f32;
        let put = |px: &mut Vec<u8>, x: u32, y: u32, a: f32| {
            let i = ((y * size + x) * 4) as usize;
            px[i] = 70;
            px[i + 1] = 190;
            px[i + 2] = 90;
            px[i + 3] = (a.clamp(0.0, 1.0) * 255.0) as u8;
        };
        for y in 0..size {
            for x in 0..size {
                let fx = (x as f32 + 0.5) / s;
                let fy = (y as f32 + 0.5) / s;
                // Capsule: rounded rect centred at x=0.5, y in [0.12, 0.56], half-width 0.15
                let cx = (fx - 0.5).abs();
                let capsule = if fy >= 0.27 && fy <= 0.41 {
                    cx <= 0.15
                } else {
                    let cy = if fy < 0.27 { 0.27 - fy } else { fy - 0.41 };
                    (cx * cx + cy * cy).sqrt() <= 0.15
                };
                // Cradle: lower half of a ring centred at (0.5, 0.41), r 0.25, thickness 0.06
                let dy = fy - 0.41;
                let r = ((fx - 0.5).powi(2) + dy * dy).sqrt();
                let cradle = dy >= 0.0 && (r - 0.25).abs() <= 0.045;
                // Stem + base
                let stem = cx <= 0.04 && fy > 0.66 && fy <= 0.82;
                let base = cx <= 0.2 && fy > 0.82 && fy <= 0.9;
                if capsule || cradle || stem || base {
                    put(&mut px, x, y, 1.0);
                }
            }
        }
        px
    }

    /// Install the window-proc hook and create the tray icon. Returns None if anything
    /// fails (the app then behaves like before: closing quits).
    pub fn install(cc: &eframe::CreationContext<'_>) -> Option<TrayIcon> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        let hwnd_val = match cc.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(h) => h.hwnd.get(),
            _ => return None,
        };
        MAIN_HWND.store(hwnd_val, Ordering::SeqCst);
        unsafe {
            let prev = SetWindowLongPtrW(
                HWND(hwnd_val as *mut core::ffi::c_void),
                GWLP_WNDPROC,
                wndproc as usize as isize,
            );
            PREV_WNDPROC.store(prev, Ordering::SeqCst);
        }

        let show = MenuItem::with_id("show", "Show Microboost", true, None);
        let quit = MenuItem::with_id("quit", "Quit", true, None);
        let menu = Menu::with_items(&[&show, &PredefinedMenuItem::separator(), &quit]).ok()?;

        MenuEvent::set_event_handler(Some(|e: MenuEvent| match e.id.0.as_str() {
            "show" => show_window(),
            // Everything is persisted on change; the audio streams die with the process.
            "quit" => std::process::exit(0),
            _ => {}
        }));
        TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                show_window();
            }
        }));

        let icon = tray_icon::Icon::from_rgba(make_icon(32), 32, 32).ok()?;
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Microboost — right-click for menu")
            .with_icon(icon)
            .build()
            .ok()
    }
}

/// "Launch with Windows" via HKCU\Software\Microsoft\Windows\CurrentVersion\Run.
/// The registry is the source of truth; nothing is duplicated in settings.json.
mod autostart {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SAM_FLAGS, REG_SZ,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Microboost";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Quoted path of the running executable, as it should appear in the Run key.
    fn command_line() -> Option<String> {
        let exe = std::env::current_exe().ok()?;
        Some(format!("\"{}\"", exe.display()))
    }

    fn open_key(access: REG_SAM_FLAGS) -> Option<HKEY> {
        let sub = wide(RUN_KEY);
        let mut key = HKEY::default();
        let err = unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), 0, access, &mut key)
        };
        (err == ERROR_SUCCESS).then_some(key)
    }

    /// The command currently registered, if any.
    pub fn current_value() -> Option<String> {
        let key = open_key(KEY_READ)?;
        let name = wide(VALUE_NAME);
        let mut buf = vec![0u8; 4096];
        let mut len = buf.len() as u32;
        let err = unsafe {
            RegQueryValueExW(
                key,
                PCWSTR(name.as_ptr()),
                None,
                None,
                Some(buf.as_mut_ptr()),
                Some(&mut len),
            )
        };
        unsafe {
            let _ = RegCloseKey(key);
        }
        if err != ERROR_SUCCESS {
            return None;
        }
        let u16s: Vec<u16> = buf[..len as usize]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        Some(String::from_utf16_lossy(&u16s))
    }

    pub fn is_enabled() -> bool {
        current_value().is_some()
    }

    /// Register the current exe. Returns an error string for the status line on failure.
    pub fn enable() -> Result<(), String> {
        let cmd = command_line().ok_or("Could not determine exe path")?;
        let key = open_key(KEY_SET_VALUE).ok_or("Could not open Run registry key")?;
        let name = wide(VALUE_NAME);
        let data = wide(&cmd);
        let bytes: Vec<u8> = data.iter().flat_map(|c| c.to_le_bytes()).collect();
        let err = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(&bytes)) };
        unsafe {
            let _ = RegCloseKey(key);
        }
        if err == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("Registry write failed (error {})", err.0))
        }
    }

    pub fn disable() -> Result<(), String> {
        let key = open_key(KEY_SET_VALUE).ok_or("Could not open Run registry key")?;
        let name = wide(VALUE_NAME);
        let err = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
        unsafe {
            let _ = RegCloseKey(key);
        }
        if err == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("Registry delete failed (error {})", err.0))
        }
    }

    /// If enabled but pointing at a different exe (e.g. a newer build was copied
    /// elsewhere), re-point it at the exe that is actually running.
    pub fn sync_path_if_enabled() {
        if let (Some(current), Some(wanted)) = (current_value(), command_line()) {
            if current != wanted {
                let _ = enable();
            }
        }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 860.0])
            .with_resizable(false)
            .with_title("Microboost"),
        ..Default::default()
    };

    eframe::run_native(
        "Microboost",
        options,
        Box::new(|cc| Ok(Box::new(MicroboostApp::new(cc)))),
    )
}

#[derive(PartialEq)]
enum SetupState {
    NotInstalled,
    Downloading,
    Failed(String),
    Ready,
}

#[derive(PartialEq, Clone)]
enum CalibrationPhase {
    Idle,
    Listening,
    Done {
        boost_pct: u32,
        raw_db: f32,      // Your voice level in dBFS
        boosted_db: f32,   // After boost, in dBFS
        target_db: f32,    // YouTube target in dBFS
    },
}

const CALIBRATION_PHRASES: &[&str] = &[
    "Hello, testing one two three. This is my normal speaking voice.",
    "The quick brown fox jumps over the lazy dog near the river bank.",
    "I'm recording a video and want my audio to sound clear and loud.",
];

/// YouTube recommended voice target: ~-16 dBFS RMS (0.16 linear)
const TARGET_RMS: f32 = 0.16;
/// Maximum boost in percent (50x, ~+34 dB). Anything above ~5x will clip on peaks for a normal mic.
const MAX_BOOST_PCT: u32 = 5000;


struct MicroboostApp {
    host: cpal::Host,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
    selected_input: usize,
    selected_output: usize,

    boost: u32,
    is_active: bool,
    status: Arc<Mutex<String>>,

    // VB-CABLE setup
    setup_state: SetupState,
    setup_thread: Option<std::thread::JoinHandle<Result<(), String>>>,

    // Audio pipeline
    input_stream: Arc<Mutex<Option<cpal::Stream>>>,
    output_stream: Arc<Mutex<Option<cpal::Stream>>>,
    ring_buffer: Arc<SpscRing>,
    pipeline_active: Arc<Mutex<bool>>,
    live_gain: Arc<Mutex<f32>>,       // Shared gain: updated without restarting pipeline
    live_input_rms: Arc<Mutex<f32>>,  // Raw input level for visualizer
    live_output_rms: Arc<Mutex<f32>>, // Boosted output level for visualizer
    input_history: Vec<f32>,   // Rolling waveform history (RMS values)
    output_history: Vec<f32>,
    vis_frame: u32,            // Frame counter for slowing down visualizer
    vis_accum_in: f32,         // Accumulated input RMS across frames
    vis_accum_out: f32,        // Accumulated output RMS across frames

    // Test recording
    is_recording: bool,
    recording_start: Option<std::time::Instant>,
    last_recording: Option<PathBuf>,
    sample_rate: u32,
    rec_stream: Arc<Mutex<Option<cpal::Stream>>>,
    rec_active: Arc<Mutex<bool>>,
    rec_samples: Arc<Mutex<Vec<f32>>>,

    // Auto-calibration
    cal_phase: CalibrationPhase,
    cal_start: Option<std::time::Instant>,
    cal_stream: Arc<Mutex<Option<cpal::Stream>>>,
    cal_active: Arc<Mutex<bool>>,
    cal_samples: Arc<Mutex<Vec<f32>>>,
    cal_rms_live: Arc<Mutex<f32>>,
    cal_phrase_idx: usize,

    // Profiles
    device_profiles: HashMap<String, profiles::Profile>,

    // Auto-start
    first_frame: bool,
    frame_count: u32,

    // Persisted app settings (auto-start, tray behaviour)
    settings: profiles::Settings,
    // Keeps the tray icon alive; None if creation failed
    _tray: Option<tray_icon::TrayIcon>,
    // Mirrors the HKCU Run registry entry
    launch_with_windows: bool,

    // Speaker echo suppression
    echo_shared: Arc<echo::Shared>,
    ref_ring: Arc<RefRing>,
    // (capture timestamp, ring position) of the first sample of the last loopback buffer
    ref_anchor: Arc<Mutex<Option<(cpal::StreamInstant, u64)>>>,
    loopback_stream: Arc<Mutex<Option<cpal::Stream>>>,
    loopback_note: String,
    loopback_desc: String,
    // Name of the output device the loopback currently captures
    loopback_name: String,
    ref_silent_since: Option<std::time::Instant>,
    // 10 s echo diagnostic recording: (raw mic, aligned reference) pairs
    diag_rec: Arc<Mutex<Option<Vec<(f32, f32)>>>>,
    diag_cap: usize,
    diag_rate: u32,
    diag_note: String,

    // Noise gate
    noise_gate: Arc<Mutex<noise_gate::NoiseGate>>,
    ng_cal_state: Arc<Mutex<noise_gate::CalibrationState>>,
    ng_calibrating: bool,
    ng_cal_start: Option<std::time::Instant>,
    ng_cal_stream: Arc<Mutex<Option<cpal::Stream>>>,

    // Hot-plug detection
    last_device_check: std::time::Instant,

}


impl MicroboostApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let host = cpal::default_host();
        let (input_devices, output_devices) = Self::enumerate_devices(&host);

        let cable_installed = vbcable::is_installed();
        let selected_output = Self::find_cable_output(&output_devices).unwrap_or(0);

        let device_profiles = profiles::load();
        let settings = profiles::load_settings();

        // Select input device: 1) last used, 2) Windows default, 3) first
        let selected_input = settings
            .last_input_device
            .as_ref()
            .and_then(|saved| input_devices.iter().position(|d| d == saved))
            .or_else(|| {
                // Try to match Windows default input device
                host.default_input_device()
                    .and_then(|d| d.name().ok())
                    .and_then(|default_name| {
                        input_devices.iter().position(|d| d == &default_name)
                    })
            })
            .unwrap_or(0);

        tray::set_enabled(settings.minimize_to_tray);
        let tray = tray::install(cc);
        autostart::sync_path_if_enabled();
        let launch_with_windows = autostart::is_enabled();
        let echo_shared = Arc::new(echo::Shared::new(
            settings.echo_adaptive,
            settings.echo_strength_db as f32,
        ));

        // Load saved boost for the selected device
        let current_profile = input_devices
            .get(selected_input)
            .and_then(|name| device_profiles.get(name));
        let boost = current_profile.map(|p| p.boost).unwrap_or(200);

        // Restore noise gate from profile
        let noise_gate = noise_gate::NoiseGate::new();
        let ng = {
            let mut ng = noise_gate;
            if let Some(profile) = current_profile {
                if let Some(floor) = profile.noise_floor_rms {
                    let headroom = ng.headroom;
                    ng.restore(floor, profile.noise_gate_enabled, headroom);
                }
            }
            ng
        };

        Self {
            host,
            input_devices,
            output_devices,
            selected_input,
            selected_output,
            boost,
            is_active: false,
            status: Arc::new(Mutex::new("Ready".to_string())),
            setup_state: if cable_installed {
                SetupState::Ready
            } else {
                SetupState::NotInstalled
            },
            setup_thread: None,
            input_stream: Arc::new(Mutex::new(None)),
            output_stream: Arc::new(Mutex::new(None)),
            ring_buffer: Arc::new(SpscRing::new(RING_SIZE)),
            pipeline_active: Arc::new(Mutex::new(false)),
            live_gain: Arc::new(Mutex::new(1.0)),
            live_input_rms: Arc::new(Mutex::new(0.0)),
            live_output_rms: Arc::new(Mutex::new(0.0)),
            input_history: vec![0.0; 120],
            output_history: vec![0.0; 120],
            vis_frame: 0,
            vis_accum_in: 0.0,
            vis_accum_out: 0.0,
            is_recording: false,
            recording_start: None,
            last_recording: None,
            sample_rate: 48000,
            rec_stream: Arc::new(Mutex::new(None)),
            rec_active: Arc::new(Mutex::new(false)),
            rec_samples: Arc::new(Mutex::new(Vec::new())),

            cal_phase: CalibrationPhase::Idle,
            cal_start: None,
            cal_stream: Arc::new(Mutex::new(None)),
            cal_active: Arc::new(Mutex::new(false)),
            cal_samples: Arc::new(Mutex::new(Vec::new())),
            cal_rms_live: Arc::new(Mutex::new(0.0)),
            cal_phrase_idx: 0,

            device_profiles,

            first_frame: true,
            frame_count: 0,
            settings,
            _tray: tray,
            launch_with_windows,
            echo_shared,
            ref_ring: Arc::new(RefRing::new(RING_SIZE)),
            ref_anchor: Arc::new(Mutex::new(None)),
            loopback_stream: Arc::new(Mutex::new(None)),
            loopback_note: String::new(),
            loopback_desc: String::new(),
            loopback_name: String::new(),
            ref_silent_since: None,
            diag_rec: Arc::new(Mutex::new(None)),
            diag_cap: 0,
            diag_rate: 48000,
            diag_note: String::new(),

            noise_gate: Arc::new(Mutex::new(ng)),
            ng_cal_state: noise_gate::new_calibration_state(),
            ng_calibrating: false,
            ng_cal_start: None,
            ng_cal_stream: Arc::new(Mutex::new(None)),

            last_device_check: std::time::Instant::now(),
        }
    }

    fn enumerate_devices(host: &cpal::Host) -> (Vec<String>, Vec<String>) {
        let inputs: Vec<String> = host
            .input_devices()
            .map(|devs| {
                devs.filter_map(|d| d.name().ok())
                    .filter(|name| {
                        let lower = name.to_lowercase();
                        !lower.contains("cable output") && !lower.contains("cable input")
                    })
                    .collect()
            })
            .unwrap_or_default();
        let outputs: Vec<String> = host
            .output_devices()
            .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default();
        (inputs, outputs)
    }

    /// Find an input device by name (avoids index mismatch when CABLE is filtered from UI)
    /// Capture what Windows is playing on the default output device (WASAPI
    /// loopback — cpal enables it when an output device is opened for input),
    /// downmix to mono, resample to the mic rate and feed the reference ring.
    /// Returns the stream, a description for the diagnostics line, and the
    /// name of the device actually captured.
    fn build_loopback(
        host: &cpal::Host,
        mic_rate: u32,
        ring: Arc<RefRing>,
        anchor: Arc<Mutex<Option<(cpal::StreamInstant, u64)>>>,
        active: Arc<Mutex<bool>>,
        wanted: Option<&str>,
    ) -> Result<(cpal::Stream, String, String), String> {
        fn err_fn(e: cpal::StreamError) {
            eprintln!("Loopback error: {}", e);
        }
        // The chosen output device, else whatever Windows currently routes to.
        let chosen = wanted.and_then(|w| {
            host.output_devices()
                .ok()?
                .find(|d| d.name().ok().as_deref() == Some(w))
        });
        let dev = match chosen {
            Some(d) => d,
            None => host
                .default_output_device()
                .ok_or_else(|| "no default output device".to_string())?,
        };
        let name = dev.name().unwrap_or_default();
        let cfg = dev
            .default_output_config()
            .map_err(|e| format!("{} ({})", e, name))?;
        let channels = cfg.channels() as usize;
        let rate = cfg.sample_rate().0;
        let format = cfg.sample_format();
        let stream_cfg: cpal::StreamConfig = cfg.into();
        ring.reset();
        *anchor.lock().unwrap() = None;
        let mut rs = echo::LinearResampler::new(rate, mic_rate);
        // Each buffer: remember where its first sample landed in the ring and
        // when it was captured, so the mic side can align by time.
        let stream = match format {
            cpal::SampleFormat::F32 => dev.build_input_stream(
                &stream_cfg,
                move |data: &[f32], info: &cpal::InputCallbackInfo| {
                    if !*active.lock().unwrap() {
                        return;
                    }
                    let start = ring.write_pos();
                    for frame in data.chunks(channels) {
                        let mono = frame.iter().sum::<f32>() / channels as f32;
                        rs.push(mono, |s| ring.push(s));
                    }
                    *anchor.lock().unwrap() = Some((info.timestamp().capture, start));
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::I16 => dev.build_input_stream(
                &stream_cfg,
                move |data: &[i16], info: &cpal::InputCallbackInfo| {
                    if !*active.lock().unwrap() {
                        return;
                    }
                    let start = ring.write_pos();
                    for frame in data.chunks(channels) {
                        let mono = frame.iter().map(|&v| v as f32 / 32768.0).sum::<f32>()
                            / channels as f32;
                        rs.push(mono, |s| ring.push(s));
                    }
                    *anchor.lock().unwrap() = Some((info.timestamp().capture, start));
                },
                err_fn,
                None,
            ),
            other => return Err(format!("unsupported sample format {:?} on {}", other, name)),
        }
        .map_err(|e| format!("{} ({})", e, name))?;
        stream.play().map_err(|e| format!("{} ({})", e, name))?;
        let desc = format!("{} {}Hz/{}ch {:?} → {}Hz", name, rate, channels, format, mic_rate);
        eprintln!("Loopback: {}", desc);
        Ok((stream, desc, name))
    }

    fn finish_diag_recording(&mut self) {
        let done = {
            let mut guard = self.diag_rec.lock().unwrap();
            match guard.as_ref() {
                Some(v) if self.diag_cap > 0 && v.len() >= self.diag_cap => guard.take(),
                _ => None,
            }
        };
        if let Some(samples) = done {
            let dir = PathBuf::from(std::env::var("APPDATA").unwrap_or(".".to_string()))
                .join("Microboost");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("echo_diag.wav");
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: self.diag_rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let result = hound::WavWriter::create(&path, spec).and_then(|mut w| {
                for (m, r) in &samples {
                    w.write_sample(*m)?;
                    w.write_sample(*r)?;
                }
                w.finalize()
            });
            self.diag_note = match result {
                Ok(()) => format!("Saved {} (mic L, reference R)", path.display()),
                Err(e) => format!("Could not save diagnostic: {}", e),
            };
        }
    }

    fn find_input_device_by_name(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
        host.input_devices().ok()?.find(|d| d.name().ok().as_deref() == Some(name))
    }

    fn find_cable_output(devices: &[String]) -> Option<usize> {
        devices.iter().position(|name| {
            let lower = name.to_lowercase();
            lower.contains("cable input")
        })
    }

    fn refresh_devices(&mut self) {
        let (inputs, outputs) = Self::enumerate_devices(&self.host);
        self.input_devices = inputs;
        self.output_devices = outputs;
        if let Some(idx) = Self::find_cable_output(&self.output_devices) {
            self.selected_output = idx;
        }
    }

    /// Periodically check for device changes (hot-plug).
    /// If the current device disappeared, switch to the Windows default (or first available).
    /// If a new default device appeared that wasn't there before, switch to it.
    fn check_device_changes(&mut self) {
        if self.last_device_check.elapsed().as_secs() < 2 {
            return;
        }
        self.last_device_check = std::time::Instant::now();

        // Following the Windows default output: restart when it moves (for
        // example the Realtek "Speakers" / "Headphones" endpoints swapping).
        if self.settings.echo_suppress
            && self.settings.echo_ref_device.is_none()
            && *self.pipeline_active.lock().unwrap()
            && self.loopback_stream.lock().unwrap().is_some()
        {
            let current = self
                .host
                .default_output_device()
                .and_then(|d| d.name().ok())
                .unwrap_or_default();
            if !current.is_empty() && current != self.loopback_name {
                let was_active = self.is_active;
                self.kill_pipeline();
                self.start_pipeline();
                if !was_active {
                    self.stop_pipeline();
                }
                return;
            }
        }

        let old_devices = self.input_devices.clone();
        let current_name = self.input_devices.get(self.selected_input).cloned();

        self.refresh_devices();

        // Check if device list actually changed
        if self.input_devices == old_devices {
            return;
        }

        // Get Windows default device name
        let default_name = self
            .host
            .default_input_device()
            .and_then(|d| d.name().ok());

        // If our current device is still present, keep it
        if let Some(ref name) = current_name {
            if let Some(pos) = self.input_devices.iter().position(|d| d == name) {
                self.selected_input = pos;
                return;
            }
        }

        // Current device disappeared — switch to default or first available
        let new_idx = default_name
            .as_ref()
            .and_then(|def| self.input_devices.iter().position(|d| d == def))
            .unwrap_or(0);

        if new_idx != self.selected_input || current_name.is_none() {
            self.selected_input = new_idx;
            self.load_profile_for_device();

            // Restart pipeline with new device
            let was_active = self.is_active;
            self.kill_pipeline();
            if !self.input_devices.is_empty() {
                self.start_pipeline();
                if !was_active {
                    self.stop_pipeline();
                }
            }

            let dev_name = self
                .input_devices
                .get(self.selected_input)
                .cloned()
                .unwrap_or("none".to_string());
            *self.status.lock().unwrap() =
                format!("Device changed — switched to {}", dev_name);
        }
    }

    fn start_vbcable_install(&mut self) {
        self.setup_state = SetupState::Downloading;
        let status = self.status.clone();

        self.setup_thread = Some(std::thread::spawn(move || {
            let zip = vbcable::download(&status)?;
            vbcable::install(&zip, &status)?;
            *status.lock().unwrap() = "VB-CABLE installed!".to_string();
            Ok(())
        }));
    }

    fn check_setup_thread(&mut self) {
        if let Some(handle) = &self.setup_thread {
            if handle.is_finished() {
                let handle = self.setup_thread.take().unwrap();
                match handle.join() {
                    Ok(Ok(())) => {
                        self.setup_state = SetupState::Ready;
                        self.refresh_devices();
                    }
                    Ok(Err(e)) => {
                        self.setup_state = SetupState::Failed(e);
                    }
                    Err(_) => {
                        self.setup_state =
                            SetupState::Failed("Setup thread panicked".to_string());
                    }
                }
            }
        }
    }

    fn start_pipeline(&mut self) {
        self.save_current_profile();

        // If pipeline is already running, just update gain
        if *self.pipeline_active.lock().unwrap() {
            *self.live_gain.lock().unwrap() = self.boost as f32 / 100.0;
            self.is_active = true;
            let out_name = self
                .output_devices
                .get(self.selected_output)
                .cloned()
                .unwrap_or_default();
            *self.status.lock().unwrap() = format!(
                "Boosting {:.0}x -> {}",
                self.boost as f32 / 100.0,
                out_name
            );
            return;
        }

        let input_device = self
            .input_devices
            .get(self.selected_input)
            .and_then(|name| Self::find_input_device_by_name(&self.host, name));

        let output_device = self
            .host
            .output_devices()
            .ok()
            .and_then(|mut devs| devs.nth(self.selected_output));

        let (input_device, output_device) = match (input_device, output_device) {
            (Some(i), Some(o)) => (i, o),
            _ => {
                *self.status.lock().unwrap() = "Could not open audio devices".to_string();
                return;
            }
        };

        let in_config = match input_device.default_input_config() {
            Ok(c) => c,
            Err(e) => {
                *self.status.lock().unwrap() = format!("Input config error: {}", e);
                return;
            }
        };

        let out_supported = match output_device.default_output_config() {
            Ok(c) => c,
            Err(e) => {
                *self.status.lock().unwrap() = format!("Output config error: {}", e);
                return;
            }
        };

        let in_channels = in_config.channels() as usize;
        let out_channels = out_supported.channels() as usize;
        let in_sample_rate = in_config.sample_rate();
        let out_sample_rate = out_supported.sample_rate();

        let out_config = cpal::StreamConfig {
            channels: out_supported.channels(),
            sample_rate: out_sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };

        let rate_ratio = in_sample_rate.0 as f64 / out_sample_rate.0 as f64;

        eprintln!(
            "Pipeline: in={}Hz out={}Hz ratio={:.4} in_ch={} out_ch={}",
            in_sample_rate.0, out_sample_rate.0, rate_ratio, in_channels, out_channels
        );

        // Reset ring buffer
        self.ring_buffer.reset();

        *self.pipeline_active.lock().unwrap() = true;

        let ring = self.ring_buffer.clone();
        let active = self.pipeline_active.clone();
        let gain_shared = self.live_gain.clone();
        let input_rms = self.live_input_rms.clone();
        let output_rms = self.live_output_rms.clone();
        let ng = self.noise_gate.clone();

        // Set gain to current boost level
        *self.live_gain.lock().unwrap() = self.boost as f32 / 100.0;

        // Speaker reference (loopback) for echo suppression
        let mut loopback_stream: Option<cpal::Stream> = None;
        self.loopback_note.clear();
        if self.settings.echo_suppress {
            match Self::build_loopback(
                &self.host,
                in_sample_rate.0,
                self.ref_ring.clone(),
                self.ref_anchor.clone(),
                self.pipeline_active.clone(),
                self.settings.echo_ref_device.as_deref(),
            ) {
                Ok((s, desc, name)) => {
                    loopback_stream = Some(s);
                    self.loopback_desc = desc;
                    self.loopback_name = name;
                }
                Err(e) => self.loopback_note = e,
            }
            self.ref_silent_since = None;
        }
        let use_ref = loopback_stream.is_some();
        let ref_ring = self.ref_ring.clone();
        let ref_anchor = self.ref_anchor.clone();
        let echo_shared = self.echo_shared.clone();
        let mut aec = echo::EchoCanceller::new(in_sample_rate.0);
        // Reference alignment by capture time: for the first mic sample of each
        // callback, read the reference sample captured REF_MARGIN earlier (so the
        // echo always lags the reference and the causal filter can model it),
        // then step through consecutively. Small discrepancies (clock drift) are
        // nudged out a couple of samples per callback; big ones re-sync.
        const REF_MARGIN: std::time::Duration = std::time::Duration::from_millis(15);
        let mic_rate_f = in_sample_rate.0 as f64;
        let resync_thresh = (in_sample_rate.0 / 200) as i64; // 5 ms
        let nudge: i64 = 2;
        let mut next_read: Option<u64> = None;
        let diag_rec = self.diag_rec.clone();
        self.diag_cap = in_sample_rate.0 as usize * 10;
        self.diag_rate = in_sample_rate.0;
        let diag_cap = self.diag_cap;
        let mut missing_ema = 0.0f32;

        let in_stream_config: cpal::StreamConfig = in_config.into();
        let input_stream = input_device.build_input_stream(
            &in_stream_config,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                if !*active.lock().unwrap() {
                    return;
                }
                let gain = *gain_shared.lock().unwrap();
                let mut gate = ng.lock().unwrap();
                let mut ref_pos: Option<u64> = None;
                if use_ref {
                    aec.set_params(
                        echo_shared.adaptive.load(Ordering::Relaxed),
                        echo::Shared::get_f32(&echo_shared.strength_db),
                    );
                    let t_mic = info.timestamp().capture;
                    if let Some((ts, pos)) = *ref_anchor.lock().unwrap() {
                        let target = t_mic.sub(REF_MARGIN).unwrap_or(t_mic);
                        let delta = match target.duration_since(&ts) {
                            Some(d) => (d.as_secs_f64() * mic_rate_f).round() as i64,
                            None => -(ts
                                .duration_since(&target)
                                .map(|d| (d.as_secs_f64() * mic_rate_f).round() as i64)
                                .unwrap_or(0)),
                        };
                        let desired = (pos as i64 + delta).max(0) as u64;
                        let cur = match next_read {
                            Some(c) => {
                                let diff = desired as i64 - c as i64;
                                if diff.abs() > resync_thresh {
                                    desired
                                } else if diff > nudge {
                                    c + nudge as u64
                                } else if diff < -nudge {
                                    c - nudge as u64
                                } else {
                                    c
                                }
                            }
                            None => desired,
                        };
                        ref_pos = Some(cur);
                        let off_ms = match t_mic.duration_since(&ts) {
                            Some(d) => d.as_secs_f32() * 1000.0,
                            None => -(ts.duration_since(&t_mic).map(|d| d.as_secs_f32()).unwrap_or(0.0) * 1000.0),
                        };
                        echo::Shared::set_f32(&echo_shared.ref_offset_ms, off_ms);
                    }
                }
                let mut sum_raw = 0.0f32;
                let mut sum_out = 0.0f32;
                let mut count = 0usize;
                let mut missing = 0usize;
                let mut diag_guard = diag_rec.lock().unwrap();
                let mut diag = diag_guard.as_mut();
                for chunk in data.chunks(in_channels) {
                    let mono = chunk[0];
                    let clean = if use_ref {
                        // None = not yet delivered or speakers idle (WASAPI
                        // loopback sends nothing then): treat as silence.
                        let r = match ref_pos.as_mut() {
                            Some(p) => {
                                let v = ref_ring.get(*p);
                                *p += 1;
                                match v {
                                    Some(v) => v,
                                    None => {
                                        missing += 1;
                                        0.0
                                    }
                                }
                            }
                            None => {
                                missing += 1;
                                0.0
                            }
                        };
                        if let Some(rec) = diag.as_mut() {
                            if rec.len() < diag_cap {
                                rec.push((mono, r));
                            }
                        }
                        aec.process(mono, r)
                    } else {
                        mono
                    };
                    let boosted = (clean * gain).clamp(-1.0, 1.0);
                    // Apply noise gate after boost
                    let gated = gate.process(boosted);
                    ring.push(gated);
                    sum_raw += mono * mono;
                    sum_out += gated * gated;
                    count += 1;
                }
                if use_ref {
                    next_read = ref_pos;
                    if count > 0 {
                        missing_ema += 0.1 * (missing as f32 / count as f32 - missing_ema);
                        echo::Shared::set_f32(&echo_shared.ref_missing, missing_ema);
                    }
                    aec.publish(&echo_shared);
                }
                if count > 0 {
                    let raw = (sum_raw / count as f32).sqrt();
                    let out = (sum_out / count as f32).sqrt();
                    let mut ir = input_rms.lock().unwrap();
                    let mut or = output_rms.lock().unwrap();
                    *ir = *ir * 0.8 + raw * 0.2;
                    *or = *or * 0.8 + out * 0.2;
                }
            },
            |e| eprintln!("Input error: {}", e),
            None,
        );

        let ring2 = self.ring_buffer.clone();
        let active2 = self.pipeline_active.clone();

        let output_stream = output_device.build_output_stream(
            &out_config,
            move |data: &mut [f32], _| {
                if !*active2.lock().unwrap() {
                    data.iter_mut().for_each(|s| *s = 0.0);
                    return;
                }

                for frame in data.chunks_mut(out_channels) {
                    let sample = if ring2.available() > 0 {
                        let s = ring2.peek(0);
                        ring2.advance(1);
                        s
                    } else {
                        0.0
                    };
                    for ch in frame.iter_mut() {
                        *ch = sample;
                    }
                }
            },
            |e| eprintln!("Output error: {}", e),
            None,
        );

        match (input_stream, output_stream) {
            (Ok(is), Ok(os)) => {
                if is.play().is_ok() && os.play().is_ok() {
                    *self.input_stream.lock().unwrap() = Some(is);
                    *self.output_stream.lock().unwrap() = Some(os);
                    *self.loopback_stream.lock().unwrap() = loopback_stream;
                    self.is_active = true;
                    let out_name = self
                        .output_devices
                        .get(self.selected_output)
                        .cloned()
                        .unwrap_or_default();
                    *self.status.lock().unwrap() = format!(
                        "Boosting {:.1}x -> {} ({}Hz {}ch -> {}Hz {}ch)",
                        self.boost as f32 / 100.0,
                        out_name,
                        in_sample_rate.0,
                        in_channels,
                        out_sample_rate.0,
                        out_channels,
                    );
                } else {
                    *self.status.lock().unwrap() = "Failed to start audio streams".to_string();
                }
            }
            (Err(e), _) => {
                *self.status.lock().unwrap() = format!("Input stream error: {}", e);
            }
            (_, Err(e)) => {
                *self.status.lock().unwrap() = format!("Output stream error: {}", e);
            }
        }
    }

    fn stop_pipeline(&mut self) {
        // Don't kill the streams — just set gain to 1x passthrough
        *self.live_gain.lock().unwrap() = 1.0;
        self.is_active = false;
        *self.status.lock().unwrap() = "Passthrough (1x)".to_string();
    }

    fn kill_pipeline(&mut self) {
        // Actually stop the audio streams (used on device switch)
        *self.pipeline_active.lock().unwrap() = false;
        *self.input_stream.lock().unwrap() = None;
        *self.output_stream.lock().unwrap() = None;
        *self.loopback_stream.lock().unwrap() = None;
        self.is_active = false;
        *self.status.lock().unwrap() = "Stopped".to_string();
    }

    fn save_current_profile(&mut self) {
        if let Some(name) = self.input_devices.get(self.selected_input) {
            let gate = self.noise_gate.lock().unwrap();
            let noise_floor_rms = if gate.is_calibrated() {
                Some(gate.noise_floor_rms())
            } else {
                None
            };
            let noise_gate_enabled = gate.enabled;
            drop(gate);

            self.device_profiles.insert(
                name.clone(),
                profiles::Profile {
                    boost: self.boost,
                    noise_floor_rms,
                    noise_gate_enabled,
                },
            );
            profiles::save(&self.device_profiles);
            self.settings.last_input_device = Some(name.clone());
            profiles::save_settings(&self.settings);
        }
    }

    fn load_profile_for_device(&mut self) {
        if let Some(name) = self.input_devices.get(self.selected_input) {
            if let Some(profile) = self.device_profiles.get(name) {
                self.boost = profile.boost;
                let mut gate = self.noise_gate.lock().unwrap();
                if let Some(floor) = profile.noise_floor_rms {
                    let headroom = gate.headroom;
                    gate.restore(floor, profile.noise_gate_enabled, headroom);
                } else {
                    gate.enabled = false;
                }
            }
        }
    }

    fn update_gain(&mut self) {
        if self.is_active {
            // Just update the shared gain — no need to restart pipeline
            *self.live_gain.lock().unwrap() = self.boost as f32 / 100.0;
            let out_name = self
                .output_devices
                .get(self.selected_output)
                .cloned()
                .unwrap_or_default();
            *self.status.lock().unwrap() = format!(
                "Boosting {:.0}x -> {}",
                self.boost as f32 / 100.0,
                out_name
            );
        }
    }

    fn start_calibration(&mut self) {
        // Keep pipeline running — calibration opens a second input stream for raw samples

        *self.cal_samples.lock().unwrap() = Vec::new();
        *self.cal_active.lock().unwrap() = true;
        *self.cal_rms_live.lock().unwrap() = 0.0;
        self.cal_phase = CalibrationPhase::Listening;
        self.cal_start = Some(std::time::Instant::now());
        // Pick a random phrase
        self.cal_phrase_idx =
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_millis() as usize)
                % CALIBRATION_PHRASES.len();

        let device = self
            .input_devices
            .get(self.selected_input)
            .and_then(|name| Self::find_input_device_by_name(&self.host, name))
            .or_else(|| self.host.default_input_device());

        if let Some(device) = device {
            if let Ok(supported_config) = device.default_input_config() {
                let config: cpal::StreamConfig = supported_config.clone().into();
                let input_channels = supported_config.channels() as usize;
                let samples = self.cal_samples.clone();
                let active = self.cal_active.clone();
                let rms_live = self.cal_rms_live.clone();
                let sample_rate = supported_config.sample_rate().0 as usize;

                if let Ok(stream) = device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        if !*active.lock().unwrap() {
                            return;
                        }
                        let mut s = samples.lock().unwrap();
                        for chunk in data.chunks(input_channels) {
                            let mono = chunk[0];
                            s.push(mono); // Raw, no boost
                        }
                        // Update live RMS over last ~0.3s
                        let window = sample_rate / 3;
                        if s.len() > window {
                            let recent = &s[s.len() - window..];
                            let sum_sq: f32 =
                                recent.iter().map(|x| x * x).sum();
                            let rms = (sum_sq / recent.len() as f32).sqrt();
                            *rms_live.lock().unwrap() = rms;
                        }
                    },
                    |e| eprintln!("Calibration error: {}", e),
                    None,
                ) {
                    if stream.play().is_ok() {
                        *self.cal_stream.lock().unwrap() = Some(stream);
                    }
                }
            }
        }
        *self.status.lock().unwrap() = "Calibrating — speak now...".to_string();
    }

    fn finish_calibration(&mut self) {
        *self.cal_active.lock().unwrap() = false;
        *self.cal_stream.lock().unwrap() = None;

        let raw_rms = {
            let samples = self.cal_samples.lock().unwrap();
            if samples.len() < 4800 {
                self.cal_phase = CalibrationPhase::Idle;
                *self.status.lock().unwrap() = "Calibration failed — not enough audio".to_string();
                return;
            }

            // Compute RMS of entire capture, ignoring silence (gate at -50 dBFS)
            let silence_gate = 0.003_f32; // ~-50 dBFS
            let voiced: Vec<f32> = samples
                .iter()
                .copied()
                .filter(|s| s.abs() > silence_gate)
                .collect();

            if voiced.len() < 2400 {
                self.cal_phase = CalibrationPhase::Idle;
                *self.status.lock().unwrap() =
                    "Calibration failed — couldn't detect speech. Try speaking louder.".to_string();
                return;
            }

            let sum_sq: f32 = voiced.iter().map(|x| x * x).sum();
            (sum_sq / voiced.len() as f32).sqrt()
        };

        // Calculate needed boost
        let needed = TARGET_RMS / raw_rms;
        let boost_pct = (needed * 100.0).round() as u32;
        let boost_pct = boost_pct.clamp(10, MAX_BOOST_PCT);

        let raw_db = 20.0 * raw_rms.log10();
        let boosted_rms = (raw_rms * boost_pct as f32 / 100.0).min(1.0);
        let boosted_db = 20.0 * boosted_rms.log10();
        let target_db = 20.0 * TARGET_RMS.log10();

        self.cal_phase = CalibrationPhase::Done {
            boost_pct,
            raw_db,
            boosted_db,
            target_db,
        };
        // Pipeline kept running — no need to restart
        *self.status.lock().unwrap() = format!(
            "Voice: {:.1} dB -> Boosted: {:.1} dB (target: {:.1} dB)",
            raw_db, boosted_db, target_db
        );
    }

    fn cancel_calibration(&mut self) {
        *self.cal_active.lock().unwrap() = false;
        *self.cal_stream.lock().unwrap() = None;
        self.cal_phase = CalibrationPhase::Idle;
        *self.status.lock().unwrap() = "Calibration cancelled".to_string();
    }

    fn start_noise_calibration(&mut self) {
        // Keep pipeline running — calibration opens a second input stream for raw samples

        {
            let mut cal = self.ng_cal_state.lock().unwrap();
            cal.active = true;
            cal.samples.clear();
        }
        self.ng_calibrating = true;
        self.ng_cal_start = Some(std::time::Instant::now());

        let device = self
            .input_devices
            .get(self.selected_input)
            .and_then(|name| Self::find_input_device_by_name(&self.host, name))
            .or_else(|| self.host.default_input_device());

        if let Some(device) = device {
            if let Ok(supported_config) = device.default_input_config() {
                let config: cpal::StreamConfig = supported_config.clone().into();
                let input_channels = supported_config.channels() as usize;
                let cal_state = self.ng_cal_state.clone();

                if let Ok(stream) = device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        let mut cal = cal_state.lock().unwrap();
                        if !cal.active {
                            return;
                        }
                        for chunk in data.chunks(input_channels) {
                            let mono = chunk[0];
                            cal.samples.push(mono);
                        }
                    },
                    |e| eprintln!("Noise cal error: {}", e),
                    None,
                ) {
                    if stream.play().is_ok() {
                        *self.ng_cal_stream.lock().unwrap() = Some(stream);
                    }
                }
            }
        }
        *self.status.lock().unwrap() = "Noise calibration — stay SILENT...".to_string();
    }

    fn finish_noise_calibration(&mut self) {
        {
            let mut cal = self.ng_cal_state.lock().unwrap();
            cal.active = false;
        }
        *self.ng_cal_stream.lock().unwrap() = None;
        self.ng_calibrating = false;
        self.ng_cal_start = None;

        let samples = {
            let cal = self.ng_cal_state.lock().unwrap();
            cal.samples.clone()
        };

        let mut gate = self.noise_gate.lock().unwrap();
        match gate.finish_calibration(&samples) {
            Ok(db) => {
                *self.status.lock().unwrap() = format!(
                    "Noise floor: {:.1} dBFS | Gate threshold: {:.1} dBFS | Gate ON",
                    db,
                    gate.threshold_db()
                );
            }
            Err(e) => {
                *self.status.lock().unwrap() = format!("Noise calibration failed: {}", e);
            }
        }
        drop(gate);

        // Save noise gate to profile (pipeline keeps running)
        self.save_current_profile();
    }

    fn cancel_noise_calibration(&mut self) {
        {
            let mut cal = self.ng_cal_state.lock().unwrap();
            cal.active = false;
        }
        *self.ng_cal_stream.lock().unwrap() = None;
        self.ng_calibrating = false;
        self.ng_cal_start = None;
        *self.status.lock().unwrap() = "Noise calibration cancelled".to_string();
    }

    fn start_recording(&mut self) {
        *self.rec_samples.lock().unwrap() = Vec::new();
        *self.rec_active.lock().unwrap() = true;
        self.is_recording = true;
        self.recording_start = Some(std::time::Instant::now());

        if self.is_active {
            // Pipeline is running — tap into the ring buffer
            let ring = self.ring_buffer.clone();
            let rec_samples = self.rec_samples.clone();
            let rec_active = self.rec_active.clone();
            self.sample_rate = 48000; // approximate, will be close enough

            std::thread::spawn(move || {
                let mut last_w = ring.write.load(Ordering::Acquire);
                while *rec_active.lock().unwrap() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    let w = ring.write.load(Ordering::Acquire);
                    let mut samples = rec_samples.lock().unwrap();
                    while last_w != w {
                        samples.push(ring.read_at(last_w));
                        last_w = (last_w + 1) % (RING_SIZE * 2);
                    }
                }
            });

            *self.status.lock().unwrap() = "Recording from boost pipeline...".to_string();
        } else {
            // Pipeline not running — open mic directly
            let device = self
                .input_devices
                .get(self.selected_input)
                .and_then(|name| Self::find_input_device_by_name(&self.host, name))
                .or_else(|| self.host.default_input_device());

            if let Some(device) = device {
                if let Ok(supported_config) = device.default_input_config() {
                    self.sample_rate = supported_config.sample_rate().0;
                    let config: cpal::StreamConfig = supported_config.clone().into();
                    let samples = self.rec_samples.clone();
                    let active = self.rec_active.clone();
                    let input_channels = supported_config.channels() as usize;
                    let gain = self.boost as f32 / 100.0;

                    if let Ok(stream) = device.build_input_stream(
                        &config,
                        move |data: &[f32], _| {
                            if *active.lock().unwrap() {
                                let mut s = samples.lock().unwrap();
                                for chunk in data.chunks(input_channels) {
                                    let mono =
                                        chunk[0];
                                    s.push((mono * gain).clamp(-1.0, 1.0));
                                }
                            }
                        },
                        |e| eprintln!("Audio error: {}", e),
                        None,
                    ) {
                        if stream.play().is_ok() {
                            *self.rec_stream.lock().unwrap() = Some(stream);
                        }
                    }
                }
            }
            *self.status.lock().unwrap() = "Recording test (with boost)...".to_string();
        }
    }

    fn stop_recording(&mut self) {
        *self.rec_active.lock().unwrap() = false;
        *self.rec_stream.lock().unwrap() = None;

        let samples = self.rec_samples.lock().unwrap().clone();
        if samples.is_empty() {
            *self.status.lock().unwrap() = "No audio recorded".to_string();
            return;
        }

        let folder =
            PathBuf::from(std::env::var("APPDATA").unwrap_or(".".to_string())).join("Microboost");
        let _ = std::fs::create_dir_all(&folder);

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let path = folder.join(format!("test_{}.wav", timestamp));

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: self.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        if let Ok(mut writer) = hound::WavWriter::create(&path, spec) {
            for sample in &samples {
                let amp = (*sample * i16::MAX as f32) as i16;
                let _ = writer.write_sample(amp);
            }
            let _ = writer.finalize();

            let elapsed = self
                .recording_start
                .map(|s| s.elapsed().as_secs())
                .unwrap_or(0);
            self.last_recording = Some(path);
            *self.status.lock().unwrap() = format!("Saved: {}s", elapsed);
        }

        self.is_recording = false;
        self.recording_start = None;
    }

    fn play_recording(&mut self) {
        if let Some(ref path) = self.last_recording {
            let path = path.clone();
            let status = self.status.clone();
            let samples = self.rec_samples.clone();
            let sample_rate = self.sample_rate;
            *status.lock().unwrap() = "Playing...".to_string();

            let host = cpal::default_host();
            let speaker = host.default_output_device();

            let speaker = match speaker {
                Some(s) => s,
                None => {
                    *status.lock().unwrap() = "No speaker found".to_string();
                    return;
                }
            };

            let out_config = match speaker.default_output_config() {
                Ok(c) => c,
                Err(e) => {
                    *status.lock().unwrap() = format!("Speaker error: {}", e);
                    return;
                }
            };

            let out_channels = out_config.channels() as usize;
            let out_rate = out_config.sample_rate().0;
            let config: cpal::StreamConfig = out_config.into();

            let samples_data = samples.lock().unwrap().clone();
            if samples_data.is_empty() {
                // Try loading from file
                if let Ok(mut reader) = hound::WavReader::open(&path) {
                    let loaded: Vec<f32> = reader
                        .samples::<i16>()
                        .filter_map(|s| s.ok())
                        .map(|s| s as f32 / i16::MAX as f32)
                        .collect();
                    if loaded.is_empty() {
                        *status.lock().unwrap() = "Empty recording".to_string();
                        return;
                    }
                    *samples.lock().unwrap() = loaded;
                } else {
                    *status.lock().unwrap() = "Could not read file".to_string();
                    return;
                }
            }

            let play_samples = samples.lock().unwrap().clone();
            let play_pos = Arc::new(Mutex::new(0usize));
            let play_done = Arc::new(Mutex::new(false));
            let done_clone = play_done.clone();
            let rate_ratio = sample_rate as f64 / out_rate as f64;
            let frac = Arc::new(Mutex::new(0.0f64));

            let stream = speaker.build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    let mut pos = play_pos.lock().unwrap();
                    let mut f = frac.lock().unwrap();
                    for frame in data.chunks_mut(out_channels) {
                        if *pos < play_samples.len() {
                            let sample = play_samples[*pos];
                            for ch in frame.iter_mut() {
                                *ch = sample;
                            }
                            *f += rate_ratio;
                            while *f >= 1.0 {
                                *f -= 1.0;
                                *pos += 1;
                            }
                        } else {
                            for ch in frame.iter_mut() {
                                *ch = 0.0;
                            }
                            *done_clone.lock().unwrap() = true;
                        }
                    }
                },
                |e| eprintln!("Playback error: {}", e),
                None,
            );

            match stream {
                Ok(stream) => {
                    if stream.play().is_ok() {
                        std::thread::spawn(move || {
                            // Wait until playback finishes
                            loop {
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                if *play_done.lock().unwrap() {
                                    break;
                                }
                            }
                            drop(stream);
                            *status.lock().unwrap() = "Ready".to_string();
                        });
                    } else {
                        *status.lock().unwrap() = "Failed to start playback".to_string();
                    }
                }
                Err(e) => {
                    *status.lock().unwrap() = format!("Playback error: {}", e);
                }
            }
        }
    }

    fn open_folder(&self) {
        let folder =
            PathBuf::from(std::env::var("APPDATA").unwrap_or(".".to_string())).join("Microboost");
        let _ = std::fs::create_dir_all(&folder);
        let _ = std::process::Command::new("explorer").arg(&folder).spawn();
    }
}

impl eframe::App for MicroboostApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll setup thread
        self.check_setup_thread();

        // Auto-start pipeline on first frame (boosting, or 1x passthrough if disabled)
        if self.first_frame && self.setup_state == SetupState::Ready {
            self.first_frame = false;
            self.start_pipeline();
            if !self.settings.auto_start_boost {
                self.stop_pipeline();
            }
        }

        // eframe shows the window after the first frame; hide it on the second if asked
        self.frame_count = self.frame_count.saturating_add(1);
        if self.frame_count == 1 {
            ctx.request_repaint();
        } else if self.frame_count == 2 && self.settings.minimize_to_tray && self.settings.start_in_tray {
            tray::hide_window();
        }

        // Finish an echo diagnostic recording (file I/O on the UI thread)
        self.finish_diag_recording();

        // Periodic device hot-plug detection
        if self.setup_state == SetupState::Ready
            && !self.ng_calibrating
            && self.cal_phase == CalibrationPhase::Idle
        {
            self.check_device_changes();
        }

        // Auto-finish calibration after 5 seconds
        if self.cal_phase == CalibrationPhase::Listening {
            if let Some(start) = self.cal_start {
                if start.elapsed().as_secs() >= 5 {
                    self.finish_calibration();
                }
            }
        }

        // Auto-finish noise calibration after 3 seconds
        if self.ng_calibrating {
            if let Some(start) = self.ng_cal_start {
                if start.elapsed().as_secs() >= 3 {
                    self.finish_noise_calibration();
                }
            }
        }

        let pipeline_running = *self.pipeline_active.lock().unwrap();
        if self.is_recording
            || self.is_active
            || pipeline_running
            || self.ng_calibrating
            || self.cal_phase == CalibrationPhase::Listening
            || matches!(self.setup_state, SetupState::Downloading)
        {
            ctx.request_repaint();
        } else if self.setup_state == SetupState::Ready {
            // Repaint every 2s for device hot-plug detection even when idle
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);

            match &self.setup_state {
                SetupState::NotInstalled => {
                    self.show_setup_screen(ui);
                }
                SetupState::Downloading => {
                    self.show_progress_screen(ui);
                }
                SetupState::Failed(err) => {
                    let err = err.clone();
                    self.show_failed_screen(ui, &err);
                }
                SetupState::Ready => {
                    self.show_main_screen(ui);
                }
            }
        });
    }
}

impl MicroboostApp {
    fn show_setup_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Setup Required");
        ui.add_space(12.0);

        ui.label("Microboost needs VB-CABLE (free) to route boosted audio to other apps.");
        ui.add_space(8.0);
        ui.label("VB-CABLE creates a virtual microphone that apps like Discord, Teams, etc. can use.");
        ui.add_space(16.0);

        ui.label("What will happen:");
        ui.label("  1. Download VB-CABLE (~1 MB)");
        ui.label("  2. Install it (admin prompt)");
        ui.label("  3. A new \"CABLE Output\" mic appears in Windows");
        ui.add_space(16.0);

        let btn = ui.add_sized(
            [320.0, 40.0],
            egui::Button::new(
                egui::RichText::new("Install VB-CABLE")
                    .color(egui::Color32::WHITE)
                    .strong(),
            )
            .fill(egui::Color32::from_rgb(60, 120, 200)),
        );
        if btn.clicked() {
            self.start_vbcable_install();
        }

        ui.add_space(8.0);
        if ui.link("Already have a virtual cable? Skip setup").clicked() {
            self.setup_state = SetupState::Ready;
            self.refresh_devices();
        }
    }

    fn show_progress_screen(&self, ui: &mut egui::Ui) {
        ui.heading("Setting up VB-CABLE...");
        ui.add_space(20.0);
        ui.spinner();
        ui.add_space(12.0);
        let status = self.status.lock().unwrap().clone();
        ui.label(&status);
        ui.add_space(8.0);
        ui.label("An admin prompt may appear — please accept it.");
    }

    fn show_failed_screen(&mut self, ui: &mut egui::Ui, err: &str) {
        ui.heading("Setup Failed");
        ui.add_space(12.0);
        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
        ui.add_space(16.0);

        if ui.button("Retry").clicked() {
            self.start_vbcable_install();
        }
        ui.add_space(8.0);
        if ui.link("Skip — I'll set up a virtual cable manually").clicked() {
            self.setup_state = SetupState::Ready;
            self.refresh_devices();
        }
    }

    fn show_main_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Microphone Boost");
        ui.add_space(4.0);

        // Input device
        ui.horizontal(|ui| {
            ui.label("Microphone");
            if let Some(name) = self.input_devices.get(self.selected_input) {
                if self.device_profiles.contains_key(name) {
                    ui.label(
                        egui::RichText::new("(saved profile)")
                            .small()
                            .color(egui::Color32::from_rgb(120, 180, 120)),
                    );
                }
            }
        });
        let prev_input = self.selected_input;
        egui::ComboBox::from_id_salt("input_device")
            .width(340.0)
            .selected_text(
                self.input_devices
                    .get(self.selected_input)
                    .cloned()
                    .unwrap_or("No input devices".to_string()),
            )
            .show_ui(ui, |ui| {
                for (i, name) in self.input_devices.iter().enumerate() {
                    ui.selectable_value(&mut self.selected_input, i, name);
                }
            });

        if prev_input != self.selected_input {
            // Save boost for old device, load for new one
            if let Some(old_name) = self.input_devices.get(prev_input) {
                let gate = self.noise_gate.lock().unwrap();
                let noise_floor_rms = if gate.is_calibrated() {
                    Some(gate.noise_floor_rms())
                } else {
                    None
                };
                let noise_gate_enabled = gate.enabled;
                drop(gate);
                self.device_profiles.insert(
                    old_name.clone(),
                    profiles::Profile {
                        boost: self.boost,
                        noise_floor_rms,
                        noise_gate_enabled,
                    },
                );
            }
            self.load_profile_for_device();
            profiles::save(&self.device_profiles);

            // Restart pipeline with new device
            let was_active = self.is_active;
            self.kill_pipeline();
            self.start_pipeline();
            if !was_active {
                // Was in passthrough mode — go back to passthrough
                self.stop_pipeline();
            }
        }

        ui.add_space(8.0);

        // Boost slider (up to MAX_BOOST_PCT)
        let boost_presets = [10, 100, 200, 500, 1000, 2000, 3000, 4000, 5000];
        let prev_boost = self.boost;
        ui.horizontal(|ui| {
            ui.label("Boost:");
            let drag = ui.add(
                egui::DragValue::new(&mut self.boost)
                    .range(10..=MAX_BOOST_PCT)
                    .speed(10)
                    .suffix("%"),
            );
            ui.label(format!("({:.1}x)", self.boost as f32 / 100.0));
            if drag.changed() {
                // Round to nearest 10
                self.boost = ((self.boost + 5) / 10) * 10;
                self.boost = self.boost.clamp(10, MAX_BOOST_PCT);
            }
            if drag.lost_focus() && self.is_active && self.boost != prev_boost {
                self.update_gain();
            }
        });

        ui.add_space(4.0);

        let slider_val = self.boost.clamp(10, MAX_BOOST_PCT);
        let mut slider_boost = slider_val;
        ui.push_id("boost_slider", |ui| {
            ui.spacing_mut().slider_width = 340.0;
            let resp = ui.add(
                egui::Slider::new(&mut slider_boost, 10..=MAX_BOOST_PCT)
                    .logarithmic(true)
                    .show_value(false)
                    .step_by(10.0)
                    .trailing_fill(true),
            );
            if resp.changed() {
                self.boost = slider_boost;
                for &step in &boost_presets {
                    if (self.boost as i32 - step as i32).abs() < 15 {
                        self.boost = step;
                        break;
                    }
                }
            }
            if resp.drag_stopped() && self.is_active {
                self.update_gain();
            }
        });

        ui.add_space(4.0);

        ui.horizontal(|ui| {
            for &preset in &boost_presets {
                let label = format!("{}x", preset as f32 / 100.0);
                if ui
                    .selectable_label(self.boost == preset, &label)
                    .clicked()
                {
                    self.boost = preset;
                    if self.is_active {
                        self.update_gain();
                    }
                }
            }
        });

        ui.add_space(4.0);

        // Auto-calibration section
        match &self.cal_phase {
            CalibrationPhase::Idle => {
                let cal_btn = ui.add_sized(
                    [340.0, 28.0],
                    egui::Button::new(
                        egui::RichText::new("Auto-Calibrate (detect my voice level)")
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(100, 80, 180)),
                );
                if cal_btn.clicked() {
                    self.start_calibration();
                }
            }
            CalibrationPhase::Listening => {
                let elapsed = self
                    .cal_start
                    .map(|s| s.elapsed().as_secs_f32())
                    .unwrap_or(0.0);
                let remaining = (5.0 - elapsed).max(0.0);

                ui.group(|ui| {
                    ui.set_width(340.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 200, 60),
                        "Speak now at your normal volume:",
                    );
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(
                            CALIBRATION_PHRASES[self.cal_phrase_idx],
                        )
                        .italics(),
                    );
                    ui.add_space(4.0);

                    // Live level meter
                    let live_rms = *self.cal_rms_live.lock().unwrap();
                    let db = if live_rms > 0.0001 {
                        20.0 * live_rms.log10()
                    } else {
                        -80.0
                    };
                    // Map -60dB..0dB to 0..1
                    let level = ((db + 60.0) / 60.0).clamp(0.0, 1.0);

                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(340.0, 12.0),
                        egui::Sense::hover(),
                    );
                    let painter = ui.painter();
                    painter.rect_filled(
                        rect,
                        3.0,
                        egui::Color32::from_rgb(40, 40, 40),
                    );
                    let bar_color = if level > 0.85 {
                        egui::Color32::from_rgb(220, 60, 60)
                    } else if level > 0.6 {
                        egui::Color32::from_rgb(60, 200, 60)
                    } else {
                        egui::Color32::from_rgb(60, 140, 200)
                    };
                    let bar_rect = egui::Rect::from_min_size(
                        rect.min,
                        egui::vec2(rect.width() * level, rect.height()),
                    );
                    painter.rect_filled(bar_rect, 3.0, bar_color);

                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.label(format!("{:.1}s remaining", remaining));
                        if ui.small_button("Cancel").clicked() {
                            self.cancel_calibration();
                        }
                    });
                });
            }
            CalibrationPhase::Done { boost_pct, raw_db, boosted_db, target_db } => {
                let boost_val = *boost_pct;
                let raw_db = *raw_db;
                let boosted_db = *boosted_db;
                let target_db = *target_db;
                ui.group(|ui| {
                    ui.set_width(340.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(100, 200, 100),
                        format!(
                            "Recommended boost: {:.1}x ({}%)",
                            boost_val as f32 / 100.0,
                            boost_val
                        ),
                    );

                    // Visual level comparison
                    ui.add_space(4.0);
                    let bar_width = 300.0;
                    // Map dBFS: -60..0 -> 0..1
                    let raw_frac = ((raw_db + 60.0) / 60.0).clamp(0.0, 1.0);
                    let boosted_frac = ((boosted_db + 60.0) / 60.0).clamp(0.0, 1.0);
                    let target_frac = ((target_db + 60.0) / 60.0).clamp(0.0, 1.0);

                    // Your voice level
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Your mic ").small());
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(bar_width, 10.0), egui::Sense::hover(),
                        );
                        let p = ui.painter();
                        p.rect_filled(rect, 2.0, egui::Color32::from_rgb(40, 40, 40));
                        p.rect_filled(
                            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * raw_frac, 10.0)),
                            2.0, egui::Color32::from_rgb(80, 130, 200),
                        );
                        // Target marker line
                        let tx = rect.min.x + rect.width() * target_frac;
                        p.line_segment(
                            [egui::pos2(tx, rect.min.y), egui::pos2(tx, rect.max.y)],
                            egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 200, 60)),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("           {:.1} dBFS", raw_db)).small()
                            .color(egui::Color32::from_rgb(80, 130, 200)));
                    });

                    // Boosted level
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Boosted  ").small());
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(bar_width, 10.0), egui::Sense::hover(),
                        );
                        let p = ui.painter();
                        p.rect_filled(rect, 2.0, egui::Color32::from_rgb(40, 40, 40));
                        p.rect_filled(
                            egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * boosted_frac, 10.0)),
                            2.0, egui::Color32::from_rgb(60, 200, 60),
                        );
                        let tx = rect.min.x + rect.width() * target_frac;
                        p.line_segment(
                            [egui::pos2(tx, rect.min.y), egui::pos2(tx, rect.max.y)],
                            egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 200, 60)),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(format!("           {:.1} dBFS", boosted_db)).small()
                            .color(egui::Color32::from_rgb(60, 200, 60)));
                        ui.label(egui::RichText::new(format!("  | target: {:.1} dBFS", target_db)).small()
                            .color(egui::Color32::from_rgb(255, 200, 60)));
                    });

                    if boost_val >= MAX_BOOST_PCT {
                        ui.label(
                            egui::RichText::new(
                                "Capped at 50x. Signal may clip — check with Record Test.",
                            )
                            .small()
                            .color(egui::Color32::from_rgb(180, 180, 120)),
                        );
                    }
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        if ui.button("Accept").clicked() {
                            self.boost = boost_val;
                            if *self.pipeline_active.lock().unwrap() {
                                self.update_gain();
                            } else {
                                self.start_pipeline();
                            }
                            self.save_current_profile();
                            self.cal_phase = CalibrationPhase::Idle;
                        }
                        if ui.button("Re-calibrate").clicked() {
                            self.start_calibration();
                        }
                        if ui.button("Dismiss").clicked() {
                            self.cal_phase = CalibrationPhase::Idle;
                        }
                    });
                });
            }
        }

        ui.add_space(4.0);

        // Noise gate
        if self.ng_calibrating {
            let elapsed = self.ng_cal_start
                .map(|s| s.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            let remaining = (3.0 - elapsed).max(0.0);
            ui.group(|ui| {
                ui.set_width(340.0);
                ui.colored_label(
                    egui::Color32::from_rgb(255, 200, 60),
                    "Stay SILENT — capturing background noise...",
                );
                ui.label(format!("{:.1}s remaining", remaining));
                if ui.small_button("Cancel").clicked() {
                    self.cancel_noise_calibration();
                }
            });
        } else {
            let mut ng_changed = false;
            ui.horizontal(|ui| {
                let mut gate = self.noise_gate.lock().unwrap();
                let is_calibrated = gate.is_calibrated();
                let floor_db = gate.noise_floor_db();

                let prev_enabled = gate.enabled;
                let mut enabled = gate.enabled;
                ui.checkbox(&mut enabled, "Noise Gate");
                gate.enabled = enabled && is_calibrated;
                if gate.enabled != prev_enabled {
                    ng_changed = true;
                }
                drop(gate);

                if is_calibrated {
                    ui.label(
                        egui::RichText::new(format!("floor: {:.0} dB", floor_db))
                            .small()
                            .color(if enabled {
                                egui::Color32::from_rgb(100, 200, 100)
                            } else {
                                egui::Color32::GRAY
                            }),
                    );
                } else {
                    ui.label(
                        egui::RichText::new("(not calibrated)")
                            .small()
                            .color(egui::Color32::GRAY),
                    );
                }
                if ui.small_button("Calibrate").clicked() {
                    self.start_noise_calibration();
                }
            });
            if ng_changed {
                self.save_current_profile();
            }
        }

        ui.add_space(4.0);

        // Start/Stop
        let btn_text = if self.is_active {
            "Stop Boost"
        } else {
            "Start Boost"
        };
        let btn_color = if self.is_active {
            egui::Color32::from_rgb(200, 60, 60)
        } else {
            egui::Color32::from_rgb(60, 160, 60)
        };
        let btn = ui.add_sized(
            [340.0, 36.0],
            egui::Button::new(
                egui::RichText::new(btn_text)
                    .color(egui::Color32::WHITE)
                    .strong(),
            )
            .fill(btn_color),
        );
        if btn.clicked() {
            if self.is_active {
                self.stop_pipeline();
            } else {
                self.start_pipeline();
            }
        }

        if self.is_active {
            ui.add_space(4.0);
            let cable_name = self
                .output_devices
                .get(self.selected_output)
                .map(|n| n.replace("CABLE Input", "CABLE Output"))
                .unwrap_or("CABLE Output".to_string());
            ui.colored_label(
                egui::Color32::from_rgb(100, 200, 100),
                format!("In Discord/Teams/etc, select \"{}\" as your mic", cable_name),
            );
        }

        // Live audio waveform visualizer — both waves overlaid
        if *self.pipeline_active.lock().unwrap() {
            ui.add_space(4.0);
            let in_rms = *self.live_input_rms.lock().unwrap();
            let out_rms = *self.live_output_rms.lock().unwrap();

            // Push to rolling history every 4 frames (4x slower scroll)
            self.vis_accum_in += in_rms;
            self.vis_accum_out += out_rms;
            self.vis_frame += 1;
            if self.vis_frame >= 4 {
                self.input_history.push(self.vis_accum_in / 4.0);
                self.output_history.push(self.vis_accum_out / 4.0);
                self.vis_frame = 0;
                self.vis_accum_in = 0.0;
                self.vis_accum_out = 0.0;
            }
            let max_points = 150;
            if self.input_history.len() > max_points {
                self.input_history.drain(0..self.input_history.len() - max_points);
            }
            if self.output_history.len() > max_points {
                self.output_history.drain(0..self.output_history.len() - max_points);
            }

            let wave_w = 370.0;
            let wave_h = 60.0;
            let in_color = egui::Color32::from_rgb(60, 140, 220);
            let out_color = egui::Color32::from_rgb(80, 220, 80);

            // Legend
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("---").small().color(in_color));
                ui.label(egui::RichText::new("Input").small());
                ui.label(egui::RichText::new("---").small().color(out_color));
                ui.label(egui::RichText::new("Output (boosted)").small());
            });

            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(wave_w, wave_h),
                egui::Sense::hover(),
            );
            let p = ui.painter();
            p.rect_filled(rect, 3.0, egui::Color32::from_rgb(20, 20, 25));

            let rms_to_frac = |rms: f32| -> f32 {
                if rms > 0.0001 {
                    ((20.0 * rms.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            };

            let make_points = |history: &[f32]| -> Vec<egui::Pos2> {
                let n = history.len();
                if n < 2 {
                    return vec![];
                }
                history
                    .iter()
                    .enumerate()
                    .map(|(i, &rms)| {
                        let x = rect.min.x + (i as f32 / (n - 1) as f32) * rect.width();
                        let frac = rms_to_frac(rms);
                        let y = rect.max.y - frac * rect.height();
                        egui::pos2(x, y)
                    })
                    .collect()
            };

            // Draw filled area between input and output (shows the boost difference)
            let in_pts = make_points(&self.input_history);
            let out_pts = make_points(&self.output_history);

            if in_pts.len() >= 2 && out_pts.len() >= 2 {
                // Fill between the two curves to show boost amount
                let n = in_pts.len().min(out_pts.len());
                let fill_color = egui::Color32::from_rgba_premultiplied(40, 180, 40, 25);
                // Build polygon strips column by column; skip when curves cross
                // to avoid bowtie artifacts from convex_polygon on non-convex quads
                for i in 0..n - 1 {
                    let in_above_l = in_pts[i].y <= out_pts[i].y;
                    let in_above_r = in_pts[i + 1].y <= out_pts[i + 1].y;
                    if in_above_l != in_above_r {
                        // Curves cross in this segment — skip fill to avoid artifacts
                        continue;
                    }
                    let quad = vec![
                        in_pts[i],
                        in_pts[i + 1],
                        out_pts[i + 1],
                        out_pts[i],
                    ];
                    p.add(egui::Shape::convex_polygon(
                        quad,
                        fill_color,
                        egui::Stroke::NONE,
                    ));
                }

                // Draw input line (thinner, behind)
                p.add(egui::Shape::line(
                    in_pts,
                    egui::Stroke::new(1.5, in_color),
                ));
                // Draw output line (thicker, on top)
                p.add(egui::Shape::line(
                    out_pts,
                    egui::Stroke::new(2.0, out_color),
                ));
            }

            // dB scale markers
            for &db in &[-40.0_f32, -20.0, -10.0] {
                let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                let y = rect.max.y - frac * rect.height();
                p.line_segment(
                    [egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
                    egui::Stroke::new(0.5, egui::Color32::from_rgb(50, 50, 55)),
                );
                p.text(
                    egui::pos2(rect.max.x - 22.0, y - 6.0),
                    egui::Align2::LEFT_TOP,
                    format!("{}dB", db as i32),
                    egui::FontId::new(8.0, egui::FontFamily::Monospace),
                    egui::Color32::from_rgb(80, 80, 90),
                );
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        // Profiles
        ui.collapsing("Saved Profiles", |ui| {
            let current_device = self.input_devices.get(self.selected_input).cloned();
            let mut to_delete: Option<String> = None;
            let mut to_load: Option<(String, u32)> = None;

            if self.device_profiles.is_empty() {
                ui.label(
                    egui::RichText::new("No saved profiles yet. Calibrate or start boost to save one.")
                        .small()
                        .color(egui::Color32::GRAY),
                );
            } else {
                let mut names: Vec<String> = self.device_profiles.keys().cloned().collect();
                names.sort();
                for name in &names {
                    let profile = &self.device_profiles[name];
                    let is_current = current_device.as_deref() == Some(name.as_str());
                    ui.horizontal(|ui| {
                        // Shorten long device names
                        let short_name = if name.len() > 30 {
                            format!("{}...", &name[..27])
                        } else {
                            name.clone()
                        };
                        let label = format!(
                            "{} — {:.1}x",
                            short_name,
                            profile.boost as f32 / 100.0
                        );
                        if is_current {
                            ui.label(
                                egui::RichText::new(&label)
                                    .small()
                                    .strong()
                                    .color(egui::Color32::from_rgb(100, 200, 100)),
                            );
                        } else if ui
                            .link(egui::RichText::new(&label).small())
                            .on_hover_text("Click to load this profile's boost level")
                            .clicked()
                        {
                            to_load = Some((name.clone(), profile.boost));
                        }
                        if ui.small_button("x").on_hover_text("Delete profile").clicked() {
                            to_delete = Some(name.clone());
                        }
                    });
                }
            }

            // Apply deferred actions
            if let Some(name) = to_delete {
                self.device_profiles.remove(&name);
                profiles::save(&self.device_profiles);
            }
            if let Some((_name, boost)) = to_load {
                self.boost = boost;
                if self.is_active {
                    self.update_gain();
                }
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.small_button("Save current").on_hover_text("Save boost for current mic").clicked() {
                    self.save_current_profile();
                }
            });
        });

        ui.add_space(4.0);

        // Test recording
        ui.horizontal(|ui| {
            let rec_text = if self.is_recording {
                let elapsed = self
                    .recording_start
                    .map(|s| s.elapsed().as_secs() as u32)
                    .unwrap_or(0);
                format!("Stop ({:02}:{:02})", elapsed / 60, elapsed % 60)
            } else {
                "Record Test".to_string()
            };

            if ui.button(&rec_text).clicked() {
                if self.is_recording {
                    self.stop_recording();
                } else {
                    self.start_recording();
                }
            }

            if ui
                .add_enabled(self.last_recording.is_some(), egui::Button::new("Play"))
                .clicked()
            {
                self.play_recording();
            }

            if ui.button("Folder").clicked() {
                self.open_folder();
            }

            if ui.button("Refresh Devices").clicked() {
                self.refresh_devices();
            }
        });

        ui.add_space(4.0);
        egui::CollapsingHeader::new("Speaker echo suppression")
            .default_open(true)
            .show(ui, |ui| {
                let mut restart = false;
                if ui
                    .checkbox(
                        &mut self.settings.echo_suppress,
                        "Remove my speakers' audio from the mic",
                    )
                    .on_hover_text(
                        "Captures what Windows is playing (podcasts, videos, call audio) \
                         and cancels or ducks it out of the mic. Adds ~11 ms latency.",
                    )
                    .changed()
                {
                    restart = true;
                }
                ui.add_enabled_ui(self.settings.echo_suppress, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Reference:")
                            .on_hover_text("The output device whose audio the mic picks up.");
                        let default_name = self
                            .host
                            .default_output_device()
                            .and_then(|d| d.name().ok())
                            .unwrap_or_default();
                        let selected = match &self.settings.echo_ref_device {
                            Some(n) => n.clone(),
                            None => format!("Windows default ({})", default_name),
                        };
                        let mut choice: Option<Option<String>> = None;
                        egui::ComboBox::from_id_salt("echo_ref_device")
                            .width(300.0)
                            .selected_text(selected)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(
                                        self.settings.echo_ref_device.is_none(),
                                        format!("Windows default ({})", default_name),
                                    )
                                    .clicked()
                                {
                                    choice = Some(None);
                                }
                                for name in &self.output_devices {
                                    if name.to_lowercase().contains("cable input") {
                                        continue; // our own output: would cancel the user
                                    }
                                    if ui
                                        .selectable_label(
                                            self.settings.echo_ref_device.as_deref() == Some(name),
                                            name,
                                        )
                                        .clicked()
                                    {
                                        choice = Some(Some(name.clone()));
                                    }
                                }
                            });
                        if let Some(c) = choice {
                            if c != self.settings.echo_ref_device {
                                self.settings.echo_ref_device = c;
                                restart = true;
                            }
                        }
                    });
                    if ui
                        .checkbox(
                            &mut self.settings.echo_adaptive,
                            "Adaptive cancellation (talk over media)",
                        )
                        .on_hover_text(
                            "On: subtracts the speaker audio, then suppresses the leftover per \
                             frequency band wherever it is louder than your voice, so you can \
                             talk while something plays. Off: mutes the mic by the strength \
                             below whenever the speakers are playing.",
                        )
                        .changed()
                    {
                        self.echo_shared
                            .adaptive
                            .store(self.settings.echo_adaptive, Ordering::Relaxed);
                        profiles::save_settings(&self.settings);
                    }
                    ui.horizontal(|ui| {
                        ui.label("Strength:");
                        let r = ui
                            .add(
                                egui::Slider::new(&mut self.settings.echo_strength_db, 6..=60)
                                    .suffix(" dB"),
                            )
                            .on_hover_text(
                                "Higher: more of the speaker audio removed, also while you talk, \
                                 at the cost of a slightly thinner voice during playback. \
                                 50 dB is a good start.",
                            );
                        if r.changed() {
                            echo::Shared::set_f32(
                                &self.echo_shared.strength_db,
                                self.settings.echo_strength_db as f32,
                            );
                        }
                        if r.drag_stopped() || r.lost_focus() {
                            profiles::save_settings(&self.settings);
                        }
                    });
                });
                let status = if !self.settings.echo_suppress {
                    "Off".to_string()
                } else if !self.loopback_note.is_empty() {
                    format!("Could not capture speaker output: {}", self.loopback_note)
                } else if !*self.pipeline_active.lock().unwrap() {
                    "Waiting for pipeline".to_string()
                } else {
                    let s = &self.echo_shared;
                    if !s.ref_active.load(Ordering::Relaxed) {
                        "Speakers: silent".to_string()
                    } else {
                        let erle = echo::Shared::get_f32(&s.erle_db);
                        let duck = echo::Shared::get_f32(&s.duck_db);
                        if !self.settings.echo_adaptive {
                            format!("Speakers: playing · ducking {:.0} dB", duck)
                        } else if s.echo_detected.load(Ordering::Relaxed) {
                            let who = if s.near_active.load(Ordering::Relaxed) {
                                "you are talking"
                            } else {
                                "only echo"
                            };
                            format!(
                                "Speakers: playing · echo in mic · cancelling {:.0} dB · suppressing leftover up to {:.0} dB · {} · ref {:+.0} ms",
                                erle, duck, who, echo::Shared::get_f32(&s.ref_offset_ms)
                            )
                        } else {
                            "Speakers: playing · no echo detected in mic".to_string()
                        }
                    }
                };
                ui.label(
                    egui::RichText::new(status)
                        .small()
                        .color(egui::Color32::from_rgb(140, 140, 150)),
                );
                if self.settings.echo_suppress && self.loopback_note.is_empty() {
                    // Reference silent while the mic clearly hears something:
                    // almost always the wrong output device being captured.
                    let ref_active = self.echo_shared.ref_active.load(Ordering::Relaxed);
                    let mic_loud = *self.live_input_rms.lock().unwrap() > 0.004;
                    if *self.pipeline_active.lock().unwrap() && !ref_active && mic_loud {
                        if self.ref_silent_since.is_none() {
                            self.ref_silent_since = Some(std::time::Instant::now());
                        }
                    } else {
                        self.ref_silent_since = None;
                    }
                    if self.ref_silent_since.map(|t| t.elapsed().as_secs() >= 3).unwrap_or(false) {
                        ui.label(
                            egui::RichText::new(format!(
                                "Nothing is playing on \"{}\" but the mic hears sound. If a video is \
                                 playing, pick the output device it uses as Reference above.",
                                self.loopback_name
                            ))
                            .small()
                            .color(egui::Color32::from_rgb(230, 180, 80)),
                        );
                    }
                    let s = &self.echo_shared;
                    let diag = format!(
                        "{} · corr {:.2} · lag {:+.0} ms · window {:.0}..{:.0} ms · ERLE max {:.0}/typ {:.0} dB · ref {:+.0} ms · missing {:.0}%",
                        self.loopback_desc,
                        echo::Shared::get_f32(&s.corr),
                        echo::Shared::get_f32(&s.lag_ms),
                        echo::Shared::get_f32(&s.window_ms),
                        echo::Shared::get_f32(&s.window_ms) + 85.0,
                        echo::Shared::get_f32(&s.erle_max_db),
                        echo::Shared::get_f32(&s.erle_db),
                        echo::Shared::get_f32(&s.ref_offset_ms),
                        echo::Shared::get_f32(&s.ref_missing) * 100.0,
                    );
                    ui.label(
                        egui::RichText::new(diag)
                            .small()
                            .color(egui::Color32::from_rgb(110, 110, 120)),
                    );
                    ui.horizontal(|ui| {
                        let recording = self.diag_rec.lock().unwrap().is_some();
                        if ui
                            .add_enabled(!recording, egui::Button::new("Record 10 s echo diagnostic"))
                            .on_hover_text(
                                "Saves raw mic (left) and the speaker reference (right) to \
                                 echo_diag.wav in the recordings folder, for offline analysis. \
                                 Play a video and stay silent while it records.",
                            )
                            .clicked()
                        {
                            *self.diag_rec.lock().unwrap() = Some(Vec::with_capacity(self.diag_cap));
                            self.diag_note = "Recording echo diagnostic…".to_string();
                        }
                        if !self.diag_note.is_empty() {
                            ui.label(egui::RichText::new(&self.diag_note).small());
                        }
                    });
                }
                if restart {
                    profiles::save_settings(&self.settings);
                    let was_active = self.is_active;
                    self.kill_pipeline();
                    self.start_pipeline();
                    if !was_active {
                        self.stop_pipeline();
                    }
                }
            });

        ui.add_space(4.0);
        ui.collapsing("Settings", |ui| {
            let mut changed = false;
            changed |= ui
                .checkbox(&mut self.settings.auto_start_boost, "Auto-start boost on launch")
                .changed();
            if ui
                .checkbox(
                    &mut self.settings.minimize_to_tray,
                    "Minimise / close to tray (keeps boosting in background)",
                )
                .changed()
            {
                changed = true;
                tray::set_enabled(self.settings.minimize_to_tray);
            }
            ui.add_enabled_ui(self.settings.minimize_to_tray, |ui| {
                changed |= ui
                    .checkbox(&mut self.settings.start_in_tray, "Start hidden in tray")
                    .changed();
            });
            if ui
                .checkbox(&mut self.launch_with_windows, "Launch with Windows")
                .on_hover_text("Adds/removes this exe in HKCU\\...\\CurrentVersion\\Run")
                .changed()
            {
                let result = if self.launch_with_windows {
                    autostart::enable()
                } else {
                    autostart::disable()
                };
                if let Err(e) = result {
                    self.launch_with_windows = autostart::is_enabled();
                    *self.status.lock().unwrap() = e;
                }
            }
            ui.label(
                egui::RichText::new(
                    "Quit from the tray icon's right-click menu. Left-click the icon to reopen.",
                )
                .small()
                .color(egui::Color32::from_rgb(140, 140, 150)),
            );
            if changed {
                profiles::save_settings(&self.settings);
            }
        });

        ui.add_space(4.0);
        let status = self.status.lock().unwrap().clone();
        ui.label(&status);

        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("v{} ({})", env!("CARGO_PKG_VERSION"), env!("BUILD_TIMESTAMP")))
                .small()
                .color(egui::Color32::from_rgb(90, 90, 100)),
        );
    }
}
