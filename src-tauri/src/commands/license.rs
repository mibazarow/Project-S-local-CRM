use crate::license;
use crate::error::Result;
use super::app_config_dir;
use tauri::AppHandle;

#[tauri::command]
pub(crate) fn license_status(app: AppHandle) -> Result<license::LicenseStatus> {
    Ok(license::status(&app_config_dir(&app)?))
}

#[tauri::command]
pub(crate) fn license_accept(app: AppHandle) -> Result<license::LicenseStatus> {
    license::accept(&app_config_dir(&app)?)
}

#[tauri::command]
pub(crate) fn license_decline(app: AppHandle) -> Result<()> {
    license::decline(|| app.exit(0));
    Ok(())
}
