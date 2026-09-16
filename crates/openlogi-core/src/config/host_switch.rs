//! Actions fired around a keyboard's host switching, plus the environment
//! those actions receive.

use serde::{Deserialize, Serialize};

use crate::binding::Action;

/// Actions fired around a keyboard's host switching.
///
/// `slot_N` fires the instant a switch to Easy-Switch slot `N` is observed —
/// from the keyboard's own host key or from a software-initiated switch — in
/// parallel with the device moves and never delaying them. `arrive` fires when
/// the keyboard transitions offline → online after the agent's first inventory
/// reconcile, as a fallback for a machine that was left without an agent.
///
/// Every field is optional and the whole table is omitted from `config.toml`
/// while empty, so existing files serialize unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSwitchActions {
    /// Fired on a switch to Easy-Switch slot 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_1: Option<Action>,
    /// Fired on a switch to Easy-Switch slot 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_2: Option<Action>,
    /// Fired on a switch to Easy-Switch slot 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_3: Option<Action>,
    /// Fired when this keyboard connects to this host after the agent's first
    /// inventory reconcile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrive: Option<Action>,
}

impl HostSwitchActions {
    /// The action for a 0-based HID++ host index (`0` → [`Self::slot_1`]).
    #[must_use]
    pub fn for_host_index(&self, index: u8) -> Option<&Action> {
        match index {
            0 => self.slot_1.as_ref(),
            1 => self.slot_2.as_ref(),
            2 => self.slot_3.as_ref(),
            _ => None,
        }
    }

    /// Whether no slot and no arrive action is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slot_1.is_none()
            && self.slot_2.is_none()
            && self.slot_3.is_none()
            && self.arrive.is_none()
    }
}

/// What caused a host-switch hook to fire, as reported to the hook in
/// `OPENLOGI_TRIGGER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostSwitchTrigger {
    /// The keyboard's own host key was pressed.
    Key,
    /// Reserved for a future software-initiated switch (`openlogi host
    /// switch`); nothing on this branch constructs it yet.
    Command,
    /// The keyboard connected to this host.
    Arrive,
}

impl HostSwitchTrigger {
    /// The `OPENLOGI_TRIGGER` value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Command => "command",
            Self::Arrive => "arrive",
        }
    }

    /// The environment a hook action receives: `OPENLOGI_HOST_SLOT` (1-based,
    /// only when `host_index` is known), `OPENLOGI_DEVICE_KEY`, and
    /// `OPENLOGI_TRIGGER`.
    #[must_use]
    pub fn hook_env(self, keyboard_key: &str, host_index: Option<u8>) -> Vec<(String, String)> {
        let mut env = Vec::with_capacity(3);
        if let Some(index) = host_index {
            env.push((
                "OPENLOGI_HOST_SLOT".to_string(),
                (u16::from(index) + 1).to_string(),
            ));
        }
        env.push(("OPENLOGI_DEVICE_KEY".to_string(), keyboard_key.to_string()));
        env.push(("OPENLOGI_TRIGGER".to_string(), self.as_str().to_string()));
        env
    }
}

#[cfg(test)]
mod tests {
    use super::{HostSwitchActions, HostSwitchTrigger};
    use crate::binding::Action;

    #[test]
    fn slots_are_addressed_by_zero_based_host_index() {
        let actions = HostSwitchActions {
            slot_1: Some(Action::RunShellCommand("one".into())),
            slot_3: Some(Action::RunShellCommand("three".into())),
            ..HostSwitchActions::default()
        };
        assert_eq!(
            actions.for_host_index(0),
            Some(&Action::RunShellCommand("one".into()))
        );
        assert_eq!(actions.for_host_index(1), None);
        assert_eq!(
            actions.for_host_index(2),
            Some(&Action::RunShellCommand("three".into()))
        );
        assert_eq!(
            actions.for_host_index(3),
            None,
            "a fourth slot never exists"
        );
    }

    #[test]
    fn empty_means_no_slot_and_no_arrive() {
        assert!(HostSwitchActions::default().is_empty());
        let arrive_only = HostSwitchActions {
            arrive: Some(Action::RunShellCommand("hi".into())),
            ..HostSwitchActions::default()
        };
        assert!(!arrive_only.is_empty());
    }

    #[test]
    fn round_trips_through_toml_and_omits_absent_entries() -> Result<(), Box<dyn std::error::Error>>
    {
        let actions: HostSwitchActions = toml::from_str(
            r#"slot_2 = { RunShellCommand = "/usr/local/bin/deskswitch switch --slot 2" }
arrive = { RunShellCommand = "/usr/local/bin/deskswitch arrive" }"#,
        )?;
        assert_eq!(
            actions.slot_2,
            Some(Action::RunShellCommand(
                "/usr/local/bin/deskswitch switch --slot 2".into()
            ))
        );
        assert_eq!(actions.slot_1, None);
        let serialized = toml::to_string(&actions)?;
        assert!(serialized.contains("slot_2"));
        assert!(!serialized.contains("slot_1"));
        assert!(!serialized.contains("slot_3"));
        Ok(())
    }

    #[test]
    fn unknown_slot_keys_are_rejected() {
        let result: Result<HostSwitchActions, _> =
            toml::from_str(r#"slot_4 = { RunShellCommand = "x" }"#);
        result.expect_err("slot_4 must not be silently accepted");
    }

    #[test]
    fn hook_env_carries_slot_key_and_trigger() {
        let env = HostSwitchTrigger::Key.hook_env("unit:7b520554", Some(1));
        assert_eq!(
            env,
            vec![
                ("OPENLOGI_HOST_SLOT".to_string(), "2".to_string()),
                (
                    "OPENLOGI_DEVICE_KEY".to_string(),
                    "unit:7b520554".to_string()
                ),
                ("OPENLOGI_TRIGGER".to_string(), "key".to_string()),
            ]
        );
    }

    #[test]
    fn arrive_env_has_no_slot() {
        let env = HostSwitchTrigger::Arrive.hook_env("unit:7b520554", None);
        assert!(env.iter().all(|(name, _)| name != "OPENLOGI_HOST_SLOT"));
        assert!(env.contains(&("OPENLOGI_TRIGGER".to_string(), "arrive".to_string())));
    }
}
