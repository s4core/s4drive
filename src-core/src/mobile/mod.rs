//! Mobile v1: platform-specific sync policy, battery-aware scheduling,
//! Wi-Fi-only transfers, background task constraints.
//!
//! Mobile OSes (Android/iOS) have strict background execution limits.
//! This module provides helpers for a "cloud-first, battery-aware" approach.
//!
//! Key policies:
//! - Cloud-first: files don't download until opened
//! - Offline: only explicitly-pinned files
//! - Background sync: 15-30 min intervals, Wi-Fi + charging preferred
//! - No full sync on mobile — only user-initiated or push-triggered

use std::time::Duration;

/// Default background sync interval (seconds).
pub const MOBILE_SYNC_INTERVAL_SEC: u64 = 900; // 15 min

/// Minimum interval for periodic mobile background sync.
pub const MOBILE_BACKGROUND_SYNC_MIN_SEC: u64 = MOBILE_SYNC_INTERVAL_SEC;

/// Maximum interval for periodic mobile background sync.
pub const MOBILE_BACKGROUND_SYNC_MAX_SEC: u64 = 30 * 60; // 30 min

/// Minimum interval for battery-aware sync.
pub const MOBILE_MIN_SYNC_INTERVAL_SEC: u64 = 300; // 5 min

/// Maximum interval when constrained.
pub const MOBILE_MAX_SYNC_INTERVAL_SEC: u64 = 3600; // 1 hour

/// File size threshold for "large file" warnings on mobile (bytes).
pub const MOBILE_LARGE_FILE_THRESHOLD: u64 = 50 * 1024 * 1024; // 50 MB

/// Maximum file size for cellular downloads (bytes) — user-configurable.
pub const MOBILE_CELLULAR_MAX_BYTES: u64 = 100 * 1024 * 1024; // 100 MB

/// Preferred chunk size for mobile uploads (smaller = more resumable).
pub const MOBILE_UPLOAD_CHUNK_SIZE: u64 = 512 * 1024; // 512 KB

/// Constraints for background sync on mobile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MobileSyncConstraints {
    /// Sync only on Wi-Fi (default: true).
    pub wifi_only: bool,
    /// Sync only while charging (default: false).
    pub charging_only: bool,
    /// Minimum battery percentage for sync (0-100, default: 20).
    pub min_battery_pct: u8,
    /// Upload only on Wi-Fi (default: true).
    pub upload_wifi_only: bool,
    /// Large transfers should wait for unmetered Wi-Fi/Ethernet (default: true).
    pub large_files_wifi_only: bool,
    /// Allow metered networks (cellular or metered Wi-Fi) for sync (default: false).
    pub allow_metered_networks: bool,
}

impl Default for MobileSyncConstraints {
    fn default() -> Self {
        Self {
            wifi_only: true,
            charging_only: false,
            min_battery_pct: 20,
            upload_wifi_only: true,
            large_files_wifi_only: true,
            allow_metered_networks: false,
        }
    }
}

/// Current network state (abstracted from platform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkState {
    Unknown,
    Wifi,
    MeteredWifi,
    Cellular,
    Ethernet,
    None,
}

impl NetworkState {
    /// Whether the device has a usable network connection.
    pub fn has_connectivity(self) -> bool {
        !matches!(self, Self::None | Self::Unknown)
    }

    /// Whether the network is Wi-Fi-like for mobile policy purposes.
    pub fn is_wifi_like(self) -> bool {
        matches!(self, Self::Wifi | Self::MeteredWifi)
    }

    /// Whether the network is unmetered enough for background or large transfers.
    pub fn is_unmetered(self) -> bool {
        matches!(self, Self::Wifi | Self::Ethernet)
    }

    /// Whether the network may incur metered-data cost.
    pub fn is_metered(self) -> bool {
        matches!(self, Self::MeteredWifi | Self::Cellular)
    }
}

/// Current battery state (abstracted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryState {
    Unknown,
    Charging(u8),    // charging, battery percentage
    Discharging(u8), // discharging, battery percentage
    Full,
    Low,
}

impl BatteryState {
    /// Battery percentage clamped to the valid 0-100 range when known.
    pub fn percentage(self) -> Option<u8> {
        match self {
            Self::Charging(pct) | Self::Discharging(pct) => Some(pct.min(100)),
            Self::Full => Some(100),
            Self::Low => Some(0),
            Self::Unknown => None,
        }
    }

    /// Whether the device is plugged in or already full.
    pub fn is_charging_or_full(self) -> bool {
        matches!(self, Self::Charging(_) | Self::Full)
    }

    /// Whether the state should block background sync for a configured threshold.
    pub fn is_below(self, min_battery_pct: u8) -> bool {
        match self {
            Self::Discharging(pct) => pct.min(100) < min_battery_pct.min(100),
            Self::Low => true,
            Self::Unknown | Self::Charging(_) | Self::Full => false,
        }
    }
}

/// Mobile transfer class used to apply stricter upload/download policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileTransferKind {
    /// Lightweight metadata poll/apply.
    Metadata,
    /// User or background upload.
    Upload,
    /// User or background download.
    Download,
}

/// Cloud-first availability state for a file on a mobile device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MobileFileAvailability {
    /// File has no local bytes and should download only when opened.
    OnlineOnly,
    /// File has local cached bytes, but is not explicitly pinned.
    Cached,
    /// File or containing folder was explicitly selected for offline use.
    PinnedOffline,
}

/// Result of a mobile sync eligibility check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEligibility {
    /// Sync can proceed.
    Allowed,
    /// Sync is blocked for a specific reason.
    Blocked(&'static str),
}

/// Check if a mobile sync cycle should run given constraints and device state.
pub fn check_sync_eligibility(
    constraints: &MobileSyncConstraints,
    network: NetworkState,
    battery: BatteryState,
) -> SyncEligibility {
    // No network at all
    if !network.has_connectivity() {
        return SyncEligibility::Blocked("no network connection");
    }

    if !constraints.allow_metered_networks && network.is_metered() {
        return SyncEligibility::Blocked("metered network is disabled");
    }

    // Wi-Fi only check
    if constraints.wifi_only && !network.is_wifi_like() && network != NetworkState::Ethernet {
        return SyncEligibility::Blocked("not on Wi-Fi (Wi-Fi only mode)");
    }

    // Battery checks
    if battery.is_below(constraints.min_battery_pct) {
        return SyncEligibility::Blocked("battery too low");
    }

    // Charging only check
    if constraints.charging_only && !battery.is_charging_or_full() {
        return SyncEligibility::Blocked("not charging (charging only mode)");
    }

    SyncEligibility::Allowed
}

/// Determine the background sync interval based on constraints and state.
pub fn compute_sync_interval(
    base_interval_sec: u64,
    network: NetworkState,
    battery: BatteryState,
) -> Duration {
    if !network.has_connectivity() {
        return Duration::from_secs(MOBILE_MAX_SYNC_INTERVAL_SEC);
    }

    let mut interval = base_interval_sec.max(MOBILE_MIN_SYNC_INTERVAL_SEC);

    // Increase interval on metered networks (save data).
    if network.is_metered() {
        interval = interval.saturating_mul(2);
    }

    // Increase interval on battery (save power)
    if matches!(battery.percentage(), Some(p) if p < 50) && !battery.is_charging_or_full() {
        interval = interval.saturating_mul(2);
    }

    // Cap at maximum
    Duration::from_secs(interval.min(MOBILE_MAX_SYNC_INTERVAL_SEC))
}

/// Determine the periodic background sync interval required by mobile v1.
///
/// WorkManager and iOS BackgroundTasks are best-effort. The core policy keeps
/// periodic work inside the planned 15-30 minute window and lets platform code
/// add OS-specific constraints.
pub fn compute_background_sync_interval(network: NetworkState, battery: BatteryState) -> Duration {
    if !network.has_connectivity() {
        return Duration::from_secs(MOBILE_BACKGROUND_SYNC_MAX_SEC);
    }

    let mut interval = MOBILE_BACKGROUND_SYNC_MIN_SEC;

    if network.is_metered() || !battery.is_charging_or_full() {
        interval = interval.saturating_mul(2);
    }

    Duration::from_secs(interval.min(MOBILE_BACKGROUND_SYNC_MAX_SEC))
}

/// Resolve the cloud-first mobile availability state for a file.
pub fn mobile_file_availability(is_pinned: bool, is_cached: bool) -> MobileFileAvailability {
    if is_pinned {
        MobileFileAvailability::PinnedOffline
    } else if is_cached {
        MobileFileAvailability::Cached
    } else {
        MobileFileAvailability::OnlineOnly
    }
}

/// Whether a file should show as "online-only" (not downloaded) on mobile.
pub fn is_online_only(is_pinned: bool, is_recently_opened: bool) -> bool {
    mobile_file_availability(is_pinned, is_recently_opened) == MobileFileAvailability::OnlineOnly
}

/// Whether cellular download is allowed for a file of given size.
pub fn allow_cellular_download(file_size: u64, max_cellular_bytes: u64) -> bool {
    file_size <= max_cellular_bytes
}

/// Check whether opening an online-only file may trigger a download now.
pub fn check_open_download_eligibility(
    is_pinned: bool,
    is_cached: bool,
    network: NetworkState,
    file_size: u64,
    max_cellular_bytes: u64,
) -> SyncEligibility {
    if is_cached {
        return SyncEligibility::Allowed;
    }

    if !network.has_connectivity() {
        return if is_pinned {
            SyncEligibility::Blocked("pinned file is not available offline yet")
        } else {
            SyncEligibility::Blocked("online-only file requires network")
        };
    }

    if network == NetworkState::Cellular && !allow_cellular_download(file_size, max_cellular_bytes)
    {
        return SyncEligibility::Blocked("file is too large for cellular download");
    }

    SyncEligibility::Allowed
}

/// Check whether a mobile transfer may run under current constraints.
pub fn check_transfer_eligibility(
    constraints: &MobileSyncConstraints,
    network: NetworkState,
    battery: BatteryState,
    kind: MobileTransferKind,
    file_size: u64,
) -> SyncEligibility {
    if let SyncEligibility::Blocked(reason) = check_sync_eligibility(constraints, network, battery)
    {
        return SyncEligibility::Blocked(reason);
    }

    if kind == MobileTransferKind::Metadata {
        return SyncEligibility::Allowed;
    }

    if constraints.large_files_wifi_only
        && file_size > MOBILE_LARGE_FILE_THRESHOLD
        && !network.is_unmetered()
    {
        return SyncEligibility::Blocked("large files require an unmetered network");
    }

    if kind == MobileTransferKind::Upload && constraints.upload_wifi_only && !network.is_unmetered()
    {
        return SyncEligibility::Blocked("uploads require an unmetered network");
    }

    if kind == MobileTransferKind::Download
        && network == NetworkState::Cellular
        && !allow_cellular_download(file_size, MOBILE_CELLULAR_MAX_BYTES)
    {
        return SyncEligibility::Blocked("download exceeds cellular size limit");
    }

    SyncEligibility::Allowed
}

/// Number of resumable chunks needed for a mobile upload plan.
pub fn mobile_upload_chunk_count(file_size: u64) -> u64 {
    if file_size == 0 {
        0
    } else {
        ((file_size - 1) / MOBILE_UPLOAD_CHUNK_SIZE) + 1
    }
}

/// Mobile-optimized chunked upload: split file into small chunks for
/// better resumability on unreliable mobile connections.
pub fn mobile_upload_chunks(file_size: u64) -> Vec<(u64, u64)> {
    let mut chunks = Vec::new();
    let mut offset = 0u64;
    while offset < file_size {
        let end = offset
            .saturating_add(MOBILE_UPLOAD_CHUNK_SIZE)
            .min(file_size);
        chunks.push((offset, end - offset));
        offset = end;
    }
    chunks
}

/// Human-readable explanation of a network state.
pub fn network_label(state: NetworkState) -> &'static str {
    match state {
        NetworkState::Wifi => "Wi-Fi",
        NetworkState::MeteredWifi => "Metered Wi-Fi",
        NetworkState::Cellular => "Cellular",
        NetworkState::Ethernet => "Ethernet",
        NetworkState::None => "Offline",
        NetworkState::Unknown => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_eligibility_allowed() {
        let c = MobileSyncConstraints::default();
        assert_eq!(
            check_sync_eligibility(&c, NetworkState::Wifi, BatteryState::Charging(80)),
            SyncEligibility::Allowed
        );
    }

    #[test]
    fn test_sync_eligibility_no_network() {
        let c = MobileSyncConstraints::default();
        let result = check_sync_eligibility(&c, NetworkState::None, BatteryState::Full);
        assert!(matches!(result, SyncEligibility::Blocked(_)));
    }

    #[test]
    fn test_sync_eligibility_cellular_blocked() {
        let c = MobileSyncConstraints {
            wifi_only: true,
            ..Default::default()
        };
        let result = check_sync_eligibility(&c, NetworkState::Cellular, BatteryState::Full);
        assert!(matches!(result, SyncEligibility::Blocked(_)));
    }

    #[test]
    fn test_sync_eligibility_metered_wifi_blocked_by_default() {
        let c = MobileSyncConstraints::default();
        let result = check_sync_eligibility(&c, NetworkState::MeteredWifi, BatteryState::Full);
        assert_eq!(
            result,
            SyncEligibility::Blocked("metered network is disabled")
        );
    }

    #[test]
    fn test_sync_eligibility_metered_wifi_can_be_enabled() {
        let c = MobileSyncConstraints {
            allow_metered_networks: true,
            ..Default::default()
        };
        assert_eq!(
            check_sync_eligibility(&c, NetworkState::MeteredWifi, BatteryState::Full),
            SyncEligibility::Allowed
        );
    }

    #[test]
    fn test_sync_eligibility_low_battery() {
        let c = MobileSyncConstraints {
            min_battery_pct: 30,
            ..Default::default()
        };
        let result = check_sync_eligibility(&c, NetworkState::Wifi, BatteryState::Discharging(15));
        assert!(matches!(result, SyncEligibility::Blocked(_)));
    }

    #[test]
    fn test_sync_eligibility_charging_only_not_charging() {
        let c = MobileSyncConstraints {
            charging_only: true,
            ..Default::default()
        };
        let result = check_sync_eligibility(&c, NetworkState::Wifi, BatteryState::Discharging(80));
        assert!(matches!(result, SyncEligibility::Blocked(_)));
    }

    #[test]
    fn test_sync_interval_cellular() {
        let interval =
            compute_sync_interval(900, NetworkState::Cellular, BatteryState::Charging(100));
        assert!(interval.as_secs() >= 900);
    }

    #[test]
    fn test_sync_interval_low_battery() {
        let interval =
            compute_sync_interval(900, NetworkState::Wifi, BatteryState::Discharging(30));
        assert!(interval.as_secs() >= 900);
    }

    #[test]
    fn test_background_sync_interval_stays_in_mobile_window() {
        assert_eq!(
            compute_background_sync_interval(NetworkState::Wifi, BatteryState::Charging(80))
                .as_secs(),
            MOBILE_BACKGROUND_SYNC_MIN_SEC
        );
        assert_eq!(
            compute_background_sync_interval(NetworkState::Cellular, BatteryState::Discharging(30))
                .as_secs(),
            MOBILE_BACKGROUND_SYNC_MAX_SEC
        );
    }

    #[test]
    fn test_battery_percentage_is_clamped() {
        assert_eq!(BatteryState::Charging(150).percentage(), Some(100));
        assert_eq!(BatteryState::Unknown.percentage(), None);
    }

    #[test]
    fn test_mobile_file_availability() {
        assert_eq!(
            mobile_file_availability(false, false),
            MobileFileAvailability::OnlineOnly
        );
        assert_eq!(
            mobile_file_availability(false, true),
            MobileFileAvailability::Cached
        );
        assert_eq!(
            mobile_file_availability(true, false),
            MobileFileAvailability::PinnedOffline
        );
    }

    #[test]
    fn test_online_only() {
        assert!(is_online_only(false, false));
        assert!(!is_online_only(true, false));
        assert!(!is_online_only(false, true));
    }

    #[test]
    fn test_cellular_download_allowed() {
        assert!(allow_cellular_download(1024, MOBILE_CELLULAR_MAX_BYTES));
        assert!(!allow_cellular_download(
            MOBILE_CELLULAR_MAX_BYTES + 1,
            MOBILE_CELLULAR_MAX_BYTES,
        ));
    }

    #[test]
    fn test_open_download_uses_cache_offline() {
        assert_eq!(
            check_open_download_eligibility(
                false,
                true,
                NetworkState::None,
                MOBILE_CELLULAR_MAX_BYTES + 1,
                MOBILE_CELLULAR_MAX_BYTES,
            ),
            SyncEligibility::Allowed
        );
    }

    #[test]
    fn test_open_download_blocks_online_only_when_offline() {
        assert_eq!(
            check_open_download_eligibility(false, false, NetworkState::None, 1024, 1024),
            SyncEligibility::Blocked("online-only file requires network")
        );
    }

    #[test]
    fn test_open_download_blocks_large_cellular_file() {
        assert_eq!(
            check_open_download_eligibility(
                false,
                false,
                NetworkState::Cellular,
                MOBILE_CELLULAR_MAX_BYTES + 1,
                MOBILE_CELLULAR_MAX_BYTES,
            ),
            SyncEligibility::Blocked("file is too large for cellular download")
        );
    }

    #[test]
    fn test_transfer_upload_wifi_only_blocks_cellular() {
        let c = MobileSyncConstraints {
            wifi_only: false,
            allow_metered_networks: true,
            large_files_wifi_only: false,
            ..Default::default()
        };
        assert_eq!(
            check_transfer_eligibility(
                &c,
                NetworkState::Cellular,
                BatteryState::Full,
                MobileTransferKind::Upload,
                1024,
            ),
            SyncEligibility::Blocked("uploads require an unmetered network")
        );
    }

    #[test]
    fn test_transfer_large_download_requires_unmetered_network() {
        let c = MobileSyncConstraints {
            wifi_only: false,
            allow_metered_networks: true,
            ..Default::default()
        };
        assert_eq!(
            check_transfer_eligibility(
                &c,
                NetworkState::Cellular,
                BatteryState::Full,
                MobileTransferKind::Download,
                MOBILE_LARGE_FILE_THRESHOLD + 1,
            ),
            SyncEligibility::Blocked("large files require an unmetered network")
        );
    }

    #[test]
    fn test_transfer_small_cellular_download_can_be_enabled() {
        let c = MobileSyncConstraints {
            wifi_only: false,
            allow_metered_networks: true,
            large_files_wifi_only: false,
            ..Default::default()
        };
        assert_eq!(
            check_transfer_eligibility(
                &c,
                NetworkState::Cellular,
                BatteryState::Full,
                MobileTransferKind::Download,
                1024,
            ),
            SyncEligibility::Allowed
        );
    }

    #[test]
    fn test_mobile_upload_chunks() {
        let chunks = mobile_upload_chunks(1024 * 1024); // 1 MB
        assert_eq!(chunks.len(), 2); // 2 chunks of 512 KB
        assert_eq!(chunks[0], (0, 512 * 1024));
        assert_eq!(chunks[1], (512 * 1024, 512 * 1024));
    }

    #[test]
    fn test_mobile_upload_chunk_count() {
        assert_eq!(mobile_upload_chunk_count(0), 0);
        assert_eq!(mobile_upload_chunk_count(1), 1);
        assert_eq!(mobile_upload_chunk_count(MOBILE_UPLOAD_CHUNK_SIZE + 1), 2);
    }

    #[test]
    fn test_mobile_upload_chunks_exact() {
        let chunks = mobile_upload_chunks(MOBILE_UPLOAD_CHUNK_SIZE);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], (0, MOBILE_UPLOAD_CHUNK_SIZE));
    }

    #[test]
    fn test_network_label() {
        assert_eq!(network_label(NetworkState::Wifi), "Wi-Fi");
        assert_eq!(network_label(NetworkState::None), "Offline");
        assert_eq!(network_label(NetworkState::Cellular), "Cellular");
        assert_eq!(network_label(NetworkState::MeteredWifi), "Metered Wi-Fi");
    }
}
