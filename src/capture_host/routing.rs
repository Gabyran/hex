//! Where a written capture goes: a new Lody session for the Agent Role in
//! `capture.routing`'s `agentLody`, made through Lody's own CLI by Scrawl's
//! router crate (`crates/router`, `lody`) run in Hex's process.
//!
//! Once a capture is written the finisher hands it to the one
//! `capture-route` thread ([`Routes`]), which sends it at once and carries
//! out the capsule's Send and Undo for the newest capture, in the order they
//! came. Every change lands in the snapshot's `last.route`. With no Agent
//! Role set nothing is sent: the capture is only copied.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use scrawl_capture::capture::RouteContext;
use scrawl_capture::delivery::{self, Delivery};
use scrawl_capture::lody;
use scrawl_capture::model::{Destination, ScrawlBundle};
use scrawl_capture::routing;
use scrawl_capture::{Router, RouterConfig, RouterDirs};

use super::{SharedOutcomes, lock};

/// How often sends Lody didn't take are retried.
const RETRY_EVERY: Duration = Duration::from_secs(15);
/// How long each of the two CLI calls that take a send back may wait on Lody.
const UNDO_DEADLINE: Duration = Duration::from_secs(30);
const QUEUE: usize = 16;

// ---------------------------------------------------------------------------
// What views see.

/// A written capture's route: where it goes and what became of the send.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureRoute {
    pub answer: RouteAnswer,
    pub delivery: Option<RouteDelivery>,
    pub offer: Option<String>,
}

impl CaptureRoute {
    fn of(answer: RouteAnswer) -> Self {
        Self {
            answer,
            delivery: None,
            offer: None,
        }
    }

    /// The written capture, while the router works out where it goes.
    pub(super) fn looking() -> Self {
        Self::of(RouteAnswer::Looking)
    }
}

/// The route of a capture the router never got to.
pub(super) fn unrouted() -> CaptureRoute {
    CaptureRoute::of(RouteAnswer::NoAnswer { recent: Vec::new() })
}

/// Where a capture can go: the Lody Agent Role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteDest {
    /// The Agent Role as configured.
    pub session: String,
    pub name: String,
    /// A quiet word on why.
    pub why: String,
}

/// `recent` is every other place the capture could go, newest first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteAnswer {
    /// Written; the router is still working.
    Looking,
    /// The Agent Role the person set: sent to `to` at once, and the
    /// capsule's Undo takes it back.
    Rule {
        to: RouteDest,
        suggested: Vec<RouteDest>,
        recent: Vec<RouteDest>,
        site: Option<String>,
    },
    #[allow(
        dead_code,
        reason = "Lody has one destination; the capsule still draws these"
    )]
    Guess {
        suggested: Vec<RouteDest>,
        recent: Vec<RouteDest>,
        site: Option<String>,
    },
    #[allow(
        dead_code,
        reason = "Lody has one destination; the capsule still draws these"
    )]
    Nothing { recent: Vec<RouteDest> },
    /// The router failed.
    NoAnswer { recent: Vec<RouteDest> },
    #[allow(
        dead_code,
        reason = "Lody has one destination; the capsule still draws these"
    )]
    Unreachable { why: String },
}

/// What became of a capture's send; `to` is the Agent Role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteDelivery {
    Sending {
        to: String,
    },
    Sent {
        to: String,
    },
    /// Lody didn't take it; retried every 15 s. `why` says what went wrong
    /// in a few words ("Lody isn't running").
    Queued {
        to: String,
        why: String,
    },
    /// Never sent, withdrawn, reverted, or disowned.
    TakenBack,
    /// Lody wouldn't give it back.
    TooLate {
        to: String,
    },
    Failed {
        to: String,
        why: String,
    },
}

// ---------------------------------------------------------------------------
// The router.

/// The router with Hex's `capture.routing`: its `agentLody` settings and the
/// captures folder. Its log lines go to tracing (`deliver: …` at info).
pub fn router() -> Router {
    let settings = crate::app_settings::capture_settings();
    let config = settings.routing.config().clone();
    let dirs = RouterDirs {
        captures: Some(settings.captures_dir()),
        ..RouterDirs::default()
    };
    Router::new(config, dirs)
        .with_log(|line| {
            if line.starts_with("deliver:") {
                tracing::info!(line, "capture router");
            } else {
                tracing::debug!(line, "capture router");
            }
        })
        .with_summary(scrawl_capture::sketch::summary_markdown)
}

/// Whether captures are sent: `autoSend` is on and an Agent Role is set.
fn sends(config: &RouterConfig) -> bool {
    config.send_captures && config.agent_lody.role().is_some()
}

/// The router as it was when a capture stopped, handed to the route thread
/// with the written capture.
pub(super) struct Look {
    router: Router,
}

/// Starts routing, or `None` when captures aren't sent (`autoSend` off, or
/// no Agent Role set): the capture is then only copied.
pub(super) fn look(_context: RouteContext) -> Option<Look> {
    look_with(router())
}

fn look_with(router: Router) -> Option<Look> {
    sends(router.config()).then_some(Look { router })
}

// ---------------------------------------------------------------------------
// The capture-route thread.

pub(super) enum Job {
    Written {
        bundle: PathBuf,
        look: Look,
    },
    Send {
        to: String,
        always: bool,
    },
    Undo,
    /// `hex capture send`: any capture, to its Agent Role.
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    SendBundle {
        bundle: PathBuf,
        to: Option<String>,
        always: bool,
        reply: SyncSender<Result<String, String>>,
    },
}

/// The running host's route queue, for `hex capture send`, with the
/// [`Routes`] it belongs to.
static JOBS: Mutex<Option<(u64, SyncSender<Job>)>> = Mutex::new(None);
static NEXT_ROUTES: AtomicU64 = AtomicU64::new(0);

/// How long `hex capture send` waits for the send, inside the control
/// deadline.
#[cfg_attr(target_os = "windows", allow(dead_code))]
const CLI_WAIT: Duration = Duration::from_secs(8);

/// Sends `bundle` through the `capture-route` thread and says what became
/// of it: as the capsule's pick when it is the newest capture, so its
/// route and Undo follow.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(super) fn send_bundle(
    bundle: PathBuf,
    to: Option<String>,
    always: bool,
) -> Result<String, String> {
    let jobs = JOBS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|(_, jobs)| jobs.clone())
        .ok_or("captures are off")?;
    let (reply, answer) = mpsc::sync_channel(1);
    let job = Job::SendBundle {
        bundle,
        to,
        always,
        reply,
    };
    if !send(&jobs, job) {
        return Err("the capture router is busy; try again shortly".into());
    }
    answer
        .recv_timeout(CLI_WAIT)
        .unwrap_or_else(|_| Ok("Still sending; `hex capture show` tells how it went".into()))
}

/// The `capture-route` thread and its queue.
pub(super) struct Routes {
    id: u64,
    jobs: SyncSender<Job>,
    thread: JoinHandle<()>,
}

impl Routes {
    pub(super) fn start(outcomes: SharedOutcomes) -> std::io::Result<Self> {
        let (jobs, queued) = mpsc::sync_channel(QUEUE);
        let thread = thread::Builder::new()
            .name("capture-route".into())
            .spawn(move || Worker::new(outcomes).run(&queued))?;
        let id = NEXT_ROUTES.fetch_add(1, Ordering::Relaxed);
        *JOBS.lock().unwrap_or_else(|error| error.into_inner()) = Some((id, jobs.clone()));
        Ok(Self { id, jobs, thread })
    }

    pub(super) fn jobs(&self) -> SyncSender<Job> {
        self.jobs.clone()
    }

    /// Queues `job` without waiting; a full queue drops it.
    pub(super) fn send(&self, job: Job) -> bool {
        send(&self.jobs, job)
    }

    /// Ends the thread once its queue is empty and every other sender is
    /// gone.
    pub(super) fn stop(self, deadline: Instant) {
        let mut global = JOBS.lock().unwrap_or_else(|error| error.into_inner());
        if global.as_ref().is_some_and(|(id, _)| *id == self.id) {
            *global = None;
        }
        drop(global);
        drop(self.jobs);
        super::join_by(self.thread, deadline, "capture-route");
    }
}

pub(super) fn send(jobs: &SyncSender<Job>, job: Job) -> bool {
    match jobs.try_send(job) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            tracing::warn!("the capture router is behind; dropping a request");
            false
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// The newest routed capture, which the capsule's commands act on.
struct Routed {
    bundle: PathBuf,
    router: Router,
    role: String,
    route: CaptureRoute,
    /// The session Lody made, once it took the send.
    sent: Option<Sent>,
}

struct Sent {
    session: String,
}

/// Makes a Lody session from a capture.
type Forward = fn(&Path) -> anyhow::Result<lody::Created>;

/// Takes a Lody session back.
type Withdraw = fn(&str, Duration) -> anyhow::Result<()>;

struct Worker {
    outcomes: SharedOutcomes,
    current: Option<Routed>,
    retry_at: Instant,
    forward: Forward,
    withdraw: Withdraw,
}

impl Worker {
    fn new(outcomes: SharedOutcomes) -> Self {
        Self {
            outcomes,
            current: None,
            retry_at: Instant::now() + RETRY_EVERY,
            forward: lody::send_bundle,
            withdraw: lody::withdraw,
        }
    }

    fn run(mut self, jobs: &Receiver<Job>) {
        loop {
            match jobs.recv_timeout(self.retry_at.saturating_duration_since(Instant::now())) {
                Ok(job) => self.handle(job),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= self.retry_at {
                self.retry();
                self.retry_at = Instant::now() + RETRY_EVERY;
            }
        }
    }

    fn handle(&mut self, job: Job) {
        match job {
            Job::Written { bundle, look } => self.written(bundle, look),
            Job::Send { to, always } => {
                if always {
                    tracing::info!("there are no rules to save; sending");
                }
                self.chosen(&to);
            }
            Job::Undo => self.undo(),
            Job::SendBundle {
                bundle,
                to,
                always,
                reply,
            } => {
                let _ = reply.try_send(self.send_bundle(router(), &bundle, to.as_deref(), always));
            }
        }
    }

    /// `hex capture send`: the newest capture as the capsule's pick, any
    /// other straight to the Agent Role. There is one place a capture goes,
    /// so `to` can only name it, and there are no rules to save.
    fn send_bundle(
        &mut self,
        router: Router,
        bundle: &Path,
        to: Option<&str>,
        always: bool,
    ) -> Result<String, String> {
        if always {
            return Err("there are no rules to save; every capture is a new Lody session".into());
        }
        let role = router
            .config()
            .agent_lody
            .role()
            .ok_or("no Lody Agent Role is set (capture.routing.agentLody.agentRole)")?
            .to_owned();
        if let Some(to) = to
            && !to.trim().eq_ignore_ascii_case(&role)
        {
            return Err(format!("captures go to the Agent Role {role}, not {to}"));
        }
        if self.is_current(bundle) {
            self.choose(&role);
            let current = self.current.as_ref().ok_or("no capture")?;
            return Ok(current
                .route
                .delivery
                .as_ref()
                .map_or_else(|| "Nothing sent".into(), describe));
        }
        scrawl_capture::handoff::load_bundle(bundle)
            .map_err(|error| format!("couldn't read the capture: {error:#}"))?;
        let (delivery, _) = deliver(&router, bundle, &role, self.forward);
        tracing::info!(
            bundle = %bundle_name(bundle),
            delivery = delivery_kind(&delivery),
            "capture sent from the CLI"
        );
        match delivery {
            RouteDelivery::Failed { why, .. } => Err(why),
            delivery => Ok(describe(&delivery)),
        }
    }

    fn written(&mut self, bundle: PathBuf, look: Look) {
        let Look { router } = look;
        let Some(role) = router.config().agent_lody.role().map(str::to_owned) else {
            return;
        };
        let to = RouteDest {
            session: role.clone(),
            name: role.clone(),
            why: "agent role".into(),
        };
        tracing::info!(bundle = %bundle_name(&bundle), "capture routed");
        self.current = Some(Routed {
            bundle,
            router,
            role,
            route: CaptureRoute::of(RouteAnswer::Rule {
                to,
                suggested: Vec::new(),
                recent: Vec::new(),
                site: None,
            }),
            sent: None,
        });
        self.publish();
        self.send_current();
    }

    /// The capsule's pick: sends the newest capture now, unless it has gone
    /// or is queued already. `to` is the Agent Role.
    fn chosen(&mut self, to: &str) {
        if self.acting_on_last() {
            self.choose(to);
        }
    }

    fn choose(&mut self, to: &str) {
        let Some(current) = self.current.as_ref() else {
            return;
        };
        if !to.trim().eq_ignore_ascii_case(&current.role) {
            tracing::info!("the capsule named a destination the route doesn't list");
            return;
        }
        let pending = current.sent.is_some()
            || matches!(current.route.delivery, Some(RouteDelivery::Queued { .. }));
        if !pending {
            self.send_current();
        }
    }

    fn undo(&mut self) {
        if self.acting_on_last() && self.take_back() {
            self.publish();
        }
    }

    /// Takes back the current capture's send, whatever state it is in;
    /// `false` when there was none. One Lody wouldn't give back is too late.
    fn take_back(&mut self) -> bool {
        let withdraw = self.withdraw;
        let Some(current) = self.current.as_mut() else {
            return false;
        };
        let to = delivered_to(current.route.delivery.as_ref());
        if let Some(sent) = current.sent.take() {
            match withdraw(&sent.session, UNDO_DEADLINE) {
                Ok(()) => {
                    tracing::info!(
                        bundle = %bundle_name(&current.bundle),
                        "capture send taken back"
                    );
                    current.route.delivery = Some(RouteDelivery::TakenBack);
                }
                // Still where it went: the capsule, already showing it taken
                // back, hears that it's too late.
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "could not take a capture's send back");
                    current.sent = Some(sent);
                    current.route.delivery = Some(RouteDelivery::TooLate {
                        to: to.unwrap_or_default(),
                    });
                }
            }
            return true;
        }
        if matches!(current.route.delivery, Some(RouteDelivery::Queued { .. })) {
            // Its retries stop with the marker.
            let _ = std::fs::remove_file(current.bundle.join(delivery::PENDING));
            self.set_delivery(RouteDelivery::TakenBack);
            return true;
        }
        false
    }

    fn send_current(&mut self) {
        let Some(role) = self.current.as_ref().map(|current| current.role.clone()) else {
            return;
        };
        self.set_delivery(RouteDelivery::Sending { to: role });
        self.publish();
        let Some(current) = self.current.as_mut() else {
            return;
        };
        let (delivery, sent) = deliver(
            &current.router,
            &current.bundle,
            &current.role,
            self.forward,
        );
        tracing::info!(
            bundle = %bundle_name(&current.bundle),
            delivery = delivery_kind(&delivery),
            "capture sent"
        );
        current.sent = sent;
        current.route.delivery = Some(delivery);
        self.publish();
    }

    /// Commands act on the snapshot's newest capture only, and only when
    /// it is the one routed.
    fn acting_on_last(&self) -> bool {
        let last = lock(&self.outcomes)
            .last
            .as_ref()
            .map(|last| last.bundle.clone());
        self.current
            .as_ref()
            .is_some_and(|current| Some(&current.bundle) == last.as_ref())
    }

    fn is_current(&self, bundle: &Path) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| current.bundle == bundle)
    }

    fn set_delivery(&mut self, delivery: RouteDelivery) {
        if let Some(current) = self.current.as_mut() {
            current.route.delivery = Some(delivery);
        }
    }

    fn publish(&self) {
        if let Some(current) = &self.current {
            lock(&self.outcomes).routed(&current.bundle, current.route.clone());
        }
    }

    /// Retries sends Lody didn't take.
    fn retry(&mut self) {
        if cfg!(test) {
            return;
        }
        let captures = crate::app_settings::capture_settings().captures_dir();
        self.retry_in(&router(), &captures, now_ms());
    }

    fn retry_in(&mut self, router: &Router, captures: &Path, now: u64) {
        let report = router.run(|| delivery::retry_due(captures, now, lody::resend));
        if !report.sent.is_empty() {
            tracing::info!(captures = report.sent.len(), "retried sends went");
        }
        let sent_now = self
            .current
            .as_ref()
            .is_some_and(|current| report.sent.iter().any(|(dir, _)| *dir == current.bundle));
        if sent_now
            && let Some(current) = self.current.as_mut()
            && let Some(RouteDelivery::Queued { to, .. }) = current.route.delivery.take()
        {
            current.route.delivery = Some(RouteDelivery::Sent { to });
            self.publish();
        }
    }
}

/// A failed send (`{err:#}`, outer context first) in the few words the
/// capsule shows under "Copied".
fn send_why(error: &str) -> String {
    let fit = |line: &str| match line.char_indices().nth(60) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    };
    if error.contains("CLI wasn't found") || error.contains("isn't an executable") {
        "Lody's CLI wasn't found".into()
    } else if error.contains("not running") || error.contains("not available") {
        "Lody isn't running".into()
    } else if error.contains("didn't answer in time") {
        "Lody didn't answer".into()
    } else if error.contains("too old") {
        "Lody's CLI is too old".into()
    } else if error.contains("no Lody Agent Role") {
        "no Agent Role set".into()
    } else if error.contains("Multiple workspaces") {
        "set agentLody.workspace".into()
    } else {
        let error = delivery::short(error);
        fit(error
            .strip_prefix("lody session create failed: ")
            .unwrap_or(&error))
    }
}

/// A delivery as a sentence for `hex capture send`.
fn describe(delivery: &RouteDelivery) -> String {
    match delivery {
        RouteDelivery::Sending { to } => format!("Sending to {to}"),
        RouteDelivery::Sent { to } => format!("Sent to {to}"),
        RouteDelivery::Queued { to, why } => format!("Queued for {to}: {why}"),
        RouteDelivery::TakenBack => "Taken back".into(),
        RouteDelivery::TooLate { to } => format!("Already read in {to}"),
        RouteDelivery::Failed { to, why } => format!("Not sent to {to}: {why}"),
    }
}

fn delivered_to(delivery: Option<&RouteDelivery>) -> Option<String> {
    match delivery? {
        RouteDelivery::Sending { to }
        | RouteDelivery::Sent { to }
        | RouteDelivery::Queued { to, .. }
        | RouteDelivery::TooLate { to }
        | RouteDelivery::Failed { to, .. } => Some(to.clone()),
        RouteDelivery::TakenBack => None,
    }
}

/// Sends the capture to a new Lody session and records where it went in its
/// bundle. A send Lody didn't take is queued for [`Worker::retry`].
fn deliver(
    router: &Router,
    bundle: &Path,
    role: &str,
    forward: Forward,
) -> (RouteDelivery, Option<Sent>) {
    router.run(|| {
        let (delivery, created) =
            delivery::deliver_by(bundle, role, role, None, |dir, _| forward(dir));
        let to = role.to_owned();
        match (delivery, created) {
            (Delivery::Sent { .. }, created) => {
                let session = created.map(|created| created.session_id);
                if let Some(session) = &session {
                    record(bundle, session, role);
                }
                (
                    RouteDelivery::Sent { to },
                    session.map(|session| Sent { session }),
                )
            }
            (Delivery::Queued { error, .. }, _) => {
                tracing::warn!(
                    error = delivery::short(&error),
                    "a capture's send didn't go; retrying"
                );
                (
                    RouteDelivery::Queued {
                        to,
                        why: send_why(&error),
                    },
                    None,
                )
            }
            (Delivery::Saved, _) => (
                RouteDelivery::Failed {
                    to,
                    why: "nothing was sent".into(),
                },
                None,
            ),
        }
    })
}

/// Writes the Lody session into the bundle's `scrawl.json`, which `hex capture
/// list` reads.
fn record(bundle: &Path, session: &str, role: &str) {
    let destination = Destination {
        session: session.to_owned(),
        title: role.to_owned(),
        via: "agent-role".into(),
        url: None,
        directory: None,
    };
    let recorded = scrawl_capture::handoff::load_bundle(bundle).and_then(
        |(_, mut loaded): (PathBuf, ScrawlBundle)| {
            routing::record_destination(bundle, &mut loaded, destination)
        },
    );
    if let Err(error) = recorded {
        tracing::warn!(error = %format!("{error:#}"), "could not record a capture's destination");
    }
}

fn bundle_name(bundle: &Path) -> String {
    bundle
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn delivery_kind(delivery: &RouteDelivery) -> &'static str {
    match delivery {
        RouteDelivery::Sending { .. } => "sending",
        RouteDelivery::Sent { .. } => "sent",
        RouteDelivery::Queued { .. } => "queued",
        RouteDelivery::TakenBack => "taken-back",
        RouteDelivery::TooLate { .. } => "too-late",
        RouteDelivery::Failed { .. } => "failed",
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hex-route-{name}-{}-{}",
            std::process::id(),
            NEXT_ROUTES.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A written capture of nothing in particular, alone in a captures folder.
    fn written() -> PathBuf {
        let dir = temp("bundle").join("test.scrawl");
        std::fs::create_dir(&dir).unwrap();
        let bundle = serde_json::json!({
            "version": 1, "id": "test", "createdAtMs": 0, "durationMs": 1000,
            "target": {
                "mode": "display", "app": "Board", "displayId": 1,
                "frame": {"x": 0.0, "y": 0.0, "width": 100.0, "height": 100.0, "scale": 2.0},
            },
            "transcript": {"source": "test", "text": "this card", "segments": []},
            "strokes": [], "marks": [], "events": [], "keyframes": [], "contactSheet": "",
        });
        std::fs::write(dir.join("scrawl.json"), bundle.to_string()).unwrap();
        scrawl_capture::handoff::load_bundle(&dir).expect("a bundle");
        dir
    }

    fn router_with(config: impl FnOnce(&mut RouterConfig)) -> Router {
        let mut routing = RouterConfig::default();
        config(&mut routing);
        Router::new(routing, RouterDirs::default())
    }

    fn with_role(config: &mut RouterConfig) {
        config.agent_lody.agent_role = Some("前端".into());
    }

    /// A `lody` that prints the session it made and keeps its arguments.
    fn fake_lody(reply: &str) -> (PathBuf, PathBuf) {
        let dir = temp("lody");
        let log = dir.join("args");
        let path = dir.join("lody");
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho \"$@\" >> {}\n{reply}\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (path, log)
    }

    fn made(_: &Path) -> anyhow::Result<lody::Created> {
        Ok(lody::Created {
            session_id: "ses_made".into(),
        })
    }

    fn refused(_: &Path) -> anyhow::Result<lody::Created> {
        anyhow::bail!("Local session runtime is not available: connect ECONNREFUSED")
    }

    thread_local! {
        static WITHDRAWN: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn withdrawn(session: &str, _: Duration) -> anyhow::Result<()> {
        assert_eq!(session, "ses_made");
        WITHDRAWN.set(WITHDRAWN.get() + 1);
        Ok(())
    }

    fn kept(_: &str, _: Duration) -> anyhow::Result<()> {
        anyhow::bail!("lody session cancel failed: nope")
    }

    fn worker(outcomes: SharedOutcomes, forward: Forward, withdraw: Withdraw) -> Worker {
        Worker {
            forward,
            withdraw,
            ..Worker::new(outcomes)
        }
    }

    /// A worker that has routed a fresh capture to the role, the capture the
    /// snapshot's newest.
    fn routed(forward: Forward, withdraw: Withdraw) -> (Worker, SharedOutcomes, PathBuf) {
        let outcomes = SharedOutcomes::default();
        let bundle = written();
        lock(&outcomes).fail(bundle.clone(), "test".into());
        let mut worker = worker(outcomes.clone(), forward, withdraw);
        let look = look_with(router_with(with_role)).expect("captures are sent");
        worker.written(bundle.clone(), look);
        (worker, outcomes, bundle)
    }

    fn published(outcomes: &SharedOutcomes) -> Option<RouteDelivery> {
        lock(outcomes)
            .last
            .as_ref()
            .and_then(|last| last.route.clone())
            .and_then(|route| route.delivery)
    }

    #[test]
    fn nothing_is_sent_without_an_agent_role_or_with_autosend_off() {
        assert!(look_with(router_with(|_| {})).is_none());
        assert!(
            look_with(router_with(|config| {
                config.agent_lody.agent_role = Some("   ".into());
            }))
            .is_none()
        );
        assert!(
            look_with(router_with(|config| {
                with_role(config);
                config.send_captures = false;
            }))
            .is_none()
        );
        assert!(look_with(router_with(with_role)).is_some());
    }

    #[test]
    fn a_written_capture_goes_to_the_agent_role_at_once() {
        let (worker, outcomes, bundle) = routed(made, withdrawn);
        let current = worker.current.as_ref().unwrap();
        assert!(matches!(
            &current.route.answer,
            RouteAnswer::Rule { to, .. } if to.name == "前端"
        ));
        assert_eq!(
            published(&outcomes),
            Some(RouteDelivery::Sent {
                to: "前端".into()
            })
        );
        assert_eq!(current.sent.as_ref().unwrap().session, "ses_made");
        let (_, recorded) = scrawl_capture::handoff::load_bundle(&bundle).unwrap();
        let destination = recorded.destination.unwrap();
        assert_eq!(
            (destination.session.as_str(), destination.title.as_str()),
            ("ses_made", "前端")
        );
    }

    #[test]
    fn a_send_lody_did_not_take_is_queued_and_retried_through_the_cli() {
        let (mut worker, outcomes, bundle) = routed(refused, withdrawn);
        assert_eq!(
            published(&outcomes),
            Some(RouteDelivery::Queued {
                to: "前端".into(),
                why: "Lody isn't running".into()
            })
        );
        assert!(bundle.join(delivery::PENDING).is_file());
        assert!(worker.current.as_ref().unwrap().sent.is_none());

        let (cli, log) = fake_lody(r#"echo '{"ok":true,"sessionId":"ses_retry"}'"#);
        let router = router_with(|config| {
            with_role(config);
            config.agent_lody.cli = Some(cli.display().to_string());
        });
        let captures = bundle.parent().unwrap().to_path_buf();
        worker.retry_in(&router, &captures, now_ms() + 60_000);
        assert_eq!(
            published(&outcomes),
            Some(RouteDelivery::Sent {
                to: "前端".into()
            })
        );
        assert!(!bundle.join(delivery::PENDING).exists());
        let args = std::fs::read_to_string(log).unwrap();
        assert!(
            args.starts_with("session create --json --agent-role 前端 "),
            "{args}"
        );
    }

    #[test]
    fn undo_withdraws_the_session_and_a_refusal_is_too_late() {
        let before = WITHDRAWN.get();
        let (mut worker, outcomes, _) = routed(made, withdrawn);
        worker.undo();
        assert_eq!(WITHDRAWN.get(), before + 1);
        assert_eq!(published(&outcomes), Some(RouteDelivery::TakenBack));
        assert!(worker.current.as_ref().unwrap().sent.is_none());

        let (mut worker, outcomes, _) = routed(made, kept);
        worker.undo();
        assert_eq!(
            published(&outcomes),
            Some(RouteDelivery::TooLate {
                to: "前端".into()
            })
        );
        assert!(worker.current.unwrap().sent.is_some(), "it's still sent");
    }

    #[test]
    fn undoing_a_queued_send_stops_its_retries() {
        let (mut worker, outcomes, bundle) = routed(refused, withdrawn);
        worker.undo();
        assert_eq!(published(&outcomes), Some(RouteDelivery::TakenBack));
        assert!(!bundle.join(delivery::PENDING).exists());
    }

    #[test]
    fn a_taken_back_capture_is_sent_again_when_the_role_is_picked() {
        let (mut worker, outcomes, _) = routed(made, withdrawn);
        worker.undo();
        worker.chosen("somewhere else");
        assert_eq!(published(&outcomes), Some(RouteDelivery::TakenBack));
        worker.chosen("前端");
        assert_eq!(
            published(&outcomes),
            Some(RouteDelivery::Sent {
                to: "前端".into()
            })
        );
        let sent = worker.current.as_ref().unwrap().sent.as_ref();
        assert_eq!(sent.map(|sent| sent.session.as_str()), Some("ses_made"));
        // A second click on what already went sends nothing more.
        let before = WITHDRAWN.get();
        worker.chosen("前端");
        assert_eq!(WITHDRAWN.get(), before);
    }

    #[test]
    fn the_cli_sends_to_the_agent_role_only_and_saves_no_rules() {
        let outcomes = SharedOutcomes::default();
        let mut worker = worker(outcomes, made, withdrawn);
        let bundle = written();
        let send = |worker: &mut Worker, router: Router, to: Option<&str>, always: bool| {
            worker.send_bundle(router, &bundle, to, always)
        };
        assert!(
            send(&mut worker, router_with(with_role), None, true)
                .unwrap_err()
                .contains("no rules")
        );
        assert!(
            send(&mut worker, router_with(|_| {}), None, false)
                .unwrap_err()
                .contains("no Lody Agent Role")
        );
        assert!(
            send(&mut worker, router_with(with_role), Some("后端"), false)
                .unwrap_err()
                .contains("not 后端")
        );
        assert_eq!(
            send(&mut worker, router_with(with_role), Some("前端"), false),
            Ok("Sent to 前端".into())
        );
        let (_, recorded) = scrawl_capture::handoff::load_bundle(&bundle).unwrap();
        assert_eq!(recorded.destination.unwrap().session, "ses_made");

        let mut worker = worker_failing();
        assert_eq!(
            send(&mut worker, router_with(with_role), None, false),
            Ok("Queued for 前端: Lody isn't running".into())
        );
    }

    fn worker_failing() -> Worker {
        worker(SharedOutcomes::default(), refused, withdrawn)
    }

    #[test]
    fn the_real_send_runs_lody_with_the_role_and_the_summary() {
        let (cli, log) = fake_lody(r#"echo '{"ok":true,"sessionId":"ses_real"}'"#);
        let outcomes = SharedOutcomes::default();
        let bundle = written();
        lock(&outcomes).fail(bundle.clone(), "test".into());
        let mut worker = Worker::new(outcomes.clone());
        let router = router_with(|config| {
            with_role(config);
            config.agent_lody.local_project = Some("~/code/hex".into());
            config.agent_lody.cli = Some(cli.display().to_string());
        });
        worker.written(bundle.clone(), look_with(router).unwrap());
        assert_eq!(
            published(&outcomes),
            Some(RouteDelivery::Sent {
                to: "前端".into()
            })
        );
        let args = std::fs::read_to_string(log).unwrap();
        assert!(
            args.starts_with(
                "session create --json --agent-role 前端 --local-project ~/code/hex --prompt-file "
            ),
            "{args}"
        );
        assert!(
            !args.contains("--attach"),
            "a capture with no screen has no sheet: {args}"
        );
        let (_, recorded) = scrawl_capture::handoff::load_bundle(&bundle).unwrap();
        assert_eq!(recorded.destination.unwrap().session, "ses_real");
    }

    #[test]
    fn a_failed_send_says_what_went_wrong() {
        for (error, said) in [
            (
                "Lody's CLI wasn't found (not on PATH, not in Lody.app)",
                "Lody's CLI wasn't found",
            ),
            (
                "the configured Lody CLI /x isn't an executable file",
                "Lody's CLI wasn't found",
            ),
            (
                "lody session create failed: Local CLI daemon is not running. Run `npx lody start` first.",
                "Lody isn't running",
            ),
            (
                "Local session runtime is not available: boom",
                "Lody isn't running",
            ),
            (
                "Lody's CLI didn't answer in time (60 s)",
                "Lody didn't answer",
            ),
            (
                "Lody's CLI from PATH is too old for session create: unknown option",
                "Lody's CLI is too old",
            ),
            (
                "no Lody Agent Role is set (capture.routing.agentLody.agentRole)",
                "no Agent Role set",
            ),
            (
                "lody session create failed: Agent Role not found: x",
                "Agent Role not found: x",
            ),
            (
                "lody session create failed: Multiple workspaces are available; pass --workspace.",
                "set agentLody.workspace",
            ),
        ] {
            assert_eq!(send_why(error), said, "{error}");
        }
        let long = format!("lody session create failed: {}", "x".repeat(200));
        assert!(send_why(&long).chars().count() <= 61);
    }
}
