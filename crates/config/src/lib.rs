//! Codape configuration system.

#![warn(missing_docs)]

pub mod dirs;
pub mod experiments;
pub mod flags;
pub mod managed;
pub mod permission_rules;
pub mod secret_store;
pub mod settings;
pub mod workspace_trust;

#[cfg(feature = "enterprise")]
pub mod org_policy;
#[cfg(feature = "enterprise")]
pub mod remote_settings;
#[cfg(feature = "enterprise")]
pub mod trusted_device;

pub use flags::{CapabilitySet, FeatureFlags, FlagRegistry, percentage_rollout};
pub use managed::{
    DEFAULT_MACOS_MANAGED_PLIST_DOMAIN, WINDOWS_MANAGED_REGISTRY_KEY, default_managed_plist_path,
};
#[cfg(feature = "enterprise")]
pub use org_policy::{OrgPolicy, OrgPolicyError, OrgPolicyStore};
#[cfg(feature = "enterprise")]
pub use remote_settings::{
    FetchState, LoadingBarrier, RemoteManagedSettings, RemoteManagedSettingsClient,
    RemoteSettingsError, RemoteSettingsManager, RemoteSettingsResponse,
};
pub use settings::{
    CURRENT_SCHEMA_VERSION, ConfigDiff, ConfigDiffEntry, ConfigError, ConfigService, HookLayers,
    MarketplaceSetting, PermissionsSettings, PluginHostSettings, ProviderSettings,
    RuntimeOverrides, SessionOverrides, Settings, config_home, migrate_settings, resolve_api_key,
};
#[cfg(feature = "enterprise")]
pub use trusted_device::{
    DeviceRecord, TrustStatus, TrustedDeviceError, TrustedDeviceRegistry,
    default_device_store_path, machine_fingerprint,
};
pub use workspace_trust::{
    TRUST_ENV, TRUST_RECORD_FILE, TrustRecord, TrustRequest, TrustedCapability, forget,
    pre_trust_from_env, trust_record_path, workspace_is_trusted,
};
