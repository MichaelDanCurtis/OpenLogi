//! `openlogi host` — Easy-Switch host slots over the running agent.
//!
//! `list` reads slot status and names; `switch` moves the keyboard (and its
//! linked devices) to a slot and fires the keyboard's configured slot action.
//! Both need the agent: the agent owns the divert session, and a direct write
//! from this process would race it.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand};
use openlogi_core::device::DeviceKind;
use openlogi_core::hid::{
    DeviceRoute, HostSlotInfo, HostSlots, HostSwitchCommandError, HostSwitchOutcome,
};
use openlogi_ipc::{AgentClient, AgentSnapshot, ClientKind, PROTOCOL_VERSION, client};
use serde::Serialize;
use tarpc::context;

/// Exit status when the agent cannot be reached or speaks another protocol.
const AGENT_UNAVAILABLE: u8 = 4;
/// Exit status when no online keyboard matched.
const KEYBOARD_NOT_FOUND: u8 = 3;
/// Exit status when the agent refused the slot.
const REFUSED: u8 = 2;

#[derive(Debug, Subcommand)]
pub enum HostCmd {
    /// Show every online keyboard's host slots.
    List(HostListArgs),
    /// Move the keyboard and its linked devices to a slot (1–3).
    Switch(HostSwitchArgs),
}

#[derive(Debug, Args)]
pub struct HostListArgs {
    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    /// Only keyboards whose name contains this text (case-insensitive).
    #[arg(long, value_name = "NAME")]
    pub device: Option<String>,
}

#[derive(Debug, Args)]
pub struct HostSwitchArgs {
    /// Easy-Switch slot, as printed on the key cap.
    #[arg(value_parser = clap::value_parser!(u8).range(1..=3))]
    pub slot: u8,
    /// Pick the keyboard whose name contains this text (case-insensitive)
    /// instead of requiring exactly one online keyboard.
    #[arg(long, value_name = "NAME")]
    pub device: Option<String>,
}

impl HostCmd {
    /// Run the subcommand and report the process exit status.
    pub async fn run(self) -> Result<ExitCode> {
        match self {
            Self::List(args) => list(args).await,
            Self::Switch(args) => switch(args).await,
        }
    }
}

/// One keyboard in `host list --json`.
#[derive(Debug, Serialize)]
pub(crate) struct KeyboardReport {
    pub name: String,
    pub route: String,
    pub online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_slot: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slots: Option<Vec<HostSlotInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl KeyboardReport {
    fn from_slots(
        name: String,
        route: String,
        online: bool,
        slots: Option<HostSlots>,
        error: Option<String>,
    ) -> Self {
        let (current_slot, slots) = match slots {
            Some(slots) => (Some(slots.current), Some(slots.slots)),
            None => (None, None),
        };
        Self {
            name,
            route,
            online,
            current_slot,
            slots,
            error,
        }
    }
}

#[derive(Debug, Serialize)]
struct ListReport {
    keyboards: Vec<KeyboardReport>,
}

struct Keyboard {
    name: String,
    route: DeviceRoute,
    online: bool,
}

async fn list(args: HostListArgs) -> Result<ExitCode> {
    let Some((agent, snapshot)) = connect().await else {
        return Ok(ExitCode::from(AGENT_UNAVAILABLE));
    };
    let mut keyboards = Vec::new();
    for keyboard in keyboards_in(&snapshot, args.device.as_deref()) {
        let (slots, error) = if keyboard.online {
            match tokio::time::timeout(
                Duration::from_secs(10),
                agent.host_slots(context::current(), keyboard.route.clone()),
            )
            .await
            {
                Ok(Ok(Ok(slots))) => (Some(slots), None),
                Ok(Ok(Err(error))) => (None, Some(error.to_string())),
                Ok(Err(error)) => (None, Some(error.to_string())),
                Err(_) => (None, Some("timed out reading host slots".into())),
            }
        } else {
            (None, Some("offline".into()))
        };
        keyboards.push(KeyboardReport::from_slots(
            keyboard.name,
            keyboard.route.to_string(),
            keyboard.online,
            slots,
            error,
        ));
    }
    if args.json {
        println!("{}", serde_json::to_string(&ListReport { keyboards })?);
        return Ok(ExitCode::SUCCESS);
    }
    if keyboards.is_empty() {
        println!("No keyboards found.");
        return Ok(ExitCode::from(KEYBOARD_NOT_FOUND));
    }
    for keyboard in &keyboards {
        println!("{} ({})", keyboard.name, keyboard.route);
        match (&keyboard.slots, &keyboard.error) {
            (Some(slots), _) => {
                for slot in slots {
                    let marker = if Some(slot.slot) == keyboard.current_slot {
                        "●"
                    } else {
                        "○"
                    };
                    let paired = match slot.paired {
                        Some(true) => "paired",
                        Some(false) => "empty",
                        None => "unknown",
                    };
                    println!(
                        "  {marker} slot {}: {paired} {}",
                        slot.slot,
                        slot.name.as_deref().unwrap_or("—")
                    );
                }
            }
            (None, Some(error)) => println!("  {error}"),
            (None, None) => {}
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn switch(args: HostSwitchArgs) -> Result<ExitCode> {
    let Some((agent, snapshot)) = connect().await else {
        return Ok(ExitCode::from(AGENT_UNAVAILABLE));
    };
    let candidates: Vec<Keyboard> = keyboards_in(&snapshot, args.device.as_deref())
        .into_iter()
        .filter(|keyboard| keyboard.online)
        .collect();
    let keyboard = match candidates.as_slice() {
        [keyboard] => keyboard,
        [] => {
            eprintln!("no online keyboard matched");
            return Ok(ExitCode::from(KEYBOARD_NOT_FOUND));
        }
        many => {
            let names: Vec<&str> = many.iter().map(|keyboard| keyboard.name.as_str()).collect();
            return Err(anyhow!(
                "several online keyboards match ({}); pick one with --device",
                names.join(", ")
            ));
        }
    };
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        agent.switch_host(context::current(), keyboard.route.clone(), args.slot),
    )
    .await
    .context("the agent did not answer the host switch within 15 seconds")?
    .context("host switch RPC failed")?;
    match &result {
        Ok(HostSwitchOutcome::Switched) => println!("{} → slot {}", keyboard.name, args.slot),
        Ok(HostSwitchOutcome::AlreadyCurrent) => {
            println!("{} already on slot {}", keyboard.name, args.slot);
        }
        Err(error) => eprintln!("{error}"),
    }
    Ok(ExitCode::from(exit_code_for(&result)))
}

/// The process exit status for a switch result.
pub(crate) fn exit_code_for(result: &Result<HostSwitchOutcome, HostSwitchCommandError>) -> u8 {
    match result {
        Ok(_) => 0,
        Err(HostSwitchCommandError::UnpairedSlot | HostSwitchCommandError::InvalidSlot) => REFUSED,
        Err(HostSwitchCommandError::KeyboardNotFound) => KEYBOARD_NOT_FOUND,
        Err(HostSwitchCommandError::Busy | HostSwitchCommandError::Io(_)) => 1,
    }
}

/// Keyboards in the agent's snapshot, optionally filtered by name.
fn keyboards_in(snapshot: &AgentSnapshot, filter: Option<&str>) -> Vec<Keyboard> {
    let needle = filter.map(str::to_lowercase);
    let mut keyboards = Vec::new();
    for inventory in &snapshot.inventory {
        for paired in &inventory.paired {
            if paired.kind != DeviceKind::Keyboard {
                continue;
            }
            let Some(route) = DeviceRoute::device_route_for(inventory, paired.slot) else {
                continue;
            };
            let name = paired
                .codename
                .clone()
                .unwrap_or_else(|| inventory.receiver.name.clone());
            if needle
                .as_deref()
                .is_some_and(|needle| !name.to_lowercase().contains(needle))
            {
                continue;
            }
            keyboards.push(Keyboard {
                name,
                route,
                online: paired.online,
            });
        }
    }
    keyboards
}

/// Connect, check the protocol, declare as a CLI, and fetch one snapshot.
/// `None` (with a message on stderr) when no compatible agent answers.
async fn connect() -> Option<(AgentClient, AgentSnapshot)> {
    let conn = match tokio::time::timeout(Duration::from_secs(2), client::connect()).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(error)) => {
            eprintln!("no agent reachable ({error}); `openlogi host` needs the running agent");
            return None;
        }
        Err(_) => {
            eprintln!("the agent did not answer the handshake");
            return None;
        }
    };
    if conn.version != PROTOCOL_VERSION {
        eprintln!(
            "the agent speaks protocol v{}, this CLI expects v{PROTOCOL_VERSION}",
            conn.version
        );
        return None;
    }
    tokio::time::timeout(
        Duration::from_secs(2),
        conn.client
            .declare_client(context::current(), ClientKind::Cli),
    )
    .await
    .ok()?
    .ok()?;
    let snapshot = tokio::time::timeout(
        Duration::from_secs(5),
        conn.client.snapshot(context::current()),
    )
    .await
    .ok()?
    .ok()?;
    Some((conn.client, snapshot))
}

#[cfg(test)]
mod tests {
    use openlogi_core::hid::{HostSlotInfo, HostSlots};

    use super::{KeyboardReport, exit_code_for};

    #[test]
    fn report_serializes_the_documented_shape() -> Result<(), serde_json::Error> {
        let report = KeyboardReport::from_slots(
            "MX KEYS S MAC".into(),
            "direct 046d:b37c".into(),
            true,
            Some(HostSlots {
                current: 1,
                slots: vec![HostSlotInfo {
                    slot: 1,
                    paired: Some(true),
                    name: Some("Mac".into()),
                }],
            }),
            None,
        );
        let json = serde_json::to_value(&report)?;
        assert_eq!(json["current_slot"], 1);
        assert_eq!(json["slots"][0]["slot"], 1);
        assert_eq!(json["slots"][0]["paired"], true);
        Ok(())
    }

    #[test]
    fn exit_codes_follow_the_contract() {
        use openlogi_core::hid::{HostSwitchCommandError as E, HostSwitchOutcome as O};
        assert_eq!(exit_code_for(&Ok(O::Switched)), 0);
        assert_eq!(exit_code_for(&Ok(O::AlreadyCurrent)), 0);
        assert_eq!(exit_code_for(&Err(E::UnpairedSlot)), 2);
        assert_eq!(exit_code_for(&Err(E::InvalidSlot)), 2);
        assert_eq!(exit_code_for(&Err(E::KeyboardNotFound)), 3);
        assert_eq!(exit_code_for(&Err(E::Busy)), 1);
        assert_eq!(exit_code_for(&Err(E::Io("x".into()))), 1);
    }
}
