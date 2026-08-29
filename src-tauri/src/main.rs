mod commands;
mod filesystem;
mod iso;
mod split;
mod ulcfg;

use commands::AppState;
use std::sync::Mutex;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .manage(AppState {
            settings: Mutex::new(commands::AppSettings::default()),
        })
        .invoke_handler(tauri::generate_handler![
            commands::detect_device,
            commands::list_devices,
            commands::validate_iso,
            commands::process_iso,
            commands::generate_ulcfg,
            commands::verify_games,
            commands::list_device_games,
            commands::delete_game,
            commands::rename_game,
            commands::repair_split_files,
            commands::get_settings,
            commands::save_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
