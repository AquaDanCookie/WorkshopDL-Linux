use std::fs;
use std::path::{Path, PathBuf};

use crate::util;

pub const APP_ID: &str = "workshopdl-linux";
pub const APP_NAME: &str = "WorkshopDL";
pub const ICON_256: &[u8] = include_bytes!("../assets/workshopdl-linux-256.png");

pub fn icon_data() -> Option<egui::IconData> {
    let image = image::load_from_memory(ICON_256).ok()?;
    let image = image.into_rgba8();
    let (width, height) = image.dimensions();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    })
}

fn data_home() -> PathBuf {
    if let Ok(value) = std::env::var("XDG_DATA_HOME") {
        if !value.trim().is_empty() {
            return PathBuf::from(value);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local").join("share")
}

/// Install a desktop entry + hicolor icon so the app shows the WDL icon in
/// the taskbar/dock, including on Wayland where windows cannot carry icons.
pub fn install_integration(project: &Path) {
    let data = data_home();
    let apps_dir = data.join("applications");
    let icon_dir = data
        .join("icons")
        .join("hicolor")
        .join("256x256")
        .join("apps");
    if fs::create_dir_all(&apps_dir).is_err() || fs::create_dir_all(&icon_dir).is_err() {
        return;
    }

    let icon_path = icon_dir.join(format!("{APP_ID}.png"));
    let _ = fs::write(&icon_path, ICON_256);

    let exe = std::env::current_exe()
        .ok()
        .unwrap_or_else(|| project.join("workshopdl-linux"));
    let exec = quote_exec(&exe);
    let try_exec = quote_exec(&exe);
    let path_line = quote_exec(project);
    let desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Version=1.0\n\
         Name=WorkshopDL\n\
         GenericName=Steam Workshop Downloader\n\
         Comment=Download Steam Workshop items\n\
         Exec={exec}\n\
         TryExec={try_exec}\n\
         Path={path_line}\n\
         Icon={}\n\
         Terminal=false\n\
         Categories=Network;FileTransfer;Utility;\n\
         Keywords=steam;workshop;mod;download;\n\
         StartupNotify=true\n\
         StartupWMClass={APP_ID}\n",
        icon_path.display()
    );
    let desktop_path = apps_dir.join(format!("{APP_ID}.desktop"));
    if fs::read_to_string(&desktop_path).ok().as_deref() != Some(desktop.as_str()) {
        let _ = fs::write(&desktop_path, desktop);
    }

    // Refresh the applications database when the helper is available.
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&apps_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn quote_exec(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "\\\\");
    value = value.replace('"', "\\\"");
    value = value.replace('%', "%%");
    format!("\"{value}\"")
}

pub fn open_workshop() {
    util::open_url("https://steamcommunity.com/workshop/");
}
