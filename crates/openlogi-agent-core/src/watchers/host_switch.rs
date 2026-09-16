//! Keep configured keyboard → pointing-device host-switch links armed.

use std::thread;
use std::time::Duration;

use openlogi_core::binding::Action;
use openlogi_core::config::{HostSwitchActions, HostSwitchTrigger};
use openlogi_core::hid::{HostSwitchCommandError, HostSwitchOutcome};
use openlogi_hid::{
    ChannelPool, DeviceIoGate, DeviceRoute, HostSwitchError, HostSwitchStopReason,
    run_host_switch_session, switch_linked_hosts_with,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::receiver_access::{ExclusiveAccessReason, ReceiverAccess, ReceiverRequestState};
use crate::runtime::ActionDispatcher;

const DEPARTURE_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// One resolved link. Config keys are converted to live routes by the
/// orchestrator so the transport watcher never needs to understand inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSwitchLink {
    /// The keyboard's config key, handed to hook actions as
    /// `OPENLOGI_DEVICE_KEY`.
    pub keyboard_key: String,
    /// Keyboard whose host switch keys initiate the transition.
    pub keyboard: DeviceRoute,
    /// Pointing devices that follow the keyboard.
    pub targets: Vec<DeviceRoute>,
    /// Actions fired around the transition.
    pub actions: HostSwitchActions,
}

/// Read-only, lossless, coalescing view of resolved links.
pub type HostSwitchLinks = watch::Receiver<std::sync::Arc<Vec<HostSwitchLink>>>;

/// A software-initiated host switch, answered once applied or refused.
pub struct HostSwitchCommand {
    /// The keyboard to move.
    pub keyboard: DeviceRoute,
    /// 0-based HID++ host index to move to.
    pub host: u8,
    /// Where the outcome goes; dropped by a caller that stopped waiting.
    pub reply: oneshot::Sender<Result<HostSwitchOutcome, HostSwitchCommandError>>,
}

/// Sender half handed to the IPC server through `SharedRuntime`.
pub type HostSwitchCommands = mpsc::UnboundedSender<HostSwitchCommand>;

struct PendingCommand {
    keyboard: DeviceRoute,
    reply: oneshot::Sender<Result<HostSwitchOutcome, HostSwitchCommandError>>,
}

/// Spawn the host switch session manager.
pub fn spawn(
    links: &HostSwitchLinks,
    channel_pool: ChannelPool,
    receiver_access: ReceiverAccess,
    device_io: DeviceIoGate,
    dispatcher: ActionDispatcher,
    commands: mpsc::UnboundedReceiver<HostSwitchCommand>,
) {
    let links = links.clone();
    let receiver_requests = receiver_access.subscribe_requests();
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                warn!(%error, "host switch watcher: could not build tokio runtime");
                return;
            }
        };
        runtime.block_on(manage(
            links,
            channel_pool,
            receiver_access,
            receiver_requests,
            device_io,
            dispatcher,
            commands,
        ));
    });
}

struct HostSwitchManagerState {
    sessions: Vec<RunningSession>,
    next_generation: u64,
    restart_after: Vec<(HostSwitchLink, Instant)>,
    pending: Option<PendingCommand>,
}

impl HostSwitchManagerState {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
            next_generation: 0,
            restart_after: Vec::new(),
            pending: None,
        }
    }

    fn deadline(&self, requests: ReceiverRequestState, device_io_allowed: bool) -> Option<Instant> {
        if requests.any() || !device_io_allowed {
            return None;
        }
        self.restart_after
            .iter()
            .map(|(_, deadline)| *deadline)
            .min()
    }

    async fn reconcile(
        &mut self,
        requests: ReceiverRequestState,
        published: &std::sync::Arc<Vec<HostSwitchLink>>,
        receiver_access: &ReceiverAccess,
        channel_pool: &ChannelPool,
        done: &mpsc::UnboundedSender<SessionCompletion>,
        device_io: &DeviceIoGate,
    ) {
        // Keep armed listeners passive while sleeping. A reconcile to an
        // empty/changed link set would restore firmware controls and a retry
        // would reopen the HID transport during DarkWake.
        if !device_io.allows_io() {
            return;
        }
        let now = Instant::now();
        let wanted = if requests.any() {
            &[][..]
        } else {
            published.as_slice()
        };
        stop_unwanted(&mut self.sessions, wanted).await;
        self.restart_after
            .retain(|(link, _)| published.contains(link));
        for link in wanted {
            if self.sessions.iter().any(|session| session.link == *link)
                || self
                    .restart_after
                    .iter()
                    .any(|(delayed, deadline)| delayed == link && *deadline > now)
            {
                continue;
            }
            self.restart_after.retain(|(delayed, _)| delayed != link);
            let Some(receiver_lease) = receiver_access.try_acquire_for_session() else {
                self.restart_after.push((link.clone(), now + RETRY_DELAY));
                break;
            };
            self.next_generation = self.next_generation.wrapping_add(1);
            self.sessions.push(spawn_session(
                link.clone(),
                self.next_generation,
                receiver_lease,
                channel_pool.clone(),
                done.clone(),
                device_io.clone(),
            ));
        }
    }

    /// Apply one software-initiated switch. An armed session for the keyboard
    /// is stopped with `Requested`, so the switch runs through the very same
    /// completion path as a key press and replies from there; a keyboard with
    /// no armed session is switched directly and answered here.
    async fn handle_command(
        &mut self,
        command: HostSwitchCommand,
        links: &mut watch::Receiver<std::sync::Arc<Vec<HostSwitchLink>>>,
        channel_pool: &ChannelPool,
        receiver_access: &ReceiverAccess,
        device_io: &DeviceIoGate,
        dispatcher: &ActionDispatcher,
    ) {
        if !device_io.allows_io() || self.pending.is_some() {
            let _ = command.reply.send(Err(HostSwitchCommandError::Busy));
            return;
        }
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.link.keyboard == command.keyboard)
        {
            match session.stop.take() {
                Some(stop) => {
                    let _ = stop.send(HostSwitchStopReason::Requested(command.host));
                    self.pending = Some(PendingCommand {
                        keyboard: command.keyboard,
                        reply: command.reply,
                    });
                }
                None => {
                    let _ = command.reply.send(Err(HostSwitchCommandError::Busy));
                }
            }
            return;
        }
        let link = links
            .borrow()
            .iter()
            .find(|link| link.keyboard == command.keyboard)
            .cloned()
            .unwrap_or_else(|| HostSwitchLink {
                keyboard_key: String::new(),
                keyboard: command.keyboard.clone(),
                targets: Vec::new(),
                actions: HostSwitchActions::default(),
            });
        run_transition(
            links,
            channel_pool,
            receiver_access,
            dispatcher,
            link,
            command.host,
            HostSwitchTrigger::Command,
            Some(command.reply),
        )
        .await;
    }
}

async fn manage(
    mut links: watch::Receiver<std::sync::Arc<Vec<HostSwitchLink>>>,
    channel_pool: ChannelPool,
    receiver_access: ReceiverAccess,
    mut receiver_requests: watch::Receiver<ReceiverRequestState>,
    mut device_io: DeviceIoGate,
    dispatcher: ActionDispatcher,
    mut commands: mpsc::UnboundedReceiver<HostSwitchCommand>,
) {
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<SessionCompletion>();
    let mut state = HostSwitchManagerState::new();
    let mut reconcile = true;

    loop {
        if reconcile {
            reconcile = false;
            let device_io_allowed = device_io.allows_io();
            if device_io_allowed {
                let requests = *receiver_requests.borrow_and_update();
                let published = std::sync::Arc::clone(&links.borrow_and_update());
                state
                    .reconcile(
                        requests,
                        &published,
                        &receiver_access,
                        &channel_pool,
                        &done_tx,
                        &device_io,
                    )
                    .await;
            }
        }

        let requests = *receiver_requests.borrow();
        let deadline = state.deadline(requests, device_io.allows_io());
        if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            reconcile = true;
            continue;
        }

        tokio::select! {
            Some(completion) = done_rx.recv() => {
                match handle_completion(
                    &mut state,
                    &mut links,
                    &channel_pool,
                    &receiver_access,
                    &dispatcher,
                    &mut device_io,
                    completion,
                )
                .await
                {
                    CompletionOutcome::Shutdown => return,
                    CompletionOutcome::NotFound => {}
                    CompletionOutcome::Handled => reconcile = true,
                }
            }
            Some(command) = commands.recv() => {
                state
                    .handle_command(
                        command,
                        &mut links,
                        &channel_pool,
                        &receiver_access,
                        &device_io,
                        &dispatcher,
                    )
                    .await;
                reconcile = true;
            }
            result = links.changed() => {
                if result.is_err() {
                    return;
                }
                reconcile = true;
            }
            result = receiver_requests.changed() => {
                if result.is_err() {
                    return;
                }
                reconcile = true;
            }
            allowed = device_io.changed() => match allowed {
                Some(true) => reconcile = true,
                Some(false) => {}
                None => return,
            },
            () = wait_for_deadline(deadline) => {
                reconcile = true;
            }
        }
    }
}

/// What [`handle_completion`] did with one session-done signal.
enum CompletionOutcome {
    /// The device-I/O producer disappeared while waiting to resume; the
    /// watcher must shut down.
    Shutdown,
    /// The completion's generation no longer matches a running session (a
    /// stale signal from a session already replaced); nothing changed.
    NotFound,
    /// The completion was applied — either a retry was queued or a
    /// transition ran.
    Handled,
}

/// React to one finished host-switch session: apply the key press it
/// observed (replying to any pending command for the same keyboard from the
/// very same transition), or answer a pending command directly when the
/// session ended without a press, and otherwise queue a retry.
async fn handle_completion(
    state: &mut HostSwitchManagerState,
    links: &mut watch::Receiver<std::sync::Arc<Vec<HostSwitchLink>>>,
    channel_pool: &ChannelPool,
    receiver_access: &ReceiverAccess,
    dispatcher: &ActionDispatcher,
    device_io: &mut DeviceIoGate,
    completion: SessionCompletion,
) -> CompletionOutcome {
    let Some(index) = state
        .sessions
        .iter()
        .position(|session| session.generation == completion.generation)
    else {
        return CompletionOutcome::NotFound;
    };
    let completed = state.sessions.remove(index);
    let _ = completed.task.await;
    if let Some((link, host)) = completion.request {
        if !device_io.wait_until_allowed().await {
            return CompletionOutcome::Shutdown;
        }
        stop_all(&mut state.sessions, HostSwitchStopReason::Graceful).await;
        let pending = state
            .pending
            .take_if(|pending| pending.keyboard == link.keyboard);
        let trigger = if pending.is_some() {
            HostSwitchTrigger::Command
        } else {
            HostSwitchTrigger::Key
        };
        run_transition(
            links,
            channel_pool,
            receiver_access,
            dispatcher,
            link,
            host,
            trigger,
            pending.map(|pending| pending.reply),
        )
        .await;
    } else {
        if let Some(pending) = state
            .pending
            .take_if(|pending| pending.keyboard == completed.link.keyboard)
        {
            let _ = pending.reply.send(Err(HostSwitchCommandError::Io(
                "host switch session ended before the switch was applied".into(),
            )));
        }
        if device_io.allows_io() {
            state
                .restart_after
                .push((completed.link, Instant::now() + RETRY_DELAY));
        }
    }
    CompletionOutcome::Handled
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline).await;
    } else {
        std::future::pending::<()>().await;
    }
}

fn spawn_session(
    link: HostSwitchLink,
    generation: u64,
    receiver_lease: crate::receiver_access::SessionReceiverLease,
    pool: ChannelPool,
    done: mpsc::UnboundedSender<SessionCompletion>,
    device_io: DeviceIoGate,
) -> RunningSession {
    let (stop, stop_rx) = oneshot::channel();
    let session_link = link.clone();
    let task = tokio::spawn(async move {
        let _receiver_lease = receiver_lease;
        let keyboard = session_link.keyboard.clone();
        let request =
            match run_host_switch_session(session_link.keyboard.clone(), stop_rx, pool, device_io)
                .await
            {
                Ok(host) => host.map(|host| (session_link, host)),
                Err(error) => {
                    debug!(%error, route = %keyboard, "host switch session ended");
                    None
                }
            };
        let _ = done.send(SessionCompletion {
            generation,
            request,
        });
    });
    RunningSession {
        link,
        generation,
        stop: Some(stop),
        task,
    }
}

struct RunningSession {
    link: HostSwitchLink,
    generation: u64,
    stop: Option<oneshot::Sender<HostSwitchStopReason>>,
    task: tokio::task::JoinHandle<()>,
}

struct SessionCompletion {
    generation: u64,
    request: Option<(HostSwitchLink, u8)>,
}

async fn stop_all(sessions: &mut Vec<RunningSession>, reason: HostSwitchStopReason) {
    let running = std::mem::take(sessions);
    let mut tasks = Vec::with_capacity(running.len());
    for RunningSession { stop, task, .. } in running {
        if let Some(stop) = stop {
            let _ = stop.send(reason);
        }
        tasks.push(task);
    }
    for task in tasks {
        let _ = task.await;
    }
}

async fn stop_unwanted(sessions: &mut Vec<RunningSession>, wanted: &[HostSwitchLink]) {
    let mut index = 0;
    while index < sessions.len() {
        if wanted.contains(&sessions[index].link) {
            index += 1;
            continue;
        }
        let RunningSession { stop, task, .. } = sessions.remove(index);
        if let Some(stop) = stop {
            let _ = stop.send(HostSwitchStopReason::Graceful);
        }
        let _ = task.await;
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the watcher's other manager entry points, which thread the same handles"
)]
async fn run_transition(
    links: &mut watch::Receiver<std::sync::Arc<Vec<HostSwitchLink>>>,
    channel_pool: &ChannelPool,
    receiver_access: &ReceiverAccess,
    dispatcher: &ActionDispatcher,
    link: HostSwitchLink,
    host: u8,
    trigger: HostSwitchTrigger,
    reply: Option<oneshot::Sender<Result<HostSwitchOutcome, HostSwitchCommandError>>>,
) {
    let _lease = receiver_access
        .acquire_exclusive(ExclusiveAccessReason::HostTransition)
        .await;
    let fire = || {
        if !fire_slot_action(dispatcher, &link, host, trigger) && !link.actions.is_empty() {
            debug!(host, route = %link.keyboard, "no host switch action configured for slot");
        }
    };
    let result =
        switch_linked_hosts_with(&link.keyboard, &link.targets, host, channel_pool, fire).await;
    if let Some(reply) = reply {
        let _ = reply.send(outcome_of(&result));
    }
    match result {
        Ok(true) => wait_for_departure(links, &link.keyboard).await,
        Ok(false) => {}
        Err(error) => {
            debug!(%error, route = %link.keyboard, host, "keyboard host switch failed");
        }
    }
}

/// Translate a switch result into the wire outcome.
fn outcome_of(
    result: &Result<bool, HostSwitchError>,
) -> Result<HostSwitchOutcome, HostSwitchCommandError> {
    match result {
        Ok(true) => Ok(HostSwitchOutcome::Switched),
        Ok(false) => Ok(HostSwitchOutcome::AlreadyCurrent),
        Err(HostSwitchError::KeyboardNotFound) => Err(HostSwitchCommandError::KeyboardNotFound),
        Err(HostSwitchError::HostSlotEmpty { .. }) => Err(HostSwitchCommandError::UnpairedSlot),
        Err(HostSwitchError::InvalidHost { .. }) => Err(HostSwitchCommandError::InvalidSlot),
        Err(error) => Err(HostSwitchCommandError::Io(error.to_string())),
    }
}

/// The action and environment for a switch of `link`'s keyboard to 0-based
/// `host`, or `None` when that slot has no action configured.
fn plan_slot_action(
    link: &HostSwitchLink,
    host: u8,
    trigger: HostSwitchTrigger,
) -> Option<(&Action, Vec<(String, String)>)> {
    let action = link.actions.for_host_index(host)?;
    Some((action, trigger.hook_env(&link.keyboard_key, Some(host))))
}

/// Fire the configured slot action, fire-and-forget. Returns whether one was
/// configured. Never awaited by the transition: the dispatcher hands script
/// actions to a worker thread and returns immediately.
fn fire_slot_action(
    dispatcher: &ActionDispatcher,
    link: &HostSwitchLink,
    host: u8,
    trigger: HostSwitchTrigger,
) -> bool {
    let Some((action, env)) = plan_slot_action(link, host, trigger) else {
        return false;
    };
    debug!(host, route = %link.keyboard, trigger = trigger.as_str(), "firing host switch action");
    dispatcher.dispatch_hook(action, Some(&link.keyboard_key), &env);
    true
}

async fn wait_for_departure(
    links: &mut watch::Receiver<std::sync::Arc<Vec<HostSwitchLink>>>,
    keyboard: &DeviceRoute,
) {
    let deadline = tokio::time::sleep(DEPARTURE_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        let departed = !links
            .borrow_and_update()
            .iter()
            .any(|link| link.keyboard == *keyboard);
        if departed {
            return;
        }
        tokio::select! {
            result = links.changed() => {
                if result.is_err() {
                    return;
                }
            }
            () = &mut deadline => {
                warn!(route = %keyboard, "host transition departure was not observed");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(slot: u8) -> DeviceRoute {
        DeviceRoute::Bolt {
            receiver_uid: "cafe".to_owned(),
            slot,
        }
    }

    #[test]
    fn suspended_device_io_disables_retry_deadlines() {
        let retry_at = Instant::now() + RETRY_DELAY;
        let mut state = HostSwitchManagerState::new();
        state.restart_after.push((
            HostSwitchLink {
                keyboard_key: "keyboard".into(),
                keyboard: route(1),
                targets: vec![route(2)],
                actions: HostSwitchActions::default(),
            },
            retry_at,
        ));

        assert_eq!(
            state.deadline(ReceiverRequestState::default(), true),
            Some(retry_at),
        );
        assert_eq!(
            state.deadline(ReceiverRequestState::default(), false),
            None,
            "host-switch retries must stay dormant until visible resume",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn departure_publication_finishes_wait_without_advancing_time() {
        let keyboard = route(1);
        let link = HostSwitchLink {
            keyboard_key: "keyboard".into(),
            keyboard: keyboard.clone(),
            targets: vec![route(2)],
            actions: HostSwitchActions::default(),
        };
        let (links, mut published) = watch::channel(std::sync::Arc::new(vec![link]));
        let started = Instant::now();
        let waiting = tokio::spawn(async move {
            wait_for_departure(&mut published, &keyboard).await;
            Instant::now()
        });
        tokio::task::yield_now().await;

        links.send_replace(std::sync::Arc::new(Vec::new()));
        tokio::task::yield_now().await;

        assert_eq!(
            waiting.await.expect("departure waiter should finish"),
            started,
            "the link publication should reconcile departure immediately"
        );
    }

    #[test]
    fn slot_action_env_names_the_keyboard_and_one_based_slot() {
        let link = HostSwitchLink {
            keyboard_key: "unit:7b520554".into(),
            keyboard: route(1),
            targets: Vec::new(),
            actions: HostSwitchActions {
                slot_3: Some(openlogi_core::binding::Action::RunShellCommand("x".into())),
                ..HostSwitchActions::default()
            },
        };
        let (action, env) =
            plan_slot_action(&link, 2, HostSwitchTrigger::Key).expect("slot 3 is configured");
        assert_eq!(
            action,
            &openlogi_core::binding::Action::RunShellCommand("x".into())
        );
        assert!(env.contains(&("OPENLOGI_HOST_SLOT".to_string(), "3".to_string())));
        assert!(env.contains(&(
            "OPENLOGI_DEVICE_KEY".to_string(),
            "unit:7b520554".to_string()
        )));
        assert!(env.contains(&("OPENLOGI_TRIGGER".to_string(), "key".to_string())));
        assert!(plan_slot_action(&link, 0, HostSwitchTrigger::Key).is_none());
    }

    fn test_dispatcher() -> (crate::runtime::ActionRuntime, ActionDispatcher) {
        let (ring, _triggers) = mpsc::unbounded_channel();
        let runtime = crate::runtime::ActionRuntime::new(
            std::sync::Arc::new(std::sync::RwLock::new(crate::DpiCycles::default())),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            openlogi_hid::ChannelRegistry::default(),
            ReceiverAccess::default(),
            openlogi_hid::device_io_channel().1,
            ring,
        )
        .expect("button worker starts");
        let dispatcher = runtime.dispatcher();
        (runtime, dispatcher)
    }

    #[test]
    fn switch_results_map_to_wire_outcomes() {
        use openlogi_core::hid::{HostSwitchCommandError, HostSwitchOutcome};
        use openlogi_hid::HostSwitchError;
        assert_eq!(outcome_of(&Ok(true)), Ok(HostSwitchOutcome::Switched));
        assert_eq!(
            outcome_of(&Ok(false)),
            Ok(HostSwitchOutcome::AlreadyCurrent)
        );
        assert_eq!(
            outcome_of(&Err(HostSwitchError::KeyboardNotFound)),
            Err(HostSwitchCommandError::KeyboardNotFound)
        );
        assert_eq!(
            outcome_of(&Err(HostSwitchError::HostSlotEmpty { host: 1 })),
            Err(HostSwitchCommandError::UnpairedSlot)
        );
        assert_eq!(
            outcome_of(&Err(HostSwitchError::InvalidHost {
                host: 3,
                host_count: 3
            })),
            Err(HostSwitchCommandError::InvalidSlot)
        );
        assert!(matches!(
            outcome_of(&Err(HostSwitchError::UnsupportedKeyboard)),
            Err(HostSwitchCommandError::Io(_))
        ));
    }

    #[tokio::test]
    async fn a_command_with_no_armed_session_and_no_keyboard_is_not_found() {
        use openlogi_core::hid::HostSwitchCommandError;
        let (links_tx, links) = watch::channel(std::sync::Arc::new(Vec::new()));
        let _keep = links_tx;
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut state = HostSwitchManagerState::new();
        let (_, dispatcher) = test_dispatcher();
        let mut links = links;
        state
            .handle_command(
                HostSwitchCommand {
                    keyboard: route(9),
                    host: 1,
                    reply: reply_tx,
                },
                &mut links,
                &openlogi_hid::host::channel_pool(),
                &ReceiverAccess::default(),
                &openlogi_hid::device_io_channel().1,
                &dispatcher,
            )
            .await;
        assert_eq!(
            reply_rx.await.expect("a reply is always sent"),
            Err(HostSwitchCommandError::KeyboardNotFound)
        );
    }
}
