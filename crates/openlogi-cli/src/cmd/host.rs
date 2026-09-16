//! `openlogi host` — Easy-Switch host slots over the running agent.
//!
//! `list` reads slot status and names; `switch` moves the keyboard (and its
//! linked devices) to a slot and fires the keyboard's configured slot action.
//! Both need the agent: the agent owns the divert session, and a direct write
//! from this process would race it.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use clap::{Args, Subcommand};
use openlogi_core::device::DeviceKind;
use openlogi_core::hid::{
    DeviceRoute, HostSlotInfo, HostSlots, HostSwitchCommandError, HostSwitchOutcome,
};
use openlogi_ipc::{
    AgentClient, AgentSnapshot, ClientKind, HOST_SLOTS_BUDGET, HOST_SWITCH_BUDGET,
    PROTOCOL_VERSION, client,
};
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
    /// Show every keyboard's Easy-Switch host slots (online and offline).
    ///
    /// Exits 3 when no keyboard matched, in both `--json` and table output.
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
    /// Firmware codename, or the receiver's name when the device reported
    /// none.
    pub name: String,
    /// How this process would address the keyboard, rendered for display.
    pub route: String,
    /// Whether the keyboard was reachable at snapshot time.
    pub online: bool,
    /// The slot the keyboard is currently talking to. Omitted when the
    /// keyboard is offline or its slots could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_slot: Option<u8>,
    /// Every host slot the keyboard reported, in slot order. Omitted under
    /// the same conditions as `current_slot`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slots: Option<Vec<HostSlotInfo>>,
    /// Why `current_slot`/`slots` are missing — offline, a read failure, or a
    /// timeout. Omitted when the read succeeded.
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

/// Print every matching keyboard's host slots — as JSON (`--json`) or a
/// table. Exits [`KEYBOARD_NOT_FOUND`] when no keyboard matched, in *both*
/// output modes: stdout still carries `{"keyboards":[]}` in JSON mode, so a
/// script parsing it and a human reading "No keyboards found." see the same
/// signal.
async fn list(args: HostListArgs) -> Result<ExitCode> {
    let Some((agent, snapshot)) = connect().await else {
        return Ok(ExitCode::from(AGENT_UNAVAILABLE));
    };
    let mut keyboards = Vec::new();
    for keyboard in keyboards_in(&snapshot, args.device.as_deref()) {
        let (slots, error) = if keyboard.online {
            let mut ctx = context::current();
            // Above the agent's own host-slots budget, or tarpc would cancel
            // the handler mid-read instead of waiting it out.
            ctx.deadline = Instant::now() + HOST_SLOTS_BUDGET + Duration::from_secs(5);
            match tokio::time::timeout(
                HOST_SLOTS_BUDGET + Duration::from_secs(10),
                agent.host_slots(ctx, keyboard.route.clone()),
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
    let exit_code = ExitCode::from(list_exit_code(&keyboards));
    if args.json {
        println!("{}", serde_json::to_string(&ListReport { keyboards })?);
        return Ok(exit_code);
    }
    if keyboards.is_empty() {
        println!("No keyboards found.");
        return Ok(exit_code);
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
    Ok(exit_code)
}

/// The process exit status for `host list`'s keyboard set:
/// [`KEYBOARD_NOT_FOUND`] when nothing matched, 0 otherwise. The same rule
/// applies whether the caller asked for JSON or the table, so the two modes
/// never disagree on whether an empty result is a "success" or a refusal.
fn list_exit_code(keyboards: &[KeyboardReport]) -> u8 {
    if keyboards.is_empty() {
        KEYBOARD_NOT_FOUND
    } else {
        0
    }
}

/// Move the matched keyboard to `args.slot`, printing the outcome and
/// mapping it to a process exit code (see [`exit_code_for`]).
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
    let mut ctx = context::current();
    // Above the agent's own host-switch budget, or tarpc would cancel the
    // handler mid-switch instead of waiting it out.
    ctx.deadline = Instant::now() + HOST_SWITCH_BUDGET + Duration::from_secs(5);
    let switch_timeout = HOST_SWITCH_BUDGET + Duration::from_secs(10);
    let result = tokio::time::timeout(
        switch_timeout,
        agent.switch_host(ctx, keyboard.route.clone(), args.slot),
    )
    .await
    .with_context(|| {
        format!(
            "the agent did not answer the host switch within {}s",
            switch_timeout.as_secs()
        )
    })?
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

    use super::{KeyboardReport, exit_code_for, list_exit_code};

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
    fn offline_report_carries_error_and_omits_slots() -> Result<(), serde_json::Error> {
        let report = KeyboardReport::from_slots(
            "MX KEYS S MAC".into(),
            "direct 046d:b37c".into(),
            false,
            None,
            Some("offline".into()),
        );
        let json = serde_json::to_value(&report)?;
        assert_eq!(json["online"], false);
        assert_eq!(json["error"], "offline");
        assert!(json.get("current_slot").is_none());
        assert!(json.get("slots").is_none());
        Ok(())
    }

    #[test]
    fn list_exit_code_flags_an_empty_result_only() {
        assert_eq!(list_exit_code(&[]), 3);
        let keyboard = KeyboardReport::from_slots(
            "MX Keys".into(),
            "direct 046d:b37c".into(),
            true,
            None,
            None,
        );
        assert_eq!(list_exit_code(&[keyboard]), 0);
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
