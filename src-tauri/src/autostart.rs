/// Auto-start (settings dialog "Auto-start" section): three states off / boot / follow.
///
/// - **boot (auto-start at login)**: launches resident immediately after login, displayed
///   in the same form as when last exited.
/// - **follow (launch with ZCode)**: after login, stays **silently on standby** (tray icon
///   only, no window shown); a background thread checks every 2s whether a ZCode process
///   exists (desktop ZCode.exe / CLI subprocess share the same name, all caught). Once
///   detected, the panel is shown automatically; once shown it is never hidden again, and
///   the detection thread exits. If the panel is manually closed, it does not auto-restore
///   (relaunch via re-login or manual open).
///
/// Implementation depends on no third-party crates: Windows reads/writes the HKCU Run
/// registry value directly (REG_SZ, quoted exe full path + optional args, no admin needed);
/// macOS writes ~/Library/LaunchAgents/com.zcode.speedpanel.autostart.plist (hand-written
/// XML, RunAtLoad=true). **The registry/plist is the single source of truth** —
/// `current_mode()` reads the real state to echo the UI, not storing a duplicate in
/// speed-panel-mode.txt (avoids state drift between the two locations).
///
/// Duplicate-launch interaction: the tauri-plugin-single-instance activation callback
/// directly shows the existing instance window; `--zcode-follow` only takes effect when no
/// instance exists at boot — they do not conflict.

/// Argument appended to the auto-start command line for follow mode (also used to
/// distinguish boot/follow when reading back the registry value)
pub const FOLLOW_ARG: &str = "--zcode-follow";

/// Name of the auto-start entry in the registry/plist (Windows value name / macOS Label)
const AUTOSTART_NAME: &str = "zcode-speed-panel";
#[cfg(target_os = "macos")]
const MAC_PLIST_LABEL: &str = "com.zcode.speedpanel.autostart";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutostartMode {
    Off,
    Boot,
    Follow,
}

impl AutostartMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AutostartMode::Off => "off",
            AutostartMode::Boot => "boot",
            AutostartMode::Follow => "follow",
        }
    }
    /// Any unknown value falls back to Off (manually editing the registry / deleting the
    /// plist naturally reverts to Off)
    pub fn parse(s: &str) -> AutostartMode {
        match s.trim() {
            "boot" => AutostartMode::Boot,
            "follow" => AutostartMode::Follow,
            _ => AutostartMode::Off,
        }
    }
}

/// Whether this launch was passed --zcode-follow (silent-standby flag for follow mode at
/// boot). tauri-plugin-single-instance only lets this process reach setup when no existing
/// instance is present, so this flag never interferes with a manual second launch.
pub fn follow_requested() -> bool {
    std::env::args().any(|a| a == FOLLOW_ARG)
}

/// Whether any ZCode process is running (desktop or CLI, either counts). Process enumeration
/// reuses liveio::platform primitives: Windows matches process name zcode.exe (desktop shell
/// and CLI subprocess share the same name); macOS matches argv[0] ending in /ZCode (desktop)
/// or argument area containing zcode-cli (CLI).
pub fn zcode_running() -> bool {
    crate::liveio::platform::any_zcode_process()
}

/// Read the currently active auto-start mode (registry / LaunchAgent is the source of truth)
pub fn current_mode() -> AutostartMode {
    read_registered().map_or(AutostartMode::Off, |cmd| {
        if cmd.contains(FOLLOW_ARG) {
            AutostartMode::Follow
        } else {
            AutostartMode::Boot
        }
    })
}

/// Set the auto-start mode: Off clears, Boot/Follow writes (Follow appends --zcode-follow).
/// On Err, the frontend surfaces the message as-is (e.g. registry locked by group policy)
pub fn set_mode(mode: AutostartMode) -> Result<(), String> {
    match mode {
        AutostartMode::Off => disable(),
        AutostartMode::Boot | AutostartMode::Follow => {
            let exe = std::env::current_exe()
                .map_err(|e| format!("Cannot locate program path: {e}"))?;
            let exe = exe.to_string_lossy().into_owned();
            enable(&exe, mode == AutostartMode::Follow)
        }
    }
}

// ---- Windows: HKCU\Software\Microsoft\Windows\CurrentVersion\Run ----

#[cfg(windows)]
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

#[cfg(windows)]
fn read_registered() -> Option<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu.open_subkey(RUN_KEY).ok()?;
    key.get_value::<String, _>(AUTOSTART_NAME).ok()
}

#[cfg(windows)]
fn enable(exe: &str, follow: bool) -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    // Run values conventionally wrap the path in quotes: an install path containing spaces
    // (e.g. a portable install moved to Program Files) is not split by the shell on spaces
    let cmd = if follow {
        format!("\"{exe}\" {FOLLOW_ARG}")
    } else {
        format!("\"{exe}\"")
    };
    let (key, _) = hkcu
        .create_subkey(RUN_KEY)
        .map_err(|e| format!("Failed to open registry: {e}"))?;
    key.set_value(AUTOSTART_NAME, &cmd)
        .map_err(|e| format!("Failed to write registry: {e}"))
}

#[cfg(windows)]
fn disable() -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey_with_flags(RUN_KEY, winreg::enums::KEY_WRITE)
        .map_err(|e| format!("Failed to open registry: {e}"))?;
    match key.delete_value(AUTOSTART_NAME) {
        Ok(()) => Ok(()),
        // Value already absent = already Off, not a failure (idempotent)
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Failed to delete registry value: {e}")),
    }
}

// ---- macOS: ~/Library/LaunchAgents/<label>.plist (hand-written XML, no plist dependency) ----

#[cfg(target_os = "macos")]
fn plist_path() -> Option<std::path::PathBuf> {
    crate::metrics::home_dir().map(|h| {
        h.join("Library")
            .join("LaunchAgents")
            .join(format!("{MAC_PLIST_LABEL}.plist"))
    })
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn read_registered() -> Option<String> {
    let path = plist_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    // Only needs to read back the full command line form (whether it contains --zcode-follow),
    // not a full plist parse
    let mut reassembled = String::new();
    for seg in content.split('<').skip(1) {
        if let Some(v) = seg.strip_prefix("string>") {
            reassembled.push_str(v.split('<').next().unwrap_or(""));
            reassembled.push(' ');
        }
    }
    Some(reassembled.trim().to_string())
}

#[cfg(target_os = "macos")]
fn enable(exe: &str, follow: bool) -> Result<(), String> {
    let Some(path) = plist_path() else {
        return Err("Cannot locate user directory".into());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Failed to create LaunchAgents: {e}"))?;
    }
    let arg_line = if follow {
        format!("<string>{}</string>", xml_escape(FOLLOW_ARG))
    } else {
        String::new()
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict>\
<key>Label</key><string>{MAC_PLIST_LABEL}</string>\
<key>ProgramArguments</key><array><string>{}</string>{arg_line}</array>\
<key>RunAtLoad</key><true/>\
</dict></plist>\n",
        xml_escape(exe)
    );
    std::fs::write(&path, xml).map_err(|e| format!("Failed to write LaunchAgent: {e}"))
}

#[cfg(target_os = "macos")]
fn disable() -> Result<(), String> {
    let Some(path) = plist_path() else {
        return Err("Cannot locate user directory".into());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Failed to delete LaunchAgent: {e}")),
    }
}

// ---- Other platforms (this project ships Windows/macOS only; report unsupported here) ----

#[cfg(not(any(windows, target_os = "macos")))]
fn read_registered() -> Option<String> {
    None
}

#[cfg(not(any(windows, target_os = "macos")))]
fn enable(_exe: &str, _follow: bool) -> Result<(), String> {
    Err("Current platform does not support auto-start".into())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn disable() -> Result<(), String> {
    Err("Current platform does not support auto-start".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip() {
        for m in [AutostartMode::Off, AutostartMode::Boot, AutostartMode::Follow] {
            assert_eq!(AutostartMode::parse(m.as_str()), m);
        }
        assert_eq!(AutostartMode::parse("garbage"), AutostartMode::Off);
    }

    /// End-to-end registry read/write (Windows only; runs on dev/CI). Value name is fixed;
    /// test restores the user's original value on completion (or leaves Off if none),
    /// leaving no persistent side effects on the test machine
    #[cfg(windows)]
    #[test]
    fn registry_enable_disable_cycle() {
        let saved = read_registered();
        let _ = disable();
        assert_eq!(current_mode(), AutostartMode::Off);

        set_mode(AutostartMode::Boot).unwrap();
        assert_eq!(current_mode(), AutostartMode::Boot);
        let cmd = read_registered().unwrap();
        assert!(
            cmd.starts_with('"') && cmd.contains(".exe\""),
            "boot mode command line should be a quoted exe path: {cmd}"
        );
        assert!(!cmd.contains(FOLLOW_ARG));

        set_mode(AutostartMode::Follow).unwrap();
        assert_eq!(current_mode(), AutostartMode::Follow);
        assert!(read_registered().unwrap().contains(FOLLOW_ARG));

        set_mode(AutostartMode::Off).unwrap();
        assert_eq!(current_mode(), AutostartMode::Off);

        // Restore user's original value
        if let Some(prev) = saved {
            use winreg::enums::HKEY_CURRENT_USER;
            use winreg::RegKey;
            let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
                .create_subkey(RUN_KEY)
                .unwrap();
            key.set_value(AUTOSTART_NAME, &prev).unwrap();
        }
    }
}
