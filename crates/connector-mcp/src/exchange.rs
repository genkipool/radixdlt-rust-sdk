//! One road to the wallet for every request, and the tools that look after what is on it.
//!
//! Every tool that asks the phone for something goes through [`interact`], which:
//!
//! 1. refuses to send while an earlier request may still be waiting in the wallet's queue
//!    (`PENDING_IN_WALLET`) — the wallet shows one request at a time and nothing withdraws one,
//!    so a second request only stacks behind a first that never showed;
//! 2. checks the dApp identity the way the wallet will, because some mismatches make the wallet
//!    DROP the request without answering;
//! 3. records the request (`requests.json`) and logs every step (`connector.log`);
//! 4. sends it, noting when the wallet confirms receipt — the line between «safe to resend» and
//!    «it is on the phone»;
//! 5. picks up late answers to EARLIER requests that arrive on the same channel, and says so;
//! 6. can be stopped by `cancel_request` or by the client cancelling the call.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use radixdlt_connect::{check_failure, ConnectError, Connector, DappContext, Progress};
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::diag::{iso_secs, now_secs, Failure};
use crate::gateway;
use crate::outbox::{Outbox, Record, State};
use crate::requests::describe_answer;
use crate::rpc::{App, InFlight};
use crate::store::Store;
use crate::tools::{
    clamp_timeout, opt_bool, opt_str, opt_u64, req_network, resolve_dapp_definition, resolve_origin, Network,
    ToolResult,
};

/// A verified dApp identity is not checked again for this long.
const DAPP_CHECK_TTL: Duration = Duration::from_secs(600);

/// Who a request goes to and as which dApp, resolved from a tool's arguments.
pub struct Target {
    pub network: Network,
    pub ctx: DappContext,
    pub password: Vec<u8>,
    /// Public key of the paired wallet.
    pub wallet: String,
    pub timeout: Duration,
}

impl Target {
    pub fn dapp_definition(&self) -> &str {
        &self.ctx.dapp_definition
    }
    pub fn origin(&self) -> &str {
        &self.ctx.origin
    }
}

/// Resolves the wallet (`wallet_public_key`, else the first paired one) and its link password.
pub fn wallet_link(app: &App, wallet_public_key: Option<&str>) -> Result<(String, Vec<u8>), Failure> {
    let not_paired = || {
        Failure::new(
            "NOT_PAIRED",
            "input",
            true,
            "no paired wallet",
            "Pair one first: pair_wallet, show the QR, pair_status (needed once per device).",
        )
    };
    let state = Store::load(app.config_path()).map_err(|_| not_paired())?;
    let wallet = match wallet_public_key {
        Some(pk) => pk.to_string(),
        None => state
            .all_links()
            .first()
            .map(|l| l.wallet_public_key.clone())
            .ok_or_else(not_paired)?,
    };
    let password = state.password_bytes_for(&wallet).map_err(|e| {
        Failure::new(
            "UNKNOWN_WALLET",
            "input",
            true,
            e.to_string(),
            "No paired wallet has that public key: see list_wallets.",
        )
    })?;
    Ok((wallet, password))
}

/// Everything a wallet request needs from its arguments.
pub fn target(app: &App, args: &Value, default_timeout: u64) -> Result<Target, Failure> {
    let network = req_network(args).map_err(Failure::input)?;
    let (wallet, password) = wallet_link(app, opt_str(args, "wallet_public_key").as_deref())?;
    let ctx = DappContext::new(
        network.id(),
        resolve_dapp_definition(args, network),
        resolve_origin(args),
    );
    let timeout = Duration::from_secs(clamp_timeout(
        opt_u64(args, "timeout_seconds").unwrap_or(default_timeout),
    ));
    Ok(Target {
        network,
        ctx,
        password,
        wallet,
        timeout,
    })
}

/// What [`interact`] says beyond the answer itself.
pub struct Reply {
    pub interaction_id: String,
    /// Things the agent must know: late answers to earlier requests, checks that could not run.
    pub notes: Vec<String>,
}

impl Reply {
    /// Appends the notes to a result's text.
    pub fn annotate(&self, mut result: ToolResult) -> ToolResult {
        if !self.notes.is_empty() {
            result.push_text(format!("\n\nNOTE:\n- {}", self.notes.join("\n- ")));
        }
        result
    }
}

/// A failure as a tool result, with the notes gathered on the way.
pub fn failed(app: &App, tool: &str, failure: &Failure, reply: Option<&Reply>) -> ToolResult {
    let result = ToolResult::failure(failure.to_text(tool, app.log.path()), failure.to_json());
    match reply {
        Some(reply) => reply.annotate(result),
        None => result,
    }
}

/// A request to send: what it is, for the record and for the person.
pub struct Ask<'a> {
    pub tool: &'a str,
    pub kind: &'a str,
    pub summary: String,
    pub interaction: Value,
}

/// Sends one request to the wallet and returns its answer — see the module docs for each step.
/// A wallet `failure` answer comes back as `Err` with the wallet's error mapped.
pub async fn interact(
    app: &Rc<App>,
    args: &Value,
    target: &Target,
    ask: Ask<'_>,
) -> (Reply, Result<Value, Failure>) {
    let id = ask
        .interaction
        .get("interactionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut reply = Reply {
        interaction_id: id.clone(),
        notes: Vec::new(),
    };

    if !opt_bool(args, "ignore_pending").unwrap_or(false) {
        let open = app.outbox.open(Some(&target.wallet));
        if !open.is_empty() {
            app.log.event(
                "refused_pending",
                json!({ "tool": ask.tool, "interaction_id": id, "pending": open.iter().map(|r| &r.interaction_id).collect::<Vec<_>>() }),
            );
            return (reply, Err(pending_failure(&open)));
        }
    }

    if !opt_bool(args, "skip_dapp_check").unwrap_or(false) {
        if let Err(failure) = preflight(app, target, &mut reply.notes).await {
            app.log.event(
                "refused_dapp_identity",
                json!({ "tool": ask.tool, "interaction_id": id, "detail": failure.detail }),
            );
            return (reply, Err(failure.with_interaction(&id)));
        }
    }

    let now = now_secs();
    let record = Record {
        interaction_id: id.clone(),
        tool: ask.tool.to_string(),
        kind: ask.kind.to_string(),
        network: target.network.label().to_string(),
        wallet: target.wallet.clone(),
        summary: ask.summary.clone(),
        created_at: now,
        updated_at: now,
        state: State::Sending,
        outcome: None,
        code: None,
        pid: std::process::id(),
    };
    if let Err(e) = app.outbox.insert(record) {
        // Losing the record must not stop the request: it only weakens the flood guard.
        reply.notes.push(format!(
            "could not record the request in {}: {e}",
            app.outbox.path().display()
        ));
    }
    app.log.event(
        "request_start",
        json!({
            "tool": ask.tool, "kind": ask.kind, "interaction_id": id, "network": target.network.label(),
            "wallet": short(&target.wallet), "dapp_definition": target.dapp_definition(), "origin": target.origin(),
            "timeout_s": target.timeout.as_secs(), "summary": ask.summary,
        }),
    );

    let result = run(app, target, &id, Some(&ask.interaction), &mut reply.notes).await;
    (reply, result)
}

/// Waits for the answer to a request sent earlier, sending nothing.
pub async fn collect(
    app: &Rc<App>,
    target: &Target,
    interaction_id: &str,
) -> (Reply, Result<Value, Failure>) {
    let mut reply = Reply {
        interaction_id: interaction_id.to_string(),
        notes: Vec::new(),
    };
    app.log.event(
        "await_start",
        json!({ "interaction_id": interaction_id, "wallet": short(&target.wallet), "timeout_s": target.timeout.as_secs() }),
    );
    let result = run(app, target, interaction_id, None, &mut reply.notes).await;
    (reply, result)
}

/// Marks a request's fate if its call is dropped half-way (client cancellation, process exit).
struct Unfinished<'a> {
    app: &'a App,
    id: &'a str,
    delivered: Rc<Cell<bool>>,
    sending: bool,
    done: bool,
}

impl Drop for Unfinished<'_> {
    fn drop(&mut self) {
        self.app.inflight.borrow_mut().remove(self.id);
        if self.done {
            return;
        }
        let delivered = self.delivered.get();
        self.app.log.event(
            "request_abandoned",
            json!({ "interaction_id": self.id, "delivered": delivered }),
        );
        // Delivered stays delivered: it is on the phone whatever happened here.
        if self.sending && !delivered {
            let _ = self.app.outbox.set(
                self.id,
                State::NotDelivered,
                Some("abandoned before the wallet received it".into()),
                Some("CANCELLED"),
            );
        }
    }
}

/// Waits for the wallet to let go of a channel ANOTHER connector process closed on this link
/// moments ago (this process's own are handled by the library).
pub async fn settle_across_processes(app: &App, wallet: &str, id: &str) {
    let Some(closed) = app.outbox.link_closed_at(wallet) else {
        return;
    };
    let settle = radixdlt_connect::WALLET_SETTLE.as_millis() as u64;
    let wait = (closed + settle).saturating_sub(crate::outbox::now_millis());
    if wait > 0 && wait <= settle {
        app.log.event(
            "settling",
            json!({ "interaction_id": id, "wait_ms": wait, "across_processes": true }),
        );
        tokio::time::sleep(Duration::from_millis(wait)).await;
    }
}

/// Records, when dropped, that this process just closed a channel to the wallet.
struct MarksLink<'a>(&'a App, &'a str);

impl Drop for MarksLink<'_> {
    fn drop(&mut self) {
        let _ = self.0.outbox.mark_link_closed(self.1);
    }
}

async fn run(
    app: &Rc<App>,
    target: &Target,
    id: &str,
    interaction: Option<&Value>,
    notes: &mut Vec<String>,
) -> Result<Value, Failure> {
    settle_across_processes(app, &target.wallet, id).await;
    let _marks = MarksLink(app, &target.wallet);
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    app.inflight.borrow_mut().insert(
        id.to_string(),
        InFlight {
            wallet: target.wallet.clone(),
            stop: stop_tx,
        },
    );
    let delivered = Rc::new(Cell::new(interaction.is_none()));
    let mut unfinished = Unfinished {
        app,
        id,
        delivered: delivered.clone(),
        sending: interaction.is_some(),
        done: false,
    };

    // The library reports progress through a `Send` callback; the bookkeeping here is not
    // `Send`, so the steps cross over a channel and are handled as they arrive.
    let (step_tx, mut step_rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
    let mut on = move |step: Progress| {
        let _ = step_tx.send(step);
    };
    let connector = Connector::new();
    let started = Instant::now();
    let outcome = {
        let exchange = async {
            match interaction {
                Some(interaction) => {
                    connector
                        .exchange(&target.password, interaction, target.timeout, &mut on)
                        .await
                }
                None => {
                    connector
                        .await_response(&target.password, id, target.timeout, &mut on)
                        .await
                }
            }
        };
        tokio::pin!(exchange);
        loop {
            tokio::select! {
                result = &mut exchange => break Some(result),
                Some(step) = step_rx.recv() => on_step(app, id, step, &delivered, notes),
                _ = &mut stop_rx => break None,
            }
        }
    };
    while let Ok(step) = step_rx.try_recv() {
        on_step(app, id, step, &delivered, notes);
    }
    unfinished.done = true;
    let delivered = delivered.get();
    let elapsed_ms = started.elapsed().as_millis() as u64;

    let Some(result) = outcome else {
        app.log.event(
            "request_cancelled",
            json!({ "interaction_id": id, "delivered": delivered }),
        );
        return Err(cancelled_failure(delivered).with_interaction(id));
    };

    match result {
        Ok(response) => match check_failure(&response) {
            Ok(()) => {
                let outcome = describe_answer(&response);
                let _ = app.outbox.set(id, State::Answered, Some(outcome.clone()), None);
                app.log.event(
                    "answered",
                    json!({ "interaction_id": id, "outcome": outcome, "elapsed_ms": elapsed_ms }),
                );
                Ok(response)
            }
            Err(wallet) => {
                let failure = Failure::from_wallet(&wallet_detail(&wallet)).with_interaction(id);
                let _ = app.outbox.set(
                    id,
                    State::Answered,
                    Some(format!("FAILURE — {}", failure.detail)),
                    Some(failure.code),
                );
                app.log.event(
                    "wallet_failure",
                    json!({ "interaction_id": id, "code": failure.code, "detail": failure.detail, "elapsed_ms": elapsed_ms }),
                );
                Err(failure)
            }
        },
        Err(error) => {
            let failure = Failure::from_connect(&error, delivered).with_interaction(id);
            let state = match (&error, delivered) {
                (ConnectError::WalletRejected(_), _) => State::Answered,
                // Every attempt went unconfirmed: the wallet most likely never got it, and
                // blocking new requests for it would only make a missing phone look like a queue.
                (ConnectError::ConfirmationTimeout, _) => State::NotDelivered,
                (_, true) => State::Delivered,
                (_, false) => State::NotDelivered,
            };
            // An await that times out leaves the request as it was: still waiting on the phone.
            if interaction.is_some() || state == State::Answered {
                let _ = app
                    .outbox
                    .set(id, state, Some(failure.detail.clone()), Some(failure.code));
            }
            app.log.event(
                "request_failed",
                json!({
                    "interaction_id": id, "code": failure.code, "stage": failure.stage, "delivered": delivered,
                    "detail": failure.detail, "elapsed_ms": elapsed_ms,
                }),
            );
            Err(failure)
        }
    }
}

/// The wallet's own words, without the localized «the wallet rejected the request:» prefix.
fn wallet_detail(error: &radixdlt_connect::WalletInteractionError) -> String {
    match error {
        radixdlt_connect::WalletInteractionError::WalletRejected(detail) => detail.clone(),
        other => other.to_string(),
    }
}

fn on_step(app: &App, id: &str, step: Progress, delivered: &Cell<bool>, notes: &mut Vec<String>) {
    match step {
        Progress::TurnTaken { waited } => {
            app.log.event(
                "turn_taken",
                json!({ "interaction_id": id, "waited_ms": waited.as_millis() as u64 }),
            );
        }
        Progress::ChannelOpen { elapsed } => {
            app.log.event(
                "channel_open",
                json!({ "interaction_id": id, "elapsed_ms": elapsed.as_millis() as u64 }),
            );
        }
        Progress::Delivered { elapsed } => {
            delivered.set(true);
            let _ = app.outbox.set(id, State::Delivered, None, None);
            app.log.event(
                "delivered",
                json!({ "interaction_id": id, "elapsed_ms": elapsed.as_millis() as u64 }),
            );
        }
        Progress::OtherMessage(message) => late_answer(&app.outbox, app, id, &message, notes),
        Progress::Settling { wait } => {
            app.log.event(
                "settling",
                json!({ "interaction_id": id, "wait_ms": wait.as_millis() as u64 }),
            );
        }
        Progress::Resending { attempt, reason } => {
            app.log.event(
                "resending",
                json!({ "interaction_id": id, "attempt": attempt, "reason": reason }),
            );
            notes.push(format!(
                "attempt {prev} did not reach the wallet ({reason}); it was sent again on a fresh channel (attempt {attempt}, same interaction id).",
                prev = attempt - 1
            ));
        }
        Progress::Reconnecting { reason } => {
            app.log
                .event("reconnecting", json!({ "interaction_id": id, "reason": reason }));
            notes.push(format!(
                "the channel to the wallet was lost after delivery ({reason}); a new one was opened to keep waiting for the answer — nothing was sent twice."
            ));
        }
        _ => {}
    }
}

/// An answer to an EARLIER request, arrived on this request's channel.
fn late_answer(outbox: &Outbox, app: &App, current: &str, message: &Value, notes: &mut Vec<String>) {
    let other = message
        .get("interactionId")
        .and_then(Value::as_str)
        .unwrap_or("(no id)")
        .to_string();
    let outcome = describe_answer(message);
    app.log.event(
        "late_answer",
        json!({ "interaction_id": other, "while": current, "outcome": outcome }),
    );
    match outbox.get(&other) {
        Some(record) => {
            let _ = outbox.set(&other, State::Answered, Some(outcome.clone()), None);
            notes.push(format!(
                "LATE ANSWER to an earlier request: {tool} ({kind}, sent {when}, interaction {other}) — {outcome}. \
                 It was answered on the phone after its call had ended{warn}.",
                tool = record.tool,
                kind = record.kind,
                when = iso_secs(record.created_at),
                warn = if outcome.contains("SUBMITTED") {
                    ": that transaction WAS submitted — check it with transaction_status and do not send it again"
                } else {
                    ""
                },
            ));
        }
        None => notes.push(format!(
            "The wallet also answered a request this connector has no record of ({other}): {outcome}."
        )),
    }
}

fn cancelled_failure(delivered: bool) -> Failure {
    if delivered {
        Failure::new(
            "CANCELLED",
            "awaiting_approval",
            false,
            "cancelled here after the wallet had received it",
            "The request is STILL on the phone (the wallet offers no way to withdraw it). Ask the person to reject it in the Radix Wallet — or, if it never shows, to force-close and reopen the app. If they approve it anyway it takes effect.",
        )
    } else {
        Failure::new(
            "CANCELLED",
            "connect",
            true,
            "cancelled before the wallet received it",
            "Nothing reached the wallet.",
        )
    }
}

fn pending_failure(open: &[Record]) -> Failure {
    let list = open
        .iter()
        .map(|r| {
            format!(
                "{id} — {tool} ({kind}, {state}, sent {age} ago): {summary}",
                id = r.interaction_id,
                tool = r.tool,
                kind = r.kind,
                state = r.state.label(),
                age = ago(r.created_at),
                summary = r.summary,
            )
        })
        .collect::<Vec<_>>()
        .join("\n             ");
    Failure::new(
        "PENDING_IN_WALLET",
        "queue",
        true,
        format!("not sent: the wallet may still hold an earlier request:\n             {list}"),
        "The wallet shows one request at a time; sending another would queue it behind this one (that is how a queue floods). Ask the person to open the Radix Wallet and approve or reject what is shown, then collect it with await_response {interaction_id}. If nothing shows on the phone, the wallet's queue is stuck: force-close the app and reopen it (that empties it), then clear it here with cancel_request {interaction_id}. Only pass ignore_pending: true when you are sure nothing is waiting.",
    )
}

/// The wallet's dApp verification, run here first (and remembered for a while once it passes).
async fn preflight(app: &App, target: &Target, notes: &mut Vec<String>) -> Result<(), Failure> {
    let key = (
        target.network.id(),
        target.dapp_definition().to_string(),
        target.origin().to_string(),
    );
    if app
        .dapp_checks
        .borrow()
        .get(&key)
        .is_some_and(|at| at.elapsed() < DAPP_CHECK_TTL)
    {
        return Ok(());
    }
    let check = gateway::check_dapp_identity(target.network, target.dapp_definition(), target.origin()).await;
    for unchecked in &check.unchecked {
        notes.push(format!("dApp identity not fully checked: {unchecked}"));
    }
    if check.ok() {
        if check.unchecked.is_empty() {
            app.dapp_checks.borrow_mut().insert(key, Instant::now());
        }
        return Ok(());
    }
    Err(Failure::new(
        "DAPP_NOT_VERIFIED",
        "preflight",
        true,
        format!("not sent — the wallet would refuse or silently drop it:\n- {}", check.problems.join("\n- ")),
        "Fix the dApp identity (pass the right dapp_definition + origin for this network; get them from get_known_addresses), or pass skip_dapp_check: true if the wallet has developer mode on. Nothing was sent.",
    ))
}

/* ──────────────────────────────── the tools ─────────────────────────────── */

pub fn pending_requests(app: &App, args: &Value) -> ToolResult {
    let wallet = opt_str(args, "wallet_public_key");
    let open = app.outbox.open(wallet.as_deref());
    let mut out = String::new();
    if open.is_empty() {
        out.push_str("NO PENDING REQUESTS — nothing this connector sent is waiting in the wallet.\n");
    } else {
        out.push_str(&format!(
            "PENDING REQUESTS: {n} (may be waiting in the wallet — sending more would queue behind them)\n",
            n = open.len()
        ));
        for r in &open {
            out.push_str(&record_line(r));
        }
        out.push_str(
            "\nWhat to do: ask the person to open the Radix Wallet and answer what it shows, then await_response {interaction_id}. \
             If nothing shows, the queue is stuck: force-close and reopen the app, then cancel_request {interaction_id}.\n",
        );
    }
    if opt_bool(args, "history").unwrap_or(false) {
        let limit = opt_u64(args, "limit").unwrap_or(10).clamp(1, 200) as usize;
        let all = app.outbox.all();
        let skip = all.len().saturating_sub(limit);
        out.push_str(&format!("\nRECENT REQUESTS (last {}):\n", all.len() - skip));
        for r in all.iter().skip(skip).rev() {
            out.push_str(&record_line(r));
        }
    }
    out.push_str(&format!(
        "\n(A delivered request stops blocking after {} min — override with RADIX_CONNECTOR_PENDING_TTL_SECONDS. Record: {})",
        Outbox::pending_ttl() / 60,
        app.outbox.path().display()
    ));
    ToolResult::text(out)
}

fn record_line(r: &Record) -> String {
    format!(
        "- {id}\n    {tool} · {kind} · {net} · {state} · sent {at} ({age} ago)\n    {summary}{outcome}\n",
        id = r.interaction_id,
        tool = r.tool,
        kind = r.kind,
        net = r.network,
        state = r.state.label(),
        at = iso_secs(r.created_at),
        age = ago(r.created_at),
        summary = r.summary,
        outcome = r
            .outcome
            .as_ref()
            .map(|o| format!("\n    → {o}"))
            .unwrap_or_default(),
    )
}

pub fn cancel_request(app: &App, args: &Value) -> ToolResult {
    let wallet = opt_str(args, "wallet_public_key");
    let targets: Vec<Record> = match opt_str(args, "interaction_id") {
        Some(id) => match app.outbox.get(&id) {
            Some(r) => vec![r],
            None if app.inflight.borrow().contains_key(&id) => vec![],
            None => {
                return failed(
                    app,
                    "cancel_request",
                    &Failure::input(format!(
                        "no request {id} in this connector's record (see pending_requests)"
                    )),
                    None,
                )
            }
        },
        None if opt_bool(args, "all").unwrap_or(false) => app.outbox.open(wallet.as_deref()),
        None => {
            return failed(
                app,
                "cancel_request",
                &Failure::input("pass interaction_id, or all: true to clear every pending request"),
                None,
            )
        }
    };

    let mut out = String::from("CANCEL\n");
    let mut stopped_ids = Vec::new();
    if let Some(id) = opt_str(args, "interaction_id") {
        if let Some(inflight) = app.inflight.borrow_mut().remove(&id) {
            let _ = inflight.stop.send(());
            stopped_ids.push(id);
        }
    } else {
        let mut inflight = app.inflight.borrow_mut();
        let ids: Vec<String> = inflight
            .iter()
            .filter(|(_, f)| wallet.as_deref().is_none_or(|w| f.wallet == w))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(f) = inflight.remove(&id) {
                let _ = f.stop.send(());
                stopped_ids.push(id);
            }
        }
    }

    if targets.is_empty() && stopped_ids.is_empty() {
        out.push_str("Nothing to cancel: no pending requests.\n");
        return ToolResult::text(out);
    }
    for r in &targets {
        let was = r.state;
        if was == State::Answered {
            out.push_str(&format!(
                "- {id}: already answered ({outcome}) — nothing to cancel.\n",
                id = r.interaction_id,
                outcome = r.outcome.as_deref().unwrap_or("answered")
            ));
            continue;
        }
        let _ = app
            .outbox
            .set(&r.interaction_id, State::Cancelled, None, Some("CANCELLED"));
        app.log.event(
            "cancelled",
            json!({ "interaction_id": r.interaction_id, "was": was.label() }),
        );
        let on_phone = matches!(was, State::Delivered | State::Unconfirmed)
            || (was == State::Sending && stopped_ids.contains(&r.interaction_id));
        out.push_str(&format!(
            "- {id} ({tool}, {kind}): {verdict}\n",
            id = r.interaction_id,
            tool = r.tool,
            kind = r.kind,
            verdict = if on_phone {
                "cleared HERE, but it may still be on the phone — the wallet offers no remote cancel. Ask the person to REJECT it in the Radix Wallet (or force-close and reopen the app if it never shows). If they approve it anyway, it takes effect; the connector reports such late answers."
            } else {
                "cleared; it had not reached the wallet."
            }
        ));
    }
    for id in stopped_ids
        .iter()
        .filter(|id| !targets.iter().any(|r| &r.interaction_id == *id))
    {
        out.push_str(&format!("- {id}: the call waiting for it was stopped.\n"));
    }
    out.push_str("\nNew requests can be sent now.");
    ToolResult::text(out)
}

pub async fn await_response(app: &Rc<App>, args: &Value) -> ToolResult {
    let wallet_arg = opt_str(args, "wallet_public_key");
    let record = match opt_str(args, "interaction_id") {
        Some(id) => app.outbox.get(&id),
        None => app.outbox.open(wallet_arg.as_deref()).into_iter().next(),
    };
    let Some(record) = record else {
        return failed(
            app,
            "await_response",
            &Failure::input(
                "no such request — pass the interaction_id of a pending request (see pending_requests)",
            ),
            None,
        );
    };
    if record.state == State::Answered {
        return ToolResult::text(format!(
            "ALREADY ANSWERED\nInteraction: {id}\n{tool} ({kind}), sent {at}\nAnswer: {outcome}",
            id = record.interaction_id,
            tool = record.tool,
            kind = record.kind,
            at = iso_secs(record.created_at),
            outcome = record.outcome.as_deref().unwrap_or("answered"),
        ));
    }
    let wallet = wallet_arg.unwrap_or_else(|| record.wallet.clone());
    let (wallet, password) = match wallet_link(app, Some(&wallet)) {
        Ok(link) => link,
        Err(f) => return failed(app, "await_response", &f, None),
    };
    let network = Network::parse(&record.network).unwrap_or(Network::Stokenet);
    let target = Target {
        network,
        ctx: DappContext::new(network.id(), String::new(), String::new()),
        password,
        wallet,
        timeout: Duration::from_secs(clamp_timeout(opt_u64(args, "timeout_seconds").unwrap_or(120))),
    };
    let (reply, result) = collect(app, &target, &record.interaction_id).await;
    match result {
        Ok(response) => {
            let text = format!(
                "ANSWER RECEIVED ✓\nInteraction: {id}\n{tool} ({kind}), sent {at}\nAnswer: {outcome}\n\n{body}\
                 Proofs in it are not re-verified here; check them against the challenge you sent.\n\n\
                 Raw answer:\n```json\n{raw}\n```",
                id = record.interaction_id,
                tool = record.tool,
                kind = record.kind,
                at = iso_secs(record.created_at),
                outcome = describe_answer(&response),
                body = crate::requests::render_answer(&response, None),
                raw = serde_json::to_string_pretty(&response).unwrap_or_default(),
            );
            reply.annotate(ToolResult::text(text))
        }
        Err(failure) => failed(app, "await_response", &failure, Some(&reply)),
    }
}

pub async fn check_wallet_connection(app: &Rc<App>, args: &Value) -> ToolResult {
    let (wallet, password) = match wallet_link(app, opt_str(args, "wallet_public_key").as_deref()) {
        Ok(link) => link,
        Err(f) => return failed(app, "check_wallet_connection", &f, None),
    };
    let timeout = Duration::from_secs(clamp_timeout(opt_u64(args, "timeout_seconds").unwrap_or(30)));
    app.log.event(
        "probe_start",
        json!({ "wallet": short(&wallet), "timeout_s": timeout.as_secs() }),
    );
    let open = app.outbox.open(Some(&wallet));
    let pending = if open.is_empty() {
        "none".to_string()
    } else {
        format!("{} (see pending_requests)", open.len())
    };
    settle_across_processes(app, &wallet, "").await;
    let probed = Connector::new().probe(&password, timeout).await;
    let _ = app.outbox.mark_link_closed(&wallet);
    match probed {
        Ok(took) => {
            app.log.event(
                "probe_ok",
                json!({ "wallet": short(&wallet), "elapsed_ms": took.as_millis() as u64 }),
            );
            ToolResult::text(format!(
                "WALLET REACHABLE ✓\nWallet:   {wallet}\nChannel:  opened in {ms} ms (nothing was shown on the phone)\nPending:  {pending}",
                ms = took.as_millis(),
            ))
        }
        Err(e) => {
            let failure = Failure::from_connect(&e, false);
            app.log.event(
                "probe_failed",
                json!({ "wallet": short(&wallet), "code": failure.code, "detail": failure.detail }),
            );
            let mut result = failed(app, "check_wallet_connection", &failure, None);
            result.push_text(format!("\n\nPending requests: {pending}"));
            result
        }
    }
}

pub async fn check_dapp_identity(args: &Value) -> ToolResult {
    let network = match req_network(args) {
        Ok(n) => n,
        Err(e) => return ToolResult::error(e),
    };
    let dapp = resolve_dapp_definition(args, network);
    let origin = resolve_origin(args);
    let check = gateway::check_dapp_identity(network, &dapp, &origin).await;
    let mut out = format!(
        "DAPP IDENTITY {verdict} (network: {net})\ndapp_definition: {dapp}\norigin:          {origin}\n",
        verdict = if check.ok() { "OK ✓" } else { "PROBLEMS ✗" },
        net = network.label(),
        dapp = if dapp.is_empty() { "(none)" } else { &dapp },
    );
    for p in &check.problems {
        out.push_str(&format!("✗ {p}\n"));
    }
    for u in &check.unchecked {
        out.push_str(&format!("? {u}\n"));
    }
    if check.ok() {
        out.push_str(
            "The wallet will show requests from this dApp as verified (unless it is on another network).",
        );
        ToolResult::text(out)
    } else {
        ToolResult::error(out)
    }
}

pub fn connector_log(app: &App, args: &Value) -> ToolResult {
    let limit = opt_u64(args, "limit").unwrap_or(40).clamp(1, 500) as usize;
    let id = opt_str(args, "interaction_id");
    let events = app.log.tail(limit, id.as_deref());
    if events.is_empty() {
        return ToolResult::text(format!(
            "No log events{}. Log file: {}",
            id.map(|i| format!(" for {i}")).unwrap_or_default(),
            app.log.path().display()
        ));
    }
    let mut out = format!(
        "CONNECTOR LOG — {} event(s), oldest first ({})\n",
        events.len(),
        app.log.path().display()
    );
    for event in &events {
        let mut fields = event.as_object().cloned().unwrap_or_default();
        let ts = fields
            .remove("ts")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let name = fields
            .remove("event")
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let rest = fields
            .iter()
            .map(|(k, v)| match v {
                Value::String(s) => format!("{k}={s}"),
                other => format!("{k}={other}"),
            })
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("{ts} {name} {rest}\n"));
    }
    ToolResult::text(out)
}

/* ──────────────────────────────── helpers ──────────────────────────────── */

/// A wallet public key, shortened for the log.
pub fn short(key: &str) -> String {
    if key.len() > 12 {
        format!("{}…", &key[..12])
    } else {
        key.to_string()
    }
}

/// «3 min», «45 s», «2 h».
fn ago(then: u64) -> String {
    let secs = now_secs().saturating_sub(then);
    match secs {
        s if s < 90 => format!("{s} s"),
        s if s < 5_400 => format!("{} min", s / 60),
        s => format!("{} h", s / 3_600),
    }
}

/// One line saying what a manifest does: its instructions and the methods it calls.
pub fn manifest_summary(manifest: &str, message: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for instruction in manifest.split(';') {
        let mut words = instruction.split_whitespace();
        let Some(op) = words.next() else { continue };
        if !op.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            continue;
        }
        let quoted: Vec<&str> = instruction.split('"').skip(1).step_by(2).collect();
        let part = match op {
            "CALL_METHOD" => quoted.get(1).map(|m| format!("{op} {m}")),
            "CALL_FUNCTION" => match (quoted.get(1), quoted.get(2)) {
                (Some(b), Some(f)) => Some(format!("{op} {b}::{f}")),
                _ => None,
            },
            _ => None,
        }
        .unwrap_or_else(|| op.to_string());
        parts.push(part);
    }
    let mut summary = parts.join(", ");
    if summary.chars().count() > 200 {
        summary = summary.chars().take(197).collect::<String>() + "…";
    }
    if !message.is_empty() {
        summary.push_str(&format!(" — “{message}”"));
    }
    if summary.is_empty() {
        "(empty manifest)".to_string()
    } else {
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_summary_names_what_it_calls() {
        let manifest = "CALL_METHOD\n Address(\"account_tdx_2_1a\")\n \"withdraw\"\n Address(\"resource_x\")\n Decimal(\"1\");\n\
                        CALL_FUNCTION Address(\"package_x\") \"Pam\" \"instantiate\";\n\
                        TAKE_ALL_FROM_WORKTOP Address(\"resource_x\") Bucket(\"b\");";
        assert_eq!(
            manifest_summary(manifest, "hola"),
            "CALL_METHOD withdraw, CALL_FUNCTION Pam::instantiate, TAKE_ALL_FROM_WORKTOP — “hola”"
        );
        assert_eq!(manifest_summary("", ""), "(empty manifest)");
    }

    #[test]
    fn a_pending_request_blocks_with_its_id_and_a_way_out() {
        let now = now_secs();
        let r = Record {
            interaction_id: "abc".into(),
            tool: "send_transaction".into(),
            kind: "transaction".into(),
            network: "stokenet".into(),
            wallet: "pk".into(),
            summary: "CALL_METHOD withdraw".into(),
            created_at: now - 120,
            updated_at: now - 120,
            state: State::Delivered,
            outcome: None,
            code: None,
            pid: 1,
        };
        let f = pending_failure(&[r]);
        assert_eq!(
            (f.code, f.stage, f.retry_safe),
            ("PENDING_IN_WALLET", "queue", true)
        );
        assert!(
            f.detail.contains("abc") && f.detail.contains("2 min"),
            "{}",
            f.detail
        );
        assert!(f.hint.contains("cancel_request") && f.hint.contains("await_response"));
    }
}
