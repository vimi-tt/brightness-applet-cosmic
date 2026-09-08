// SPDX-License-Identifier: MPL-2.0
//
// COSMIC Desktop Brightness Control Applet
// Designed for System76 COSMIC Desktop (libcosmic / iced).
// Supports AMD iGPU internal displays (/sys/class/backlight/amdgpu_bl*) and external DDC/CI displays (/dev/i2c-*).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex as StdMutex, RwLock};
use std::time::Duration;

use cosmic::iced::mouse::ScrollDelta;
use cosmic::iced::platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup};
use cosmic::iced::{window::Id, Alignment, Length, Limits, Subscription};
use cosmic::prelude::*;
use cosmic::widget;
use tokio::sync::Mutex as TokioMutex;

// VESA MCCS (Monitor Control Command Set) VCP Code for Brightness (Luminance)
const VCP_BRIGHTNESS: u8 = 0x10;

/// Resolves the appropriate symbolic icon based on brightness percentage.
pub fn brightness_icon_name(percentage: u32) -> &'static str {
    match percentage {
        0..=10 => "display-brightness-off-symbolic",
        11..=35 => "display-brightness-low-symbolic",
        36..=70 => "display-brightness-medium-symbolic",
        _ => "display-brightness-high-symbolic",
    }
}

/// Hardware backend used to control the display brightness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorBackend {
    /// Internal eDP display controlled via sysfs (/sys/class/backlight) and systemd-logind.
    Internal {
        sysfs_name: String,
        max_raw: u32,
    },
    /// External monitor controlled via DDC/CI over Linux I2C device node.
    DdcCi {
        i2c_path: PathBuf,
        bus_num: u32,
        max_val: u16,
    },
    /// External display/TV without DDC/CI controlled via software brightness (xrandr gamma scaling).
    Software {
        output_name: String,
    },
}

impl MonitorBackend {
    /// Unique stable identifier string for the backend instance.
    pub fn id(&self) -> String {
        match self {
            MonitorBackend::Internal { sysfs_name, .. } => format!("sysfs:{}", sysfs_name),
            MonitorBackend::DdcCi { bus_num, .. } => format!("ddc:/dev/i2c-{}", bus_num),
            MonitorBackend::Software { output_name } => format!("software:{}", output_name),
        }
    }
}

/// Tracks the latest requested target brightness per monitor to debounce rapid slider drags.
static TARGET_BRIGHTNESS: LazyLock<RwLock<HashMap<String, u32>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Per-monitor async mutex locks to prevent concurrent hardware/driver access collisions.
static MONITOR_LOCKS: LazyLock<StdMutex<HashMap<String, Arc<TokioMutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn get_monitor_lock(id: &str) -> Arc<TokioMutex<()>> {
    let mut locks = MONITOR_LOCKS.lock().unwrap();
    locks.entry(id.to_string()).or_default().clone()
}

/// Representation of an active display monitor.
#[derive(Debug, Clone)]
pub struct DisplayMonitor {
    pub id: String,
    pub name: String,
    pub backend: MonitorBackend,
    pub brightness: u32, // 1..=100 %
    pub is_primary: bool,
    pub is_accessible: bool,
    pub is_supported: bool,
}

/// Messages handled by the COSMIC Applet event loop.
#[derive(Debug, Clone)]
pub enum Message {
    /// Toggles the applet popup window visibility.
    TogglePopup,
    /// Closed notification when the popup is dismissed.
    PopupClosed(Id),
    /// Mouse wheel scroll intercepted directly on the panel icon.
    Scrolled(ScrollDelta),
    /// User dragged or changed an independent monitor slider.
    BrightnessChanged {
        index: usize,
        percentage: u32,
    },
    /// Triggers an asynchronous re-enumeration of displays.
    RefreshMonitors,
    /// Result received from background monitor discovery.
    MonitorsDiscovered(Vec<DisplayMonitor>),
    /// Result received after setting hardware brightness.
    BrightnessApplied {
        id: String,
        result: Result<(), String>,
    },
    /// Periodic tick to keep monitor statuses up-to-date.
    Tick,
}

// ============================================================================
// Asynchronous Hardware & Driver Interfacing Logic
// ============================================================================

/// Representation of an output queried from xrandr.
#[derive(Debug, Clone)]
struct XrandrOutput {
    name: String,
    is_connected: bool,
    is_primary: bool,
    brightness: Option<u32>,
}

/// Tests if an X11 DISPLAY can be connected to via xrandr.
fn test_xrandr(display: &str) -> bool {
    let mut cmd = std::process::Command::new("xrandr");
    cmd.arg("--current").env("DISPLAY", display);
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Detects the active working X11 DISPLAY socket (e.g. :1 for Wayland user session).
fn detect_x11_display() -> String {
    if let Ok(disp) = std::env::var("DISPLAY")
        && !disp.is_empty()
        && test_xrandr(&disp)
    {
        return disp;
    }
    if test_xrandr(":1") {
        return ":1".to_string();
    }
    if test_xrandr(":0") {
        return ":0".to_string();
    }
    if let Ok(entries) = std::fs::read_dir("/tmp/.X11-unix") {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if let Some(num) = fname.strip_prefix('X') {
                let candidate = format!(":{}", num);
                if test_xrandr(&candidate) {
                    return candidate;
                }
            }
        }
    }
    std::env::var("DISPLAY").unwrap_or_else(|_| ":1".to_string())
}

/// Queries all outputs from xrandr.
fn get_xrandr_outputs(display: &str, verbose: bool) -> Vec<XrandrOutput> {
    let mut cmd = std::process::Command::new("xrandr");
    if verbose {
        cmd.arg("--verbose");
    } else {
        cmd.arg("--current");
    }
    cmd.env("DISPLAY", display);

    let output = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut list = Vec::new();
    let mut current: Option<XrandrOutput> = None;

    for line in stdout.lines() {
        let trimmed = line.trim();
        if !line.starts_with(' ') && !line.starts_with('\t') {
            if let Some(prev) = current.take() {
                list.push(prev);
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && (parts[1] == "connected" || parts[1] == "disconnected") {
                current = Some(XrandrOutput {
                    name: parts[0].to_string(),
                    is_connected: parts[1] == "connected",
                    is_primary: parts.contains(&"primary"),
                    brightness: None,
                });
            }
        } else if let Some(ref mut cur) = current
            && let Some(val_str) = trimmed.strip_prefix("Brightness:")
            && let Ok(val) = val_str.trim().parse::<f64>()
        {
            let pct = (val * 100.0).round().clamp(1.0, 100.0) as u32;
            cur.brightness = Some(pct);
        }
    }
    if let Some(last) = current {
        list.push(last);
    }
    list
}

/// Normalizes DRM connector names and X11 output names for cross-comparison.
fn normalize_connector(s: &str) -> String {
    let s = s.strip_prefix("card").and_then(|r| r.split_once('-')).map(|(_, r)| r).unwrap_or(s);
    let alphanum: String = s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_uppercase();
    alphanum.replace("HDMIA", "HDMI").replace("DPA", "DP")
}

/// Resolves the actual xrandr output name that corresponds to a DRM connector name.
fn resolve_xrandr_output_name(outputs: &[XrandrOutput], drm_name: &str) -> Option<String> {
    let connected: Vec<&XrandrOutput> = outputs.iter().filter(|o| o.is_connected).collect();
    if connected.is_empty() {
        return None;
    }

    // 1. Exact match
    if let Some(found) = connected.iter().find(|o| o.name.eq_ignore_ascii_case(drm_name)) {
        return Some(found.name.clone());
    }

    // 2. Normalized alias match (e.g. HDMI-A-1 vs HDMI-1)
    let norm_target = normalize_connector(drm_name);
    if let Some(found) = connected.iter().find(|o| normalize_connector(&o.name) == norm_target) {
        return Some(found.name.clone());
    }

    // 3. Port type prefix match (e.g. both start with HDMI or DP)
    let drm_upper = drm_name.to_uppercase();
    for prefix in &["HDMI", "DP", "EDP", "VGA", "DVI"] {
        if drm_upper.contains(prefix)
            && let Some(found) = connected.iter().find(|o| o.name.to_uppercase().contains(prefix))
        {
            return Some(found.name.clone());
        }
    }

    // 4. Single connected output fallback (e.g. XWAYLAND0 or default)
    if connected.len() == 1 {
        return Some(connected[0].name.clone());
    }

    // 5. Primary connected output
    if let Some(primary) = connected.iter().find(|o| o.is_primary) {
        return Some(primary.name.clone());
    }

    // 6. First connected output
    Some(connected[0].name.clone())
}

/// Queries the current software brightness for an output from xrandr (in percentage 1..=100).
fn get_xrandr_brightness(output_name: &str) -> Option<u32> {
    let display = detect_x11_display();
    let outputs = get_xrandr_outputs(&display, true);
    let target_name = resolve_xrandr_output_name(&outputs, output_name)?;
    let output_info = outputs.into_iter().find(|o| o.name == target_name)?;
    output_info.brightness
}

/// Applies software brightness using xrandr with automatic output resolution and retry.
fn apply_xrandr_brightness(output_name: &str, percentage: u32) -> Result<(), String> {
    let display = detect_x11_display();
    let factor = ((percentage as f64).max(5.0) / 100.0).clamp(0.05, 1.0);
    let factor_str = format!("{:.2}", factor);

    let mut outputs = get_xrandr_outputs(&display, false);
    if outputs.is_empty() {
        std::thread::sleep(Duration::from_millis(80));
        outputs = get_xrandr_outputs(&display, false);
    }

    let target_name = resolve_xrandr_output_name(&outputs, output_name)
        .unwrap_or_else(|| output_name.to_string());

    let mut cmd = std::process::Command::new("xrandr");
    cmd.args(["--output", &target_name, "--brightness", &factor_str]);
    cmd.env("DISPLAY", &display);

    let res = cmd.output().map_err(|e| format!("Falha ao executar xrandr: {e}"))?;
    if res.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&res.stderr);

    // If it failed (e.g. output not found or no crtc during mode switch), retry with fresh outputs
    std::thread::sleep(Duration::from_millis(100));
    let fresh_outputs = get_xrandr_outputs(&display, false);
    if let Some(new_target) = resolve_xrandr_output_name(&fresh_outputs, output_name) {
        let mut retry_cmd = std::process::Command::new("xrandr");
        retry_cmd.args(["--output", &new_target, "--brightness", &factor_str]);
        retry_cmd.env("DISPLAY", &display);
        if let Ok(retry_res) = retry_cmd.output()
            && retry_res.status.success()
        {
            return Ok(());
        }
    }

    Err(format!(
        "xrandr falhou ao ajustar brilho em {target_name} (DISPLAY={display}): {stderr}"
    ))
}

/// Discovers connected displays asynchronously.
/// - Enumerates internal laptop backlight via `/sys/class/backlight/*`.
/// - Enumerates external monitors via DDC/CI `/dev/i2c-*` and DRM EDID fallback.
async fn discover_monitors() -> Vec<DisplayMonitor> {
    tokio::task::spawn_blocking(scan_monitors_sync)
        .await
        .unwrap_or_default()
}

/// Synchronous probe routine executed in a worker thread.
fn scan_monitors_sync() -> Vec<DisplayMonitor> {
    let mut monitors = Vec::new();
    let mut detected_buses = HashSet::new();

    // Check if internal laptop screen (eDP/LVDS) is actually connected and enabled
    let internal_is_enabled = if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        let mut any_internal = false;
        let mut enabled = false;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains("eDP") || name.contains("LVDS") || name.contains("DSI") {
                any_internal = true;
                let en_path = entry.path().join("enabled");
                if let Ok(s) = std::fs::read_to_string(&en_path)
                    && s.trim() == "enabled"
                {
                    enabled = true;
                }
            }
        }
        if any_internal { enabled } else { true }
    } else {
        true
    };

    // 1. Enumerate Internal Displays (/sys/class/backlight/*)
    if let Ok(entries) = std::fs::read_dir("/sys/class/backlight") {
        for entry in entries.flatten() {
            let sysfs_name = entry.file_name().to_string_lossy().to_string();
            let base_path = entry.path();
            let max_path = base_path.join("max_brightness");
            let cur_path = base_path.join("brightness");

            let max_raw: u32 = std::fs::read_to_string(&max_path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(255);

            let cur_raw: u32 = std::fs::read_to_string(&cur_path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(max_raw / 2);

            let percentage = if max_raw > 0 {
                ((cur_raw as f64 / max_raw as f64) * 100.0)
                    .round()
                    .clamp(1.0, 100.0) as u32
            } else {
                50
            };

            let name = if sysfs_name.starts_with("amdgpu") {
                "Tela Integrada (AMD Radeon)".to_string()
            } else if sysfs_name.starts_with("intel") {
                "Tela Integrada (Intel HD)".to_string()
            } else {
                format!("Tela Interna ({})", sysfs_name)
            };

            monitors.push(DisplayMonitor {
                id: format!("sysfs:{}", sysfs_name),
                name,
                backend: MonitorBackend::Internal {
                    sysfs_name,
                    max_raw,
                },
                brightness: percentage,
                is_primary: internal_is_enabled && monitors.is_empty(),
                is_accessible: true,
                is_supported: true,
            });
        }
    }

    // 2. Enumerate External Displays via DRM connectors and DDC/CI buses
    if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
        for entry in entries.flatten() {
            let conn_name = entry.file_name().to_string_lossy().to_string();
            // Skip non-connectors or internal eDP laptop screens already handled by backlight
            if !conn_name.contains('-') || conn_name.contains("eDP") {
                continue;
            }

            let status_path = entry.path().join("status");
            if let Ok(status) = std::fs::read_to_string(&status_path) {
                if status.trim() != "connected" {
                    continue;
                }
            } else {
                continue;
            }

            // Find mapped DDC I2C bus link
            let ddc_symlink = entry.path().join("ddc");
            let i2c_path = if let Ok(target) = std::fs::read_link(&ddc_symlink) {
                let bus_filename = target
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if bus_filename.starts_with("i2c-") {
                    PathBuf::from(format!("/dev/{}", bus_filename))
                } else {
                    continue;
                }
            } else {
                continue;
            };

            let bus_num: u32 = i2c_path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|s| s.strip_prefix("i2c-"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            detected_buses.insert(bus_num);

            // Read and decode EDID data for exact model name
            let edid_path = entry.path().join("edid");
            let mut model_name = None;
            if let Ok(edid_bytes) = std::fs::read(&edid_path)
                && !edid_bytes.is_empty()
                && let Ok(info) = ddc_hi::DisplayInfo::from_edid(
                    ddc_hi::Backend::I2cDevice,
                    format!("i2c-{}", bus_num),
                    edid_bytes,
                )
            {
                model_name = info.model_name.or(info.manufacturer_id);
            }

            let output_name = conn_name
                .split_once('-')
                .map(|(_, c)| c)
                .unwrap_or(&conn_name)
                .to_string();

            let clean_name = model_name.unwrap_or_else(|| {
                format!("Monitor Externo ({output_name})")
            });

            // Check if /dev/i2c-X is accessible by current user
            let is_accessible = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&i2c_path)
                .is_ok();

            let mut current_brightness = 50;
            let mut max_val = 100;
            let mut ddc_supported = false;

            // Probe brightness via DDC/CI if bus is accessible
            if is_accessible
                && let Ok(mut ddc) = ddc_i2c::from_i2c_device(&i2c_path)
            {
                use ddc_hi::Ddc;
                let vcp_res = ddc.get_vcp_feature(VCP_BRIGHTNESS).or_else(|_| {
                    std::thread::sleep(Duration::from_millis(40));
                    ddc.get_vcp_feature(VCP_BRIGHTNESS)
                });
                if let Ok(vcp) = vcp_res {
                    max_val = vcp.maximum();
                    if max_val > 0 {
                        current_brightness =
                            ((vcp.value() as f64 / max_val as f64) * 100.0).round() as u32;
                    }
                    ddc_supported = true;
                }
            }

            let is_primary = !monitors.iter().any(|m| m.is_primary);
            if ddc_supported {
                monitors.push(DisplayMonitor {
                    id: format!("ddc:/dev/i2c-{}", bus_num),
                    name: clean_name,
                    backend: MonitorBackend::DdcCi {
                        i2c_path,
                        bus_num,
                        max_val,
                    },
                    brightness: current_brightness.clamp(1, 100),
                    is_primary,
                    is_accessible: true,
                    is_supported: true,
                });
            } else {
                // Fallback: Software Brightness via xrandr for TVs or displays without DDC/CI
                let sw_brightness = get_xrandr_brightness(&output_name).unwrap_or(100);
                monitors.push(DisplayMonitor {
                    id: format!("software:{}", output_name),
                    name: clean_name,
                    backend: MonitorBackend::Software {
                        output_name,
                    },
                    brightness: sw_brightness.clamp(1, 100),
                    is_primary,
                    is_accessible: true,
                    is_supported: true,
                });
            }
        }
    }

    // 3. Fallback: Query any remaining DDC/CI displays via ddc-hi enumeration
    for mut display in ddc_hi::Display::enumerate() {
        use std::os::unix::fs::MetadataExt;
        #[allow(irrefutable_let_patterns)]
        if let ddc_hi::Handle::I2cDevice(ref ddc) = display.handle {
            let meta = ddc.inner_ref().inner_ref().metadata().ok();
            let bus_num = meta.map(|m| (m.rdev() & 0xff) as u32).unwrap_or(999);

            if detected_buses.contains(&bus_num) {
                continue;
            }
            detected_buses.insert(bus_num);

            let i2c_path = PathBuf::from(format!("/dev/i2c-{}", bus_num));
            let mut current_brightness = 50;
            let mut max_val = 100;

            use ddc_hi::Ddc;
            if let Ok(vcp) = display.handle.get_vcp_feature(VCP_BRIGHTNESS) {
                max_val = vcp.maximum();
                if max_val > 0 {
                    current_brightness =
                        ((vcp.value() as f64 / max_val as f64) * 100.0).round() as u32;
                }
            }

            let name = display
                .info
                .model_name
                .or(display.info.manufacturer_id)
                .unwrap_or_else(|| format!("Monitor Externo (i2c-{})", bus_num));

            let is_primary = !monitors.iter().any(|m| m.is_primary);
            monitors.push(DisplayMonitor {
                id: format!("ddc:/dev/i2c-{}", bus_num),
                name,
                backend: MonitorBackend::DdcCi {
                    i2c_path,
                    bus_num,
                    max_val,
                },
                brightness: current_brightness.clamp(1, 100),
                is_primary,
                is_accessible: true,
                is_supported: true,
            });
        }
    }

    monitors
}

/// Asynchronously sets the brightness on the appropriate display backend.
async fn apply_brightness(backend: MonitorBackend, percentage: u32) -> Result<(), String> {
    let id = backend.id();
    let lock = get_monitor_lock(&id);
    let _guard = lock.lock().await;

    // Discard stale in-flight events if a newer value was queued while waiting
    let latest_requested = {
        let targets = TARGET_BRIGHTNESS.read().unwrap();
        targets.get(&id).copied().unwrap_or(percentage)
    };
    if latest_requested != percentage {
        return Ok(());
    }

    match backend {
        MonitorBackend::Internal { sysfs_name, max_raw } => {
            let raw_value = ((percentage as f64 / 100.0) * max_raw as f64).round() as u32;
            let raw_value = raw_value.clamp(1, max_raw);

            // Attempt 1: Direct sysfs write (fastest if udev rule or group video is active)
            let sysfs_path = format!("/sys/class/backlight/{}/brightness", sysfs_name);
            if tokio::fs::write(&sysfs_path, raw_value.to_string())
                .await
                .is_ok()
            {
                return Ok(());
            }

            // Attempt 2: systemd-logind D-Bus SetBrightness (works standard without root)
            match zbus::Connection::system().await {
                Ok(connection) => {
                    match zbus::Proxy::new(
                        &connection,
                        "org.freedesktop.login1",
                        "/org/freedesktop/login1/session/auto",
                        "org.freedesktop.login1.Session",
                    )
                    .await
                    {
                        Ok(proxy) => {
                            let call_res: Result<(), zbus::Error> = proxy
                                .call(
                                    "SetBrightness",
                                    &("backlight", &sysfs_name, raw_value),
                                )
                                .await;
                            match call_res {
                                Ok(()) => Ok(()),
                                Err(e) => Err(format!("D-Bus SetBrightness falhou: {e}")),
                            }
                        }
                        Err(e) => Err(format!("Falha ao instanciar proxy login1: {e}")),
                    }
                }
                Err(e) => Err(format!("Falha ao conectar no barramento D-Bus: {e}")),
            }
        }
        MonitorBackend::DdcCi {
            i2c_path,
            bus_num,
            max_val,
        } => {
            tokio::task::spawn_blocking(move || {
                let target_vcp = ((percentage as f64 / 100.0) * max_val as f64).round() as u16;
                let target_vcp = target_vcp.clamp(0, max_val);

                // 1. Check file access permissions first
                match std::fs::OpenOptions::new().read(true).write(true).open(&i2c_path) {
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        return Err(format!(
                            "Acesso negado em {}. Configure o grupo 'i2c' e as regras udev.",
                            i2c_path.display()
                        ));
                    }
                    Err(e) => {
                        return Err(format!("Erro ao acessar {}: {e}", i2c_path.display()));
                    }
                    Ok(_) => {}
                }

                // Attempt 1: Direct DDC/CI over /dev/i2c-*
                if let Ok(mut ddc) = ddc_i2c::from_i2c_device(&i2c_path) {
                    use ddc_hi::Ddc;
                    if ddc.set_vcp_feature(VCP_BRIGHTNESS, target_vcp).is_ok() {
                        return Ok(());
                    }
                    // Bus recovery wait & single retry
                    std::thread::sleep(Duration::from_millis(40));
                    if ddc.set_vcp_feature(VCP_BRIGHTNESS, target_vcp).is_ok() {
                        return Ok(());
                    }
                }

                // Attempt 2: Fallback to ddcutil CLI if installed on the host
                if let Ok(output) = std::process::Command::new("ddcutil")
                    .args([
                        "setvcp",
                        &VCP_BRIGHTNESS.to_string(),
                        &target_vcp.to_string(),
                        "--bus",
                        &bus_num.to_string(),
                    ])
                    .output()
                    && output.status.success()
                {
                    return Ok(());
                }

                Err(format!(
                    "Falha de comunicação DDC/CI em {}. O dispositivo não respondeu aos comandos (comum em TVs ou monitores com DDC/CI desativado).",
                    i2c_path.display()
                ))
            })
            .await
            .map_err(|e| format!("Falha de execução assíncrona: {e}"))?
        }
        MonitorBackend::Software { output_name } => {
            tokio::task::spawn_blocking(move || apply_xrandr_brightness(&output_name, percentage))
                .await
                .map_err(|e| format!("Falha de execução assíncrona: {e}"))?
        }
    }
}

// ============================================================================
// COSMIC Applet Model & UI Implementation
// ============================================================================

/// The main application model managing the brightness applet lifecycle and UI.
pub struct BrightnessApplet {
    core: cosmic::Core,
    popup: Option<Id>,
    monitors: Vec<DisplayMonitor>,
}

impl BrightnessApplet {
    /// Returns the current brightness percentage of the primary monitor.
    fn primary_brightness(&self) -> u32 {
        self.monitors
            .iter()
            .find(|m| m.is_primary && m.is_accessible && m.is_supported)
            .or_else(|| self.monitors.iter().find(|m| m.is_accessible && m.is_supported))
            .or_else(|| self.monitors.first())
            .map(|m| m.brightness)
            .unwrap_or(100)
    }

    /// Returns the index of the primary monitor in the monitors list.
    fn primary_monitor_index(&self) -> Option<usize> {
        self.monitors
            .iter()
            .position(|m| m.is_primary && m.is_accessible && m.is_supported)
            .or_else(|| {
                self.monitors
                    .iter()
                    .position(|m| m.is_accessible && m.is_supported)
            })
    }
}

impl cosmic::Application for BrightnessApplet {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "com.system76.CosmicAppletBrightness";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(
        core: cosmic::Core,
        _flags: Self::Flags,
    ) -> (Self, Task<cosmic::Action<Self::Message>>) {
        let app = BrightnessApplet {
            core,
            popup: None,
            monitors: Vec::new(),
        };

        // Trigger initial monitor discovery on startup
        let init_task = cosmic::task::future(async {
            Message::MonitorsDiscovered(discover_monitors().await)
        });

        (app, init_task)
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    /// Applet panel button view.
    /// Intercepts mouse scroll events directly on the panel icon without opening popup.
    fn view(&self) -> Element<'_, Self::Message> {
        let current_brightness = self.primary_brightness();
        let icon_name = brightness_icon_name(current_brightness);

        // Wrap the standard applet icon button in a MouseArea to capture wheel scrolling
        widget::mouse_area(
            self.core
                .applet
                .icon_button(icon_name)
                .on_press(Message::TogglePopup),
        )
        .on_scroll(Message::Scrolled)
        .into()
    }

    /// Popup interface containing independent sliders for each connected monitor.
    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        let mut root_column = widget::Column::new().spacing(12).padding(14);

        // Header with title and refresh button
        let header = widget::row::with_children(vec![
            widget::text::title4("Brilho dos Monitores").into(),
            widget::space::horizontal().into(),
            widget::button::icon(widget::icon::from_name("view-refresh-symbolic"))
                .on_press(Message::RefreshMonitors)
                .into(),
        ])
        .align_y(Alignment::Center);

        root_column = root_column.push(header);
        root_column = root_column.push(widget::divider::horizontal::default());

        if self.monitors.is_empty() {
            let empty_view = widget::column::with_children(vec![
                widget::icon::from_name("display-symbolic").size(32).into(),
                widget::text::body("Nenhum monitor compatível detectado.").into(),
                widget::button::standard("Buscar Monitores")
                    .on_press(Message::RefreshMonitors)
                    .into(),
            ])
            .spacing(10)
            .align_x(Alignment::Center);

            root_column = root_column.push(empty_view);
        } else {
            let mut list = widget::list_column();

            for (index, monitor) in self.monitors.iter().enumerate() {
                let dynamic_icon = brightness_icon_name(monitor.brightness);
                let percent_str = if monitor.is_supported {
                    format!("{}%", monitor.brightness)
                } else {
                    "—".to_string()
                };
                let monitor_type_icon = match &monitor.backend {
                    MonitorBackend::Internal { .. } => "video-display-symbolic",
                    MonitorBackend::DdcCi { .. } => "video-display-symbolic",
                    MonitorBackend::Software { .. } => "tv-symbolic",
                };

                // Title row showing display name and current percentage
                let info_row = widget::row::with_children(vec![
                    widget::icon::from_name(monitor_type_icon).size(16).into(),
                    widget::text::heading(&monitor.name).into(),
                    widget::space::horizontal().into(),
                    widget::text::body(percent_str).into(),
                ])
                .align_y(Alignment::Center)
                .spacing(8);

                let mut card_content = widget::Column::new()
                    .spacing(8)
                    .push(info_row);

                if !monitor.is_accessible {
                    card_content = card_content.push(
                        widget::text::caption(
                            "⚠ Sem permissão de acesso em /dev/i2c-*. Adicione as regras udev e configure o grupo 'i2c'.",
                        ),
                    );
                } else {
                    // Slider row with reactive icon and independent range slider
                    let slider_widget = widget::slider(1..=100, monitor.brightness, move |val| {
                        Message::BrightnessChanged {
                            index,
                            percentage: val,
                        }
                    })
                    .width(Length::Fill);

                    let control_row = widget::row::with_children(vec![
                        widget::icon::from_name(dynamic_icon).size(18).into(),
                        slider_widget.into(),
                    ])
                    .align_y(Alignment::Center)
                    .spacing(12);

                    card_content = card_content.push(control_row);

                    if let MonitorBackend::Software { .. } = &monitor.backend {
                        card_content = card_content.push(
                            widget::text::caption(
                                "Modo Software (xrandr): compatível com TVs e telas sem DDC/CI",
                            ),
                        );
                    }
                }

                list = list.add(card_content);
            }

            root_column = root_column.push(list);
        }

        self.core.applet.popup_container(root_column).into()
    }

    /// Register background subscriptions.
    /// Periodically queries monitor presence every 4 seconds.
    fn subscription(&self) -> Subscription<Self::Message> {
        cosmic::iced::time::every(Duration::from_secs(4)).map(|_| Message::Tick)
    }

    /// Application update reducer.
    fn update(&mut self, message: Self::Message) -> Task<cosmic::Action<Self::Message>> {
        match message {
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let new_id = Id::unique();
                    self.popup.replace(new_id);
                    let mut popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    popup_settings.positioner.size_limits = Limits::NONE
                        .max_width(400.0)
                        .min_width(320.0)
                        .min_height(140.0)
                        .max_height(800.0);

                    // Re-discover monitors whenever the popup opens
                    let refresh_task = cosmic::task::future(async {
                        Message::MonitorsDiscovered(discover_monitors().await)
                    });

                    Task::batch(vec![get_popup(popup_settings), refresh_task])
                };
            }

            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }

            Message::Scrolled(delta) => {
                let scroll_y = match delta {
                    ScrollDelta::Lines { y, .. } => y,
                    ScrollDelta::Pixels { y, .. } => y,
                };

                if scroll_y.abs() > 0.05 {
                    let delta_percent = if scroll_y > 0.0 { 5 } else { -5 };

                    if let Some(pos) = self.primary_monitor_index() {
                        let current = self.monitors[pos].brightness as i32;
                        let new_percentage = (current + delta_percent).clamp(1, 100) as u32;

                        if new_percentage != self.monitors[pos].brightness {
                            self.monitors[pos].brightness = new_percentage;
                            let backend = self.monitors[pos].backend.clone();
                            let id = self.monitors[pos].id.clone();
                            TARGET_BRIGHTNESS
                                .write()
                                .unwrap()
                                .insert(backend.id(), new_percentage);

                            return cosmic::task::future(async move {
                                let result = apply_brightness(backend, new_percentage).await;
                                Message::BrightnessApplied { id, result }
                            });
                        }
                    }
                }
            }

            Message::BrightnessChanged { index, percentage } => {
                if let Some(monitor) = self.monitors.get_mut(index) {
                    if !monitor.is_accessible || !monitor.is_supported {
                        return Task::none();
                    }
                    let new_val = percentage.clamp(1, 100);
                    if monitor.brightness != new_val {
                        monitor.brightness = new_val;
                        let backend = monitor.backend.clone();
                        let id = monitor.id.clone();
                        TARGET_BRIGHTNESS
                            .write()
                            .unwrap()
                            .insert(backend.id(), new_val);

                        return cosmic::task::future(async move {
                            let result = apply_brightness(backend, new_val).await;
                            Message::BrightnessApplied { id, result }
                        });
                    }
                }
            }

            Message::RefreshMonitors | Message::Tick => {
                return cosmic::task::future(async {
                    Message::MonitorsDiscovered(discover_monitors().await)
                });
            }

            Message::MonitorsDiscovered(fresh_monitors) => {
                // Merge discovered monitors with local state preserving in-flight sliders
                if self.monitors.is_empty() {
                    self.monitors = fresh_monitors;
                } else {
                    for fresh in fresh_monitors {
                        if let Some(existing) = self.monitors.iter_mut().find(|m| m.id == fresh.id) {
                            existing.name = fresh.name;
                            existing.backend = fresh.backend;
                            existing.is_accessible = fresh.is_accessible;
                            existing.is_supported = fresh.is_supported;
                            existing.is_primary = fresh.is_primary;
                            // Only update brightness if not actively focused
                            if self.popup.is_none() {
                                existing.brightness = fresh.brightness;
                            }
                        } else {
                            self.monitors.push(fresh);
                        }
                    }
                }
            }

            Message::BrightnessApplied { id: _, result } => {
                if let Err(err) = result {
                    eprintln!("[cosmic-brightness-applet] Erro ao aplicar brilho: {err}");
                }
            }
        }

        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

// ============================================================================
// Main Application Entry Point
// ============================================================================

fn main() -> cosmic::iced::Result {
    // Run the applet event loop
    cosmic::applet::run::<BrightnessApplet>(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_monitor_discovery() {
        let monitors = scan_monitors_sync();
        println!("Monitores detectados: {} monitor(es)", monitors.len());
        for m in &monitors {
            println!(
                "- ID: {}, Nome: {}, Brilho: {}%, Acessível: {}, Suportado: {}, Primário: {}",
                m.id, m.name, m.brightness, m.is_accessible, m.is_supported, m.is_primary
            );
        }
        assert!(!monitors.is_empty(), "Ao menos um monitor deveria ser detectado");
    }

    #[tokio::test]
    async fn test_software_brightness_apply() {
        let backend = MonitorBackend::Software {
            output_name: "HDMI-A-1".to_string(),
        };
        let result = apply_brightness(backend, 100).await;
        assert!(result.is_ok(), "apply_brightness para software falhou: {:?}", result);
    }

    #[test]
    fn test_xrandr_resolution() {
        let display = detect_x11_display();
        let outputs = get_xrandr_outputs(&display, false);
        assert!(!outputs.is_empty(), "Deve detectar saídas no xrandr");
        let target = resolve_xrandr_output_name(&outputs, "HDMI-A-1");
        assert!(target.is_some(), "Deve resolver HDMI-A-1 para uma saída conectada");
    }
}


