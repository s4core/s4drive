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
}

impl Default for MobileSyncConstraints {
    fn default() -> Self {
        Self {
            wifi_only: true,
            charging_only: false,
            min_battery_pct: 20,
            upload_wifi_only: true,
        }
    }
}

/// Current network state (abstracted from platform).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkState {
    Unknown,
    Wifi,
    Cellular,
    Ethernet,
    None,
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
    if network == NetworkState::None || network == NetworkState::Unknown {
        return SyncEligibility::Blocked("no network connection");
    }

    // Wi-Fi only check
    if constraints.wifi_only && network != NetworkState::Wifi && network != NetworkState::Ethernet {
        return SyncEligibility::Blocked("not on Wi-Fi (Wi-Fi only mode)");
    }

    // Battery checks
    match battery {
        BatteryState::Discharging(pct) if pct < constraints.min_battery_pct => {
            return SyncEligibility::Blocked("battery too low");
        }
        BatteryState::Low => {
            return SyncEligibility::Blocked("battery is low");
        }
        BatteryState::Charging(_) | BatteryState::Full => {
            // Charging or full — always allow
        }
        BatteryState::Unknown => {
            // Unknown battery state — allow but warn
        }
        _ => {}
    }

    // Charging only check
    if constraints.charging_only {
        match battery {
            BatteryState::Charging(_) | BatteryState::Full => {
                // OK
            }
            _ => {
                return SyncEligibility::Blocked("not charging (charging only mode)");
            }
        }
    }

    SyncEligibility::Allowed
}

/// Determine the background sync interval based on constraints and state.
pub fn compute_sync_interval(
    base_interval_sec: u64,
    network: NetworkState,
    battery: BatteryState,
) -> Duration {
    let mut interval = base_interval_sec.max(MOBILE_MIN_SYNC_INTERVAL_SEC);

    // Increase interval on cellular (save data)
    if network == NetworkState::Cellular {
        interval = interval.saturating_mul(2);
    }

    // Increase interval on battery (save power)
    if matches!(battery, BatteryState::Discharging(p) if p < 50) {
        interval = interval.saturating_mul(2);
    }

    // Cap at maximum
    Duration::from_secs(interval.min(MOBILE_MAX_SYNC_INTERVAL_SEC))
}

/// Whether a file should show as "online-only" (not downloaded) on mobile.
pub fn is_online_only(is_pinned: bool, is_recently_opened: bool) -> bool {
    !is_pinned && !is_recently_opened
}

/// Whether cellular download is allowed for a file of given size.
pub fn allow_cellular_download(file_size: u64, max_cellular_bytes: u64) -> bool {
    file_size <= max_cellular_bytes
}

/// Mobile-optimized chunked upload: split file into small chunks for
/// better resumability on unreliable mobile connections.
pub fn mobile_upload_chunks(file_size: u64) -> Vec<(u64, u64)> {
    let mut chunks = Vec::new();
    let mut offset = 0u64;
    while offset < file_size {
        let end = (offset + MOBILE_UPLOAD_CHUNK_SIZE).min(file_size);
        chunks.push((offset, end - offset));
        offset = end;
    }
    chunks
}

/// Human-readable explanation of a network state.
pub fn network_label(state: NetworkState) -> &'static str {
    match state {
        NetworkState::Wifi => "Wi-Fi",
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
    fn test_mobile_upload_chunks() {
        let chunks = mobile_upload_chunks(1024 * 1024); // 1 MB
        assert!(chunks.len() == 2); // 2 chunks of 512 KB
        assert_eq!(chunks[0], (0, 512 * 1024));
        assert_eq!(chunks[1], (512 * 1024, 512 * 1024));
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
    }
}
