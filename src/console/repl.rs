//! `srg repl` — an interactive terminal player over the **same** decision protocol
//! the web frontend drives (`Session` → `Step` JSON → `submit(index)`; the seam
//! `WasmSession` wraps). A human takes one seat; a local AI policy takes the other.
//!
//! It exists to *understand the wire contract before building UI*, and to give a
//! high-observability play surface for debugging / a Claude-assisted play:
//!   * a live **play-by-play** rendered from the game log between decisions,
//!   * visibility-respecting `inspect` commands over `observable_state`,
//!   * `dump`/`state` full-state introspection (loss-less, for debugging),
//!   * a `--transcript` JSONL **observer feed** (the raw wire traffic + resolved
//!     card names + each choice) an attached observer can read.
//!
//! Because the web will run this same Rust backend, everything here — the log
//! exposure, the observable projection, the transcript — is reusable for real
//! online gameplay, not just the terminal.

use super::loader::{overrides, CardIndex};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use srg_core::cards::Deck;
use srg_core::engine::{DecisionResponse, Step};
use srg_core::gamelog::Event;
use srg_core::session::{Seat, Session};
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::Path;

/// uuid → "number: Name" for every card in both decks (all zones, all visibility).
type Names = BTreeMap<String, (i64, String)>;

/// Run an interactive match. `human` is the human's seat key ("A"/"B"); the other
/// seat is resolved by local policy `opponent`. `transcript`, when set, receives a
/// JSONL observer feed. `debug` echoes the loss-less full state each decision.
#[allow(clippy::too_many_arguments)]
pub fn run(
    cards: &Path,
    decks: (&Path, &Path),
    seed: u64,
    human: &str,
    opponent: &str,
    transcript: Option<&Path>,
    debug: bool,
) -> Result<()> {
    let index = CardIndex::from_yaml(cards)?;
    let ov = overrides()?;
    let deck_a = index.load_playable(decks.0, &ov)?;
    let deck_b = index.load_playable(decks.1, &ov)?;
    let names = name_map(&deck_a, &deck_b);

    let opp = other_seat(human);
    let seats = BTreeMap::from([
        (human.to_owned(), Seat::from_spec("remote")),
        (opp.clone(), Seat::from_spec(opponent)),
    ]);
    let (mut session, mut step) =
        Session::open(deck_a, deck_b, seats, seed, String::new(), "real".into())
            .map_err(|e| anyhow!("open match: {e}"))?;

    let mut tx = transcript
        .map(std::fs::File::create)
        .transpose()
        .map_err(|e| anyhow!("open transcript: {e}"))?;

    println!(
        "SRG interactive — you are seat {human} vs {opponent} (seat {opp}), seed {seed}.\n\
         Type `help` for commands.\n"
    );

    let stdin = std::io::stdin();
    let mut seen_events = 0usize;
    let mut step_no = 0u64;
    loop {
        seen_events = play_by_play(&session, seen_events, &names);
        record_step(&mut tx, step_no, &step, &session, &names, debug)?;
        step_no += 1;

        match &step {
            Step::Done(result) => {
                println!(
                    "\n=== MATCH OVER === winner: {} ({}), turns: {}",
                    result.winner, result.reason, result.turns
                );
                return Ok(());
            }
            Step::Decision(req) => {
                let req = req.clone();
                match prompt(&stdin, &req, &names, &session, debug)? {
                    Some(idx) => {
                        record_choice(&mut tx, step_no - 1, idx, &req, &names)?;
                        let chosen = req.legal[idx].clone();
                        step = session.submit(DecisionResponse {
                            request_id: req.request_id.clone(),
                            chosen,
                        });
                    }
                    None => {
                        println!("(quit)");
                        return Ok(());
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Card naming + rendering
// ---------------------------------------------------------------------------

fn name_map(a: &Deck, b: &Deck) -> Names {
    let mut m = Names::new();
    for deck in [a, b] {
        for c in &deck.cards {
            m.insert(c.db_uuid.clone(), (c.number, c.name.clone()));
        }
    }
    m
}

/// "13: Al13n Invasion" for a uuid, falling back to a bare number/uuid.
fn label(names: &Names, uuid: &str, number: Option<i64>) -> String {
    if let Some((n, name)) = names.get(uuid) {
        format!("{n}: {name}")
    } else if let Some(n) = number {
        format!("{n}: <unknown>")
    } else {
        format!("<{uuid}>")
    }
}

/// One legal option rendered as "(13: Name)" — cards carry number+uuid; non-card
/// options fall back to their `kind`/payload.
fn option_label(names: &Names, opt: &Value) -> String {
    let kind = opt.get("kind").and_then(Value::as_str).unwrap_or("?");
    if let Some(uuid) = opt.get("card").and_then(Value::as_str) {
        let n = opt.get("number").and_then(Value::as_i64);
        return format!("({})", label(names, uuid, n));
    }
    match kind {
        "name" => format!(
            "name={:?}",
            opt.get("name").and_then(Value::as_str).unwrap_or("")
        ),
        "choice" => opt
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("choice")
            .to_owned(),
        "draw" => format!("draw {}", opt.get("n").and_then(Value::as_i64).unwrap_or(0)),
        "seat" => format!(
            "seat {}",
            opt.get("seat").and_then(Value::as_str).unwrap_or("?")
        ),
        "reroll_target" => format!(
            "reroll {}",
            opt.get("target").and_then(Value::as_str).unwrap_or("?")
        ),
        other => other.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Play-by-play (log events since last seen)
// ---------------------------------------------------------------------------

/// Print the log events after `seen`; return the new event count. Renders the
/// notable events (rolls, plays, stops, finishes, breakouts, draws, crowd meter,
/// and unsupported no-ops) — the same feed the web play screen would show.
fn play_by_play(session: &Session, seen: usize, names: &Names) -> usize {
    let Some(log) = session.log() else {
        return seen;
    };
    let events = &log.events;
    for ev in events.iter().skip(seen) {
        if let Some(line) = render_event(ev, names) {
            println!("  · {line}");
        }
    }
    events.len()
}

fn render_event(ev: &Event, names: &Names) -> Option<String> {
    Some(match ev {
        Event::Roll {
            player,
            skill,
            base,
            value,
            mods,
            ..
        } => {
            let m = if mods.is_empty() {
                String::new()
            } else {
                format!(" ({} mods)", mods.len())
            };
            format!("roll {player}: {skill} base {base} → {value}{m}")
        }
        Event::TurnResult {
            winner, tie_bumps, ..
        } => {
            let b = if *tie_bumps > 0 {
                format!(" after {tie_bumps} bump(s)")
            } else {
                String::new()
            };
            format!("turn roll won by {winner}{b}")
        }
        Event::Play {
            player,
            card,
            order,
            atk_type,
            ..
        } => {
            format!(
                "{player} plays ({}) [{order}/{atk_type}]",
                card_lbl(names, card)
            )
        }
        Event::Stop {
            player,
            card,
            stopped,
            ..
        } => {
            format!(
                "{player} STOPS ({}) with ({})",
                card_lbl(names, stopped),
                card_lbl(names, card)
            )
        }
        Event::FinishAttempt {
            player,
            finish,
            value,
            crowd_meter,
            auto_success,
            ..
        } => {
            let a = if *auto_success { " AUTO" } else { "" };
            format!(
                "{player} FINISH ({}) roll {value} vs crowd {crowd_meter}{a}",
                card_lbl(names, finish)
            )
        }
        Event::Breakout {
            defender,
            broke_out,
            ..
        } => {
            format!(
                "{defender} breakout: {}",
                if *broke_out { "SUCCESS" } else { "failed" }
            )
        }
        Event::CrowdMeter { delta, value, .. } => format!("crowd meter {delta:+} → {value}"),
        Event::Draw(m) => format!("{} draws {} card(s)", m.player, m.cards.len()),
        Event::Bury(m) => format!("{} buries {} card(s)", m.player, m.cards.len()),
        Event::Discard(m) => format!("{} discards/flips {} card(s)", m.player, m.cards.len()),
        Event::Unsupported { owner, raw, .. } => format!("[unsupported no-op · {owner}] {raw}"),
        _ => return None,
    })
}

fn card_lbl(names: &Names, uuid: &str) -> String {
    label(names, uuid, None)
}

// ---------------------------------------------------------------------------
// The interactive prompt
// ---------------------------------------------------------------------------

/// Prompt the human for the outstanding decision. Read-only commands (`inspect`,
/// `dump`, `log`, `help`) loop without advancing; a decision command returns the
/// chosen `legal` index; `quit` returns `None`.
fn prompt(
    stdin: &std::io::Stdin,
    req: &srg_core::engine::DecisionRequest,
    names: &Names,
    session: &Session,
    debug: bool,
) -> Result<Option<usize>> {
    println!();
    describe_decision(req, names);
    loop {
        print!("{} [{}]> ", req.viewer, req.point);
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(None); // EOF
        }
        let line = line.trim();
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("");
        let arg = it.next();
        match cmd {
            "" => continue,
            "quit" | "exit" | "q" => return Ok(None),
            "help" | "h" | "?" => print_help(),
            "inspect" | "i" => inspect(
                &req.observable_state,
                arg,
                it.next(),
                req.viewer.as_str(),
                names,
            ),
            "dump" | "state" => dump_state(req, session, debug),
            "log" | "events" => replay_events(session, names),
            _ => match resolve(cmd, arg, &req.legal, req.point.as_str()) {
                Some(idx) => return Ok(Some(idx)),
                None => println!("? unknown or illegal command `{line}` — try `help`"),
            },
        }
    }
}

/// Turn a human command into a `legal` index, or `None` if it doesn't resolve.
/// Card commands (`play`/`stop`/`bury`/`discard`/`pick <n>`) match the card
/// *number*; word commands (`pass`/`none`/`yes`/`no`/`keep`/`redraw`) match a
/// `kind`; `choose <i>`/`opt <i>` is the universal by-index escape hatch.
fn resolve(cmd: &str, arg: Option<&str>, legal: &[Value], _point: &str) -> Option<usize> {
    let by_kind = |k: &str| {
        legal
            .iter()
            .position(|o| o.get("kind").and_then(Value::as_str) == Some(k))
    };
    let by_number = |n: i64| {
        legal
            .iter()
            .position(|o| o.get("number").and_then(Value::as_i64) == Some(n))
    };
    match cmd {
        // universal by-index
        "choose" | "opt" | "c" => arg
            .and_then(|a| a.parse::<usize>().ok())
            .filter(|&i| i < legal.len()),
        // card picks by number
        "play" | "stop" | "bury" | "discard" | "pick" | "p" => {
            arg.and_then(|a| a.parse::<i64>().ok()).and_then(by_number)
        }
        // yes/no + decline families
        "yes" | "y" => by_kind("yes"),
        "no" | "n" => by_kind("no").or_else(|| by_kind("none")),
        "pass" => by_kind("pass").or_else(|| by_kind("none")),
        "none" => by_kind("none"),
        "keep" => by_kind("keep"),
        "redraw" => by_kind("redraw"),
        // named binds / counts / seats
        "name" => arg.and_then(|a| {
            legal
                .iter()
                .position(|o| o.get("name").and_then(Value::as_str) == Some(a))
        }),
        "draw" => arg.and_then(|a| a.parse::<i64>().ok()).and_then(|n| {
            legal
                .iter()
                .position(|o| o.get("n").and_then(Value::as_i64) == Some(n))
        }),
        "seat" => arg.and_then(|a| {
            legal
                .iter()
                .position(|o| o.get("seat").and_then(Value::as_str) == Some(a))
        }),
        _ => None,
    }
}

/// One-line description of the decision plus its legal options as commands.
fn describe_decision(req: &srg_core::engine::DecisionRequest, names: &Names) {
    let os = &req.observable_state;
    println!(
        "── decision `{}` (turn {}, active {}, crowd {}) ──",
        req.point,
        os.get("turn_no").and_then(Value::as_i64).unwrap_or(0),
        os.get("active").and_then(Value::as_str).unwrap_or("?"),
        os.get("crowd_meter").and_then(Value::as_i64).unwrap_or(0),
    );
    if let Some(ctx) = decision_context(&req.legal, names) {
        println!("  → {ctx}");
    }
    println!("  {}", decision_hint(req.point.as_str()));
    for (i, opt) in req.legal.iter().enumerate() {
        println!("    [{i}] {}", option_label(names, opt));
    }
}

/// What the decision is *about*, pulled from the option payloads so a yes/no or a
/// stop is never opaque: the effect's rules text (`clause`), the attack being
/// defended (`vs_order`/`vs_type`), or the same-skill-bump hint (`losing`).
fn decision_context(legal: &[Value], names: &Names) -> Option<String> {
    let first = legal.first()?;
    if let Some(clause) = first.get("clause").and_then(Value::as_str) {
        return Some(format!("\"{clause}\""));
    }
    if let Some(none) = legal
        .iter()
        .find(|o| o.get("kind").and_then(Value::as_str) == Some("none"))
    {
        if let (Some(o), Some(t)) = (
            none.get("vs_order").and_then(Value::as_str),
            none.get("vs_type").and_then(Value::as_str),
        ) {
            return Some(format!("defending a {o} {t}"));
        }
    }
    if let Some(losing) = first.get("losing").and_then(Value::as_bool) {
        return Some(format!(
            "elect the same-skill bump (you are currently {})",
            if losing { "behind" } else { "ahead" }
        ));
    }
    // A card-target decision with a single owner's pool — name the cards involved.
    let cards: Vec<String> = legal
        .iter()
        .filter_map(|o| o.get("card").and_then(Value::as_str))
        .map(|u| label(names, u, None))
        .collect();
    (!cards.is_empty()).then(|| format!("choose among: {}", cards.join(", ")))
}

fn decision_hint(point: &str) -> &'static str {
    match point {
        "turn_action" => "Your turn: `play <#>` a listed card, or `pass`.",
        "stop" => "Stop the attack: `stop <#>`, or `none` to allow it.",
        "optional" | "optional_swap" | "elect_bump" => "`yes` or `no`.",
        "mulligan" => "`redraw` your opening hand, or `keep`.",
        "mulligan_draw" => "`draw <n>` — how many to redraw.",
        "bury" | "bury_hand" | "bury_opp_hand" => "`bury <#>` a listed card.",
        "discard" | "discard_opp_hand" => "`discard <#>` a listed card.",
        "choice" => "`choose <index>` one branch.",
        "name" => "`name <string>` to bind a name.",
        "reshuffle_target" => "`seat <A|B>`.",
        "reroll_target" => "`choose <index>` (SELF/OPP).",
        _ => "Pick a card by number, or `choose <index>`.",
    }
}

// ---------------------------------------------------------------------------
// inspect / dump / log
// ---------------------------------------------------------------------------

fn inspect(os: &Value, zone: Option<&str>, who: Option<&str>, viewer: &str, names: &Names) {
    let (Some(zone), players) = (zone, os.get("players")) else {
        println!("usage: inspect <hand|discard|in_play> [self|opponent]");
        return;
    };
    let key = match who.unwrap_or("self") {
        "self" | "me" | "s" => viewer.to_owned(),
        _ => other_seat(viewer),
    };
    let Some(p) = players.and_then(|pl| pl.get(&key)) else {
        println!("no such player {key}");
        return;
    };
    let zkey = match zone {
        "hand" => "hand",
        "discard" => "discard",
        "in_play" | "inplay" | "board" => "in_play",
        _ => {
            println!("zone must be hand|discard|in_play");
            return;
        }
    };
    if zone == "hand" && p.get("hand").is_none() {
        let n = p.get("hand_size").and_then(Value::as_i64).unwrap_or(0);
        println!("  {key} hand: {n} card(s) [hidden]");
        return;
    }
    let empty = vec![];
    let cards = p.get(zkey).and_then(Value::as_array).unwrap_or(&empty);
    println!("  {key} {zone} ({} card(s)):", cards.len());
    for c in cards {
        let uuid = c.get("db_uuid").and_then(Value::as_str).unwrap_or("");
        let ord = c.get("play_order").and_then(Value::as_str).unwrap_or("");
        let atk = c.get("atk_type").and_then(Value::as_str).unwrap_or("");
        println!("    ({}) [{ord}/{atk}]", label(names, uuid, None));
    }
}

fn dump_state(req: &srg_core::engine::DecisionRequest, session: &Session, debug: bool) {
    if debug {
        if let Some(full) = session.debug_state() {
            println!("{}", serde_json::to_string_pretty(full).unwrap_or_default());
            return;
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&req.observable_state).unwrap_or_default()
    );
}

fn replay_events(session: &Session, names: &Names) {
    let Some(log) = session.log() else { return };
    for ev in &log.events {
        if let Some(line) = render_event(ev, names) {
            println!("  · {line}");
        }
    }
}

fn print_help() {
    println!(
        "commands:\n\
         \x20 inspect <hand|discard|in_play> [self|opponent]   — view a zone (visibility-respecting)\n\
         \x20 dump | state                                     — print observable_state (full state with --debug)\n\
         \x20 log | events                                     — replay the play-by-play so far\n\
         \x20 play <#> | pass                                  — on your turn\n\
         \x20 stop <#> | none                                  — the stop window\n\
         \x20 bury <#> | discard <#> | pick <#>                — card-pick decisions\n\
         \x20 yes | no | keep | redraw | draw <n> | name <s>   — yes/no, mulligan, binds\n\
         \x20 choose <index>                                   — universal: pick legal[index]\n\
         \x20 help | quit"
    );
}

// ---------------------------------------------------------------------------
// Transcript (observer feed)
// ---------------------------------------------------------------------------

fn record_step(
    tx: &mut Option<std::fs::File>,
    step_no: u64,
    step: &Step,
    session: &Session,
    names: &Names,
    debug: bool,
) -> Result<()> {
    let Some(f) = tx.as_mut() else { return Ok(()) };
    let new_events: Vec<Value> = session
        .log()
        .map(|l| {
            l.events
                .iter()
                .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                .collect()
        })
        .unwrap_or_default();
    let rec = match step {
        Step::Done(r) => json!({"step": step_no, "kind": "done",
            "result": {"winner": r.winner, "reason": r.reason, "turns": r.turns}, "log": new_events}),
        Step::Decision(req) => json!({"step": step_no, "kind": "decision", "viewer": req.viewer,
            "point": req.point,
            "legal": req.legal.iter().enumerate()
                .map(|(i, o)| json!({"index": i, "label": option_label(names, o), "option": o}))
                .collect::<Vec<_>>(),
            "observable_state": req.observable_state,
            "full_state": if debug { session.debug_state().cloned().unwrap_or(Value::Null) } else { Value::Null },
            "log": new_events}),
    };
    writeln!(f, "{rec}").map_err(|e| anyhow!("transcript write: {e}"))?;
    Ok(())
}

fn record_choice(
    tx: &mut Option<std::fs::File>,
    step_no: u64,
    idx: usize,
    req: &srg_core::engine::DecisionRequest,
    names: &Names,
) -> Result<()> {
    let Some(f) = tx.as_mut() else { return Ok(()) };
    let rec = json!({"step": step_no, "kind": "choice", "viewer": req.viewer, "point": req.point,
        "chosen_index": idx, "chosen_label": option_label(names, &req.legal[idx]),
        "chosen": req.legal[idx]});
    writeln!(f, "{rec}").map_err(|e| anyhow!("transcript write: {e}"))?;
    Ok(())
}

fn other_seat(key: &str) -> String {
    if key == "A" {
        "B".to_owned()
    } else {
        "A".to_owned()
    }
}
