use smappservice_rs::{AppService, ServiceStatus, ServiceType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchAtLoginStatus {
    Disabled,
    Enabled,
    RequiresApproval,
}

#[must_use]
pub fn launch_at_login_status() -> LaunchAtLoginStatus {
    map_service_status(AppService::new(ServiceType::MainApp).status())
}

fn map_service_status(status: ServiceStatus) -> LaunchAtLoginStatus {
    match status {
        ServiceStatus::Enabled => LaunchAtLoginStatus::Enabled,
        ServiceStatus::RequiresApproval => LaunchAtLoginStatus::RequiresApproval,
        ServiceStatus::NotRegistered | ServiceStatus::NotFound => LaunchAtLoginStatus::Disabled,
    }
}

/// Registers or unregisters the main app as a macOS login item.
///
/// # Errors
///
/// Returns a localized `ServiceManagement` error when macOS rejects the change.
pub fn set_launch_at_login(enabled: bool) -> anyhow::Result<LaunchAtLoginStatus> {
    let service = AppService::new(ServiceType::MainApp);
    let result = if enabled {
        service.register()
    } else {
        service.unregister()
    };
    result.map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(launch_at_login_status())
}

#[cfg(test)]
mod tests {
    use smappservice_rs::ServiceStatus;

    use super::{LaunchAtLoginStatus, map_service_status};

    #[test]
    fn missing_main_app_remains_available_for_registration() {
        assert_eq!(
            map_service_status(ServiceStatus::NotFound),
            LaunchAtLoginStatus::Disabled
        );
        assert_eq!(
            map_service_status(ServiceStatus::NotRegistered),
            LaunchAtLoginStatus::Disabled
        );
    }

    #[test]
    fn registered_statuses_preserve_system_state() {
        assert_eq!(
            map_service_status(ServiceStatus::Enabled),
            LaunchAtLoginStatus::Enabled
        );
        assert_eq!(
            map_service_status(ServiceStatus::RequiresApproval),
            LaunchAtLoginStatus::RequiresApproval
        );
    }
}
