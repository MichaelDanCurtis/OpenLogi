//! Wire DTOs for Easy-Switch host slots and software-initiated host switches.
//! These cross the agent↔client IPC boundary: append-only, never reorder.

use serde::{Deserialize, Serialize};

/// One Easy-Switch slot as the keyboard reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSlotInfo {
    /// 1-based slot number, matching the key cap.
    pub slot: u8,
    /// Whether the slot has a pairing; `None` when the device did not say.
    pub paired: Option<bool>,
    /// The host's friendly name as stored on the device, when readable.
    pub name: Option<String>,
}

/// The keyboard's host slots and which one it is talking to now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSlots {
    /// 1-based slot number the device currently uses, or `None` when the
    /// device did not report one.
    pub current: Option<u8>,
    /// Every slot the device has, in slot order.
    pub slots: Vec<HostSlotInfo>,
}

/// What a software-initiated host switch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostSwitchOutcome {
    /// The keyboard (and its linked devices) moved to the requested slot.
    Switched,
    /// The keyboard already used the requested slot; nothing moved and no
    /// hook action fired.
    AlreadyCurrent,
}

/// Why a software-initiated host switch was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum HostSwitchCommandError {
    /// No online keyboard matched the route.
    #[error("no connected keyboard matched the route")]
    KeyboardNotFound,
    /// The slot is outside the device's host count.
    #[error("the requested host slot does not exist on the keyboard")]
    InvalidSlot,
    /// The device reports the slot as empty; switching would strand it.
    #[error("the requested host slot is not paired on the keyboard")]
    UnpairedSlot,
    /// Pairing or another switch holds exclusive access, or the agent is
    /// dormant.
    #[error("the agent is busy; retry shortly")]
    Busy,
    /// Transport or protocol failure, as text.
    #[error("host switch failed: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::{HostSlotInfo, HostSlots, HostSwitchCommandError, HostSwitchOutcome};

    #[test]
    fn slots_serialize_with_one_based_numbers() -> Result<(), serde_json::Error> {
        let slots = HostSlots {
            current: Some(1),
            slots: vec![HostSlotInfo {
                slot: 1,
                paired: Some(true),
                name: Some("Michael’s MacBook Pro".into()),
            }],
        };
        let json = serde_json::to_string(&slots)?;
        assert!(json.contains("\"current\":1"));
        assert!(json.contains("\"slot\":1"));
        Ok(())
    }

    #[test]
    fn errors_render_for_humans() {
        assert_eq!(
            HostSwitchCommandError::UnpairedSlot.to_string(),
            "the requested host slot is not paired on the keyboard"
        );
        assert_eq!(
            HostSwitchOutcome::AlreadyCurrent,
            HostSwitchOutcome::AlreadyCurrent
        );
    }
}
