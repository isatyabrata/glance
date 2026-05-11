//! Glance menu-bar GUI entry point.
//!
//! Behaviour:
//! - Tray icon visible on launch (ActivationPolicy::Accessory → no Dock icon).
//! - Main window starts HIDDEN (configured in tauri.conf.json `visible: false`).
//! - Click tray → toggle window.
//! - Close button (cmd+W / red dot) → hide instead of exit (`prevent_close`).
//! - Tray menu has Show/Hide + Quit.

mod commands;
mod tray;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .setup(|app| {
            // Menu-bar app: no Dock icon at launch.
            #[cfg(target_os = "macos")]
            {
                let _ = app
                    .handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            if let Err(e) = tray::init(app.handle()) {
                eprintln!("[glance-app] tray init failed: {:?}", e);
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // Close button → hide; keep app alive in the menu bar.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                #[cfg(target_os = "macos")]
                {
                    let app = window.app_handle();
                    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
                }
                api.prevent_close();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::set_config,
            commands::get_config_path,
            commands::list_tool_toggles,
            commands::list_tool_clients,
            commands::set_tool_clients,
            commands::chrome_status,
            commands::chrome_install,
            commands::chrome_uninstall,
            commands::chrome_open_extensions_page,
            commands::chrome_open_extension_dir,
            commands::chrome_adapter_list,
            commands::chrome_adapter_get,
            commands::chrome_adapter_save,
            commands::chrome_adapter_delete,
            commands::chrome_adapter_open_dir,
            commands::chrome_get_last_evaluate,
            commands::chrome_save_last_evaluate_as_adapter,
            commands::tail_events,
            commands::clear_events,
            commands::today_stats,
            commands::test_backend,
            commands::list_models,
            commands::ping_model,
            commands::test_github_token,
            commands::pick_folder,
            commands::show_main_window_cmd,
            commands::open_url,
            commands::list_upstream_mcps,
            commands::add_upstream_mcp,
            commands::remove_upstream_mcp,
            commands::set_upstream_mcp_enabled,
            commands::test_upstream_mcp,
            commands::list_upstream_templates,
            commands::reload_upstream_mcps,
            commands::rtk_status,
            commands::rtk_gain,
            commands::rtk_history,
            commands::rtk_init,
            commands::rtk_uninstall,
            commands::rtk_check_update,
            commands::rtk_update,
            commands::ccusage_status,
            commands::ccusage_daily,
            commands::ccusage_sessions,
            commands::ccusage_codex_daily,
            commands::ccusage_codex_sessions,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
