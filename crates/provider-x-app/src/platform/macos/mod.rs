mod activation;
mod startup;
mod startup_failure;
pub mod tray;

pub use activation::{
    ActivationPolicyError, is_accessory_activation_policy, set_accessory_activation_policy,
};
pub use startup::{LaunchAtLoginStatus, launch_at_login_status, set_launch_at_login};
pub(crate) use startup_failure::{show_startup_failure, show_upgrade_notice};
