// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `sugarglider context`: lists and switches contexts, the named window
//! sets. The design is in `docs/specs/contexts.md`, section "Command line".

use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};
use serde::Serialize;
use sugarglider::actor::contexts_snapshot::{ContextsSnapshot, MemberSummary, RequestId, Scope};
use sugarglider::actor::reactor::{ContextCommand, ContextRef, RecordRef};
use sugarglider::actor::server::{ContextRequest, Request, Response};
use sugarglider::model::contexts::{ContextError, ContextId, ContextKey};
use sugarglider::sys::message_port::SendError;

/// What the command says when the server replies with nothing, which is how
/// a server answers a request it can't read, such as a server from before
/// contexts or from before this request (I4).
const OLD_SERVER: &str = "The running Sugarglider doesn't support this command. Restart it.";

/// What a command says when Sugarglider took it but published no result in
/// time.
const NOT_CONFIRMED: &str = "Sugarglider did not confirm the command";

/// How long a command waits between asks for its result.
const RESULT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How many times a command asks for its result, about a second in all.
const RESULT_POLLS: u32 = 50;

#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum CmdContext {
    /// Print the script folder to import in Raycast or SuperCmd.
    LauncherPath,
    /// List the contexts.
    List(Output),
    /// Print the active context.
    Current(Output),
    /// Create a context from the windows on screen, and switch to it.
    Create {
        /// The new context's name.
        name: String,
    },
    /// Switch to a context.
    Switch(Query),
    /// Add the focused window to a context. It stays where it is until the
    /// next switch.
    Add(Query),
    /// Show every window.
    Everything,
    /// Switch back to the context used before the current one.
    Previous,
    /// Move the focused window out of the active context and into another.
    Move(Query),
    /// Remove the focused window from the active context.
    Remove,
    /// Rename a context.
    Rename {
        /// The context's number from 1 to 9, or its name or part of it.
        #[command(flatten)]
        query: Query,
        /// The new name.
        new_name: String,
    },
    /// Delete a context. Its windows stay open.
    Delete(Query),
    /// Give a context a number from 1 to 9.
    Number {
        /// The context's number from 1 to 9, or its name or part of it.
        #[command(flatten)]
        query: Query,
        /// The number to give the context, from 1 to 9.
        number: u8,
    },
    /// Pin the focused window, or unpin it. A pinned window is a member of
    /// every context.
    Pin,
    /// Remove the member record of a window that is gone.
    Forget {
        /// The context's number from 1 to 9, or its name or part of it.
        #[command(flatten)]
        query: Query,
        /// The record's index, as `list --json` prints it in `members`.
        record: usize,
    },
}

/// Names a context.
#[derive(Args, Clone, Debug, PartialEq)]
pub struct Query {
    /// The context's number from 1 to 9, or its name or part of it.
    query: String,
    /// Take the query as a name, also when it is a number, for a context
    /// named "2024".
    #[arg(long)]
    name: bool,
    /// Use a stable context ID from `list --json`, without fuzzy matching.
    #[arg(long, conflicts_with = "name")]
    id: bool,
}

#[derive(Args, Clone, Debug, PartialEq)]
pub struct Output {
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

/// Sends a message to the server and returns its reply.
pub trait Transport {
    fn request(&mut self, message: &[u8]) -> Result<Vec<u8>, SendError>;

    /// Waits before the next ask for a command's result.
    fn pause(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Runs the command on the server that `connect` reaches, or fails with
/// `None` when no server runs. Prints the result to `out`, or the reason for
/// a failure to `err`, and returns the exit status.
pub fn run<T: Transport>(
    command: &CmdContext,
    connect: impl FnOnce() -> Option<T>,
    out: &mut impl Write,
    err: &mut impl Write,
) -> u8 {
    match execute(command, connect) {
        Ok(text) => {
            _ = out.write_all(text.as_bytes());
            0
        }
        Err(reason) => {
            _ = writeln!(err, "{reason}");
            1
        }
    }
}

fn execute<T: Transport>(
    command: &CmdContext,
    connect: impl FnOnce() -> Option<T>,
) -> Result<String, String> {
    if matches!(command, CmdContext::LauncherPath) {
        return Ok(format!(
            "{}\n",
            sugarglider::actor::context_launchers::directory().display()
        ));
    }
    let id = new_request_id();
    let run = |command| ContextRequest::Run(id, command);
    let mut transport = connect().ok_or("Sugarglider isn't running.")?;
    let request = match command {
        CmdContext::LauncherPath => unreachable!("handled before connecting"),
        CmdContext::List(_) => ContextRequest::List,
        CmdContext::Current(_) => ContextRequest::Current,
        CmdContext::Create { name } => run(ContextCommand::CreateContext(name.clone())),
        CmdContext::Switch(query) => run(ContextCommand::SwitchContext(parse_query(query)?)),
        CmdContext::Add(query) => run(ContextCommand::AddWindowToContext(parse_query(query)?)),
        CmdContext::Everything => run(ContextCommand::ShowEverything),
        CmdContext::Previous => run(ContextCommand::PreviousContext),
        CmdContext::Move(query) => run(ContextCommand::MoveWindowToContext(parse_query(query)?)),
        CmdContext::Remove => run(ContextCommand::RemoveWindowFromContext),
        CmdContext::Rename { query, new_name } => run(ContextCommand::RenameContext {
            context: parse_query(query)?,
            name: new_name.clone(),
        }),
        CmdContext::Delete(query) => run(ContextCommand::DeleteContext(parse_query(query)?)),
        CmdContext::Number { query, number } => run(ContextCommand::SetContextNumber {
            context: parse_query(query)?,
            number: check_number(*number)?,
        }),
        CmdContext::Pin => run(ContextCommand::ToggleWindowPinned),
        CmdContext::Forget { query, record } => {
            run(forget_command(&mut transport, query, *record)?)
        }
    };
    match (send(&mut transport, request)?, command) {
        (
            Response::Contexts(snapshot),
            CmdContext::List(Output { json: true }) | CmdContext::Current(Output { json: true }),
        ) => Ok(json(&snapshot)),
        (Response::Contexts(snapshot), CmdContext::List(_)) => Ok(list(&snapshot)),
        (Response::Contexts(snapshot), CmdContext::Current(_)) => {
            Ok(format!("{}\n", name(&snapshot, snapshot.active)))
        }
        (
            Response::Success,
            CmdContext::Create { .. }
            | CmdContext::Switch(_)
            | CmdContext::Add(_)
            | CmdContext::Everything
            | CmdContext::Previous
            | CmdContext::Move(_)
            | CmdContext::Remove
            | CmdContext::Rename { .. }
            | CmdContext::Delete(_)
            | CmdContext::Number { .. }
            | CmdContext::Pin
            | CmdContext::Forget { .. },
        ) => wait_for_result(&mut transport, id).map(|()| String::new()),
        (Response::Error(reason), _) => Err(reason),
        (response, _) => Err(unexpected(&response)),
    }
}

/// The command that removes the record at `record` of the context that
/// `query` names. It reads the snapshot first, so the command carries the
/// record's app and title, which the reactor checks: a list that shifted
/// since the user read it can't remove another record. A record whose
/// window is open is refused here.
fn forget_command(
    transport: &mut impl Transport,
    query: &Query,
    record: usize,
) -> Result<ContextCommand, String> {
    let snapshot = match send(transport, ContextRequest::List)? {
        Response::Contexts(snapshot) => snapshot,
        Response::Error(reason) => return Err(reason),
        response => return Err(unexpected(&response)),
    };
    let reference = parse_query(query)?;
    let key = snapshot.resolve(reference.query()).map_err(|err| err.to_string())?;
    let ContextKey::Named(id) = key else {
        return Err("Only a named context has member records".to_string());
    };
    let member = snapshot
        .get(id)
        .and_then(|context| context.members.get(record))
        .ok_or_else(|| ContextError::NoSuchRecord.to_string())?;
    if member.window.is_some() {
        return Err("The member's window is open; remove the window instead".to_string());
    }
    Ok(ContextCommand::RemoveRecord {
        context: ContextRef::Id(id),
        record: RecordRef {
            record,
            app: member.app.clone(),
            title: member.title.clone(),
        },
    })
}

/// A request id that another run of the command is unlikely to pick: the
/// time in nanoseconds, mixed with the process id.
fn new_request_id() -> RequestId {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_nanos());
    RequestId((nanos as u64) ^ (u64::from(std::process::id()) << 32))
}

/// Asks for the result of the command sent with `request` until the reactor
/// has run it, for about a second.
fn wait_for_result(transport: &mut impl Transport, request: RequestId) -> Result<(), String> {
    for _ in 0..RESULT_POLLS {
        transport.pause(RESULT_POLL_INTERVAL);
        match send(transport, ContextRequest::Result(request))? {
            Response::Success => return Ok(()),
            Response::Error(reason) => return Err(reason),
            Response::Pending => {}
            response => return Err(unexpected(&response)),
        }
    }
    Err(NOT_CONFIRMED.to_string())
}

fn unexpected(response: &Response) -> String {
    format!("Unexpected reply from Sugarglider: {response:?}")
}

/// A whole number names a context by its number, from 1 to 9. Any other
/// text is a name, and so is a number with `--name`. The reactor matches
/// names with the switcher's ranking.
fn parse_query(query: &Query) -> Result<ContextRef, String> {
    let Query { query, name, id } = query;
    let trimmed = query.trim();
    if *id {
        let value =
            trimmed.parse::<u32>().ok().filter(|id| *id != 0).ok_or_else(|| {
                "A context ID must be an integer from 1 to 4294967295".to_string()
            })?;
        return Ok(ContextRef::Id(ContextId::from_raw(value)));
    }
    if *name || trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(ContextRef::Name(query.to_string()));
    }
    match trimmed.parse::<u8>() {
        Ok(number @ 1..=9) => Ok(ContextRef::Number(number)),
        _ => Err(format!(
            "Context numbers go from 1 to 9, not {trimmed}. For a context with that name, \
             add --name."
        )),
    }
}

/// Checks a number from 1 to 9, the numbers a context can have (R5).
fn check_number(number: u8) -> Result<u8, String> {
    if (1..=9).contains(&number) {
        return Ok(number);
    }
    Err(format!("Context numbers go from 1 to 9, not {number}"))
}

fn send(transport: &mut impl Transport, request: ContextRequest) -> Result<Response, String> {
    let message = ron::ser::to_string(&Request::Context(request))
        .map_err(|err| format!("Could not write the request: {err}"))?;
    let reply = transport
        .request(message.as_bytes())
        .map_err(|err| format!("Could not reach Sugarglider: {err}"))?;
    if reply.is_empty() {
        return Err(OLD_SERVER.to_string());
    }
    ron::de::from_bytes(&reply).map_err(|err| {
        format!(
            "Could not read the reply from Sugarglider, which may be another version. \
             Restart it. ({err})"
        )
    })
}

fn name(snapshot: &ContextsSnapshot, key: ContextKey) -> &str {
    snapshot.name(key).unwrap_or("Unknown context")
}

/// One line per entry: a star on the active one, the number, the name, and
/// how many windows are open, with their apps. Unsorted is listed while the
/// snapshot lists it.
fn list(snapshot: &ContextsSnapshot) -> String {
    let windows = |count: usize| match count {
        1 => "1 window".to_string(),
        count => format!("{count} windows"),
    };
    let mut rows: Vec<(ContextKey, Option<u8>, String)> = snapshot
        .contexts
        .iter()
        .map(|context| {
            let mut detail = windows(context.windows);
            if !context.apps.is_empty() {
                detail = format!("{detail}  {}", context.apps.join(", "));
            }
            (ContextKey::Named(context.id), context.number, detail)
        })
        .collect();
    if snapshot.unsorted.listed {
        rows.push((ContextKey::Unsorted, None, windows(snapshot.unsorted.windows)));
    }
    rows.push((ContextKey::Everything, None, String::new()));
    let width = rows.iter().map(|(key, ..)| name(snapshot, *key).chars().count()).max();
    let width = width.unwrap_or(0);
    let mut text = String::new();
    for (key, number, detail) in rows {
        let marker = if key == snapshot.active { '*' } else { ' ' };
        let number = number.map_or(" ".to_string(), |n| n.to_string());
        let line = format!("{marker} {number} {:<width$}  {detail}", name(snapshot, key));
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

/// The shape in the spec's "Command line" section, with the active context
/// at the top level.
#[derive(Serialize)]
struct SnapshotJson<'a> {
    scope: Scope,
    /// The name of the active context, also when it is Everything or
    /// Unsorted, and while no screen shows a managed Space.
    active: &'a str,
    screens: Vec<ScreenJson<'a>>,
    contexts: Vec<ContextJson<'a>>,
    /// How many windows on the visible Spaces are unsorted.
    unsorted: usize,
}

#[derive(Serialize)]
struct ScreenJson<'a> {
    id: u32,
    active: &'a str,
}

#[derive(Serialize)]
struct ContextJson<'a> {
    id: u32,
    name: &'a str,
    number: Option<u8>,
    active: bool,
    apps: &'a [String],
    /// How many member windows are open, wherever they are, pinned windows
    /// included.
    windows: usize,
    /// Every member record, with the index that `context forget` takes.
    members: &'a [MemberSummary],
}

fn json(snapshot: &ContextsSnapshot) -> String {
    let view = SnapshotJson {
        scope: snapshot.scope,
        active: name(snapshot, snapshot.active),
        screens: snapshot
            .screens
            .iter()
            .map(|screen| ScreenJson {
                id: screen.id,
                active: name(snapshot, screen.shows),
            })
            .collect(),
        contexts: snapshot
            .contexts
            .iter()
            .map(|context| ContextJson {
                id: context.id.get(),
                name: &context.name,
                number: context.number,
                active: snapshot.active == ContextKey::Named(context.id),
                apps: &context.apps,
                windows: context.windows,
                members: &context.members,
            })
            .collect(),
        unsorted: snapshot.unsorted.windows,
    };
    let mut text = serde_json::to_string_pretty(&view).expect("the snapshot serializes");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use clap::Parser;
    use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
    use pretty_assertions::assert_eq;
    use serde::Deserialize;
    use serde_json::json;
    use sugarglider::actor::app::WindowId;
    use sugarglider::actor::contexts_snapshot::{
        ContextSummary, EverythingSummary, ScreenContext, UnsortedSummary,
    };
    use sugarglider::config::Config;
    use sugarglider::model::contexts::ContextId;
    use sugarglider::sys::message_port::{LocalMessagePort, RemoteMessagePort};
    use sugarglider::sys::window_server::WindowServerId;

    use super::*;
    use crate::{Client, Command, Opt};

    fn id(id: u32) -> ContextId {
        serde_json::from_value(json!(id)).unwrap()
    }

    fn summary(
        id: u32,
        name: &str,
        apps: &[&str],
        windows: usize,
        members: Vec<MemberSummary>,
    ) -> ContextSummary {
        ContextSummary {
            id: self::id(id),
            name: name.into(),
            number: u8::try_from(id).ok(),
            last_used: 0,
            apps: apps.iter().map(|app| app.to_string()).collect(),
            windows,
            members,
        }
    }

    /// A member record that `context list --json` prints with its index,
    /// app, and title, and with `window` null when the window is gone.
    fn member(record: usize, app: &str, title: &str, window: Option<WindowId>) -> MemberSummary {
        MemberSummary {
            record,
            app: app.into(),
            title: title.into(),
            window,
        }
    }

    /// The window of a record, as a real window server id names it.
    fn window(pid: i32, idx: u32) -> Option<WindowId> {
        Some(WindowId::with_wsid(pid, WindowServerId::new(idx)))
    }

    /// The spec's example: Comms is active, and 3 windows are unsorted.
    /// Comms holds two open records and the record of a window that is
    /// gone, and Relax holds none.
    fn snapshot() -> ContextsSnapshot {
        ContextsSnapshot {
            enabled: true,
            scope: Scope::Global,
            active: ContextKey::Named(id(1)),
            focused_screen: None,
            screens: vec![ScreenContext {
                id: 1,
                shows: ContextKey::Named(id(1)),
            }],
            contexts: vec![
                summary(
                    1,
                    "Comms",
                    &["WhatsApp", "Microsoft Teams"],
                    2,
                    vec![
                        member(0, "WhatsApp", "WhatsApp", window(903, 9201)),
                        member(1, "Microsoft Teams", "Team", window(904, 9202)),
                        member(2, "Mail", "Inbox", None),
                    ],
                ),
                summary(2, "Relax", &["WhatsApp", "Google Chrome"], 2, vec![]),
            ],
            unsorted: UnsortedSummary {
                listed: true,
                windows: 3,
                last_used: 0,
            },
            everything: EverythingSummary { last_used: 0 },
            results: vec![],
        }
    }

    fn parse(args: &[&str]) -> Result<CmdContext, clap::Error> {
        let args = ["sugarglider", "context"].iter().chain(args);
        match Opt::try_parse_from(args)?.command {
            Command::Context(command) => Ok(command),
            _ => panic!("not a context command"),
        }
    }

    /// A server that answers every request with the same bytes, except a
    /// `List`, which it answers with `list_reply` when it has one, and each
    /// ask for a command's result, which it answers with the next of
    /// `results` and then with the last one again.
    struct Server {
        reply: Vec<u8>,
        list_reply: Option<Vec<u8>>,
        results: Vec<Response>,
        requests: Vec<ContextRequest>,
        pauses: Vec<Duration>,
    }

    impl Transport for &mut Server {
        fn request(&mut self, message: &[u8]) -> Result<Vec<u8>, SendError> {
            let request = match ron::de::from_bytes(message).unwrap() {
                Request::Context(request) => request,
                other => panic!("{other:?}"),
            };
            let is_result = matches!(request, ContextRequest::Result(_));
            let is_list = matches!(request, ContextRequest::List);
            self.requests.push(request);
            if !is_result {
                return Ok(match (is_list, &self.list_reply) {
                    (true, Some(reply)) => reply.clone(),
                    _ => self.reply.clone(),
                });
            }
            let asked = self.requests.iter().filter(|r| matches!(r, ContextRequest::Result(_)));
            let result = &self.results[(asked.count() - 1).min(self.results.len() - 1)];
            Ok(ron::ser::to_string(result).unwrap().into_bytes())
        }

        fn pause(&mut self, duration: Duration) {
            self.pauses.push(duration);
        }
    }

    struct Ran {
        status: u8,
        out: String,
        err: String,
        requests: Vec<ContextRequest>,
        pauses: Vec<Duration>,
    }

    impl Ran {
        /// The commands sent with `Run`. Each ask for a result names the id
        /// of the command sent before it.
        fn commands(&self) -> Vec<ContextCommand> {
            let mut sent = None;
            let mut commands = Vec::new();
            for request in &self.requests {
                match request {
                    ContextRequest::Run(id, command) => {
                        sent = Some(*id);
                        commands.push(command.clone());
                    }
                    ContextRequest::Result(id) => assert_eq!(sent, Some(*id)),
                    ContextRequest::List | ContextRequest::Current => {}
                }
            }
            commands
        }

        /// How many times the command asked for its result.
        fn asks(&self) -> usize {
            let asks = self.requests.iter().filter(|r| matches!(r, ContextRequest::Result(_)));
            asks.count()
        }
    }

    /// Runs `sugarglider context <args>` against a server that replies with
    /// `reply`, and with `results` to each ask for a command's result, or
    /// with no server when `reply` is `None`.
    fn run_against(args: &[&str], reply: Option<Vec<u8>>, results: Vec<Response>) -> Ran {
        let server = reply.map(|reply| Server {
            reply,
            list_reply: None,
            results,
            requests: Vec::new(),
            pauses: Vec::new(),
        });
        run_server(args, server)
    }

    /// Runs the command against `server`, or with no server when it is
    /// `None`, and collects what it printed and asked.
    fn run_server(args: &[&str], server: Option<Server>) -> Ran {
        let command = parse(args).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut server = server;
        let status = run(&command, || server.as_mut(), &mut out, &mut err);
        let (requests, pauses) =
            server.map(|server| (server.requests, server.pauses)).unwrap_or_default();
        Ran {
            status,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
            requests,
            pauses,
        }
    }

    /// Runs `sugarglider context forget <args>` against a server that
    /// answers `List` with the spec's snapshot, takes the command with
    /// `Success`, and answers the asks for its result with `results`.
    fn run_forget(args: &[&str], results: Vec<Response>) -> Ran {
        let reply = ron::ser::to_string(&Response::Success).unwrap().into_bytes();
        let list_reply = ron::ser::to_string(&Response::Contexts(snapshot())).unwrap().into_bytes();
        run_server(
            args,
            Some(Server {
                reply,
                list_reply: Some(list_reply),
                results,
                requests: Vec::new(),
                pauses: Vec::new(),
            }),
        )
    }

    /// Runs `sugarglider context <args>` against a server that replies with
    /// `reply`, or with no server when `reply` is `None`. A command that the
    /// server takes has run by the first ask for its result.
    fn run_raw(args: &[&str], reply: Option<Vec<u8>>) -> Ran {
        run_against(args, reply, vec![Response::Success])
    }

    fn run_with(args: &[&str], reply: Response) -> Ran {
        run_raw(args, Some(ron::ser::to_string(&reply).unwrap().into_bytes()))
    }

    /// Runs `sugarglider context <args>` against a server that takes the
    /// command and gives `results` to the asks for its result.
    fn run_with_results(args: &[&str], results: Vec<Response>) -> Ran {
        let reply = ron::ser::to_string(&Response::Success).unwrap().into_bytes();
        run_against(args, Some(reply), results)
    }

    fn switch(reference: ContextRef) -> ContextCommand {
        ContextCommand::SwitchContext(reference)
    }

    fn query(text: &str) -> Query {
        Query {
            query: text.into(),
            name: false,
            id: false,
        }
    }

    fn by_name(text: &str) -> Query {
        Query {
            query: text.into(),
            name: true,
            id: false,
        }
    }

    #[test]
    fn launcher_ids_are_exact_and_wait_for_the_command_result() {
        let ran = run_with_results(
            &["switch", "--id", "42"],
            vec![Response::Pending, Response::Success],
        );
        assert_eq!(0, ran.status);
        assert!(
            matches!(&ran.requests[0], ContextRequest::Run(_, ContextCommand::SwitchContext(ContextRef::Id(value))) if value.get() == 42)
        );
        assert_eq!(3, ran.requests.len());
        let failed = run_with_results(
            &["switch", "--id", "42"],
            vec![Response::Error("No such context".into())],
        );
        assert_eq!(1, failed.status);
        assert!(failed.err.contains("No such context"));
        for value in ["0", "-1", "3.5", "4294967296", "Client work"] {
            assert!(
                parse_query(&Query {
                    query: value.into(),
                    name: false,
                    id: true
                })
                .is_err()
            );
        }
        assert!(parse(&["switch", "--id", "--name", "42"]).is_err());
    }

    #[test]
    fn launcher_path_does_not_require_a_running_server() {
        let ran = run_raw(&["launcher-path"], None);
        assert_eq!(0, ran.status);
        assert!(ran.out.ends_with("/.config/raycast/script-commands/sugarglider-contexts\n"));
    }

    /// Clap's own errors, such as a missing argument, exit with status 2
    /// before the command runs.
    #[test]
    fn subcommands_parse() {
        let output = |json| Output { json };
        for (args, command) in [
            (&["list"][..], CmdContext::List(output(false))),
            (&["list", "--json"], CmdContext::List(output(true))),
            (&["current"], CmdContext::Current(output(false))),
            (&["current", "--json"], CmdContext::Current(output(true))),
            (
                &["create", "Client work"],
                CmdContext::Create { name: "Client work".into() },
            ),
            (&["switch", "2"], CmdContext::Switch(query("2"))),
            (
                &["switch", "--name", "2024"],
                CmdContext::Switch(by_name("2024")),
            ),
            (
                &["switch", "2024", "--name"],
                CmdContext::Switch(by_name("2024")),
            ),
            (&["add", "Comms"], CmdContext::Add(query("Comms"))),
            (&["add", "--name", "3"], CmdContext::Add(by_name("3"))),
            (&["everything"], CmdContext::Everything),
            (&["previous"], CmdContext::Previous),
            (&["move", "Comms"], CmdContext::Move(query("Comms"))),
            (&["remove"], CmdContext::Remove),
            (
                &["rename", "Comms", "Client work"],
                CmdContext::Rename {
                    query: query("Comms"),
                    new_name: "Client work".into(),
                },
            ),
            (
                &["rename", "--name", "2024", "Twenty"],
                CmdContext::Rename {
                    query: by_name("2024"),
                    new_name: "Twenty".into(),
                },
            ),
            (&["delete", "2"], CmdContext::Delete(query("2"))),
            (
                &["delete", "--name", "2024"],
                CmdContext::Delete(by_name("2024")),
            ),
            (
                &["number", "Comms", "3"],
                CmdContext::Number {
                    query: query("Comms"),
                    number: 3,
                },
            ),
            (
                &["number", "--name", "3", "9"],
                CmdContext::Number { query: by_name("3"), number: 9 },
            ),
            (&["pin"], CmdContext::Pin),
            (
                &["forget", "Comms", "0"],
                CmdContext::Forget {
                    query: query("Comms"),
                    record: 0,
                },
            ),
        ] {
            assert_eq!(command, parse(args).unwrap(), "{args:?}");
        }
        for args in [
            &["create"][..],
            &["switch"],
            &["switch", "a", "b"],
            &["switch", "--name"],
            &["add"],
            &["rename", "Comms"],
            &["number", "Comms"],
            &["number", "Comms", "ten"],
            &["forget", "Comms"],
            &["forget", "Comms", "-1"],
            &["list", "--yaml"],
        ] {
            let err = parse(args).unwrap_err();
            assert_eq!(2, err.exit_code(), "{args:?}");
        }
    }

    /// A whole number from 1 to 9 is a number, and other text is a name.
    /// With `--name`, a number is a name too.
    #[test]
    fn a_query_is_a_number_or_a_name() {
        let parse_query = |text: &str| parse_query(&query(text));
        assert_eq!(Ok(ContextRef::Number(2)), parse_query("2"));
        assert_eq!(Ok(ContextRef::Number(9)), parse_query(" 09 "));
        assert_eq!(Ok(ContextRef::Name("cli".into())), parse_query("cli"));
        assert_eq!(Ok(ContextRef::Name("2nd".into())), parse_query("2nd"));
        assert_eq!(Ok(ContextRef::Name("-1".into())), parse_query("-1"));
        for query in ["0", "10", "300"] {
            assert_eq!(
                Err(format!(
                    "Context numbers go from 1 to 9, not {query}. For a context with that \
                     name, add --name."
                )),
                parse_query(query)
            );
        }
        for text in ["3", "2024", " 0 "] {
            assert_eq!(
                Ok(ContextRef::Name(text.into())),
                super::parse_query(&by_name(text))
            );
        }
    }

    #[test]
    fn list_prints_one_line_per_entry() {
        let ran = run_with(&["list"], Response::Contexts(snapshot()));

        assert_eq!(
            "* 1 Comms       2 windows  WhatsApp, Microsoft Teams\n\
            \x20 2 Relax       2 windows  WhatsApp, Google Chrome\n\
            \x20   Unsorted    3 windows\n\
            \x20   Everything\n",
            ran.out
        );
        assert_eq!("", ran.err);
        assert_eq!(0, ran.status);
        assert_eq!(vec![ContextRequest::List], ran.requests);
    }

    /// R29. Unsorted is listed while the snapshot lists it, which is while
    /// it has windows. An active Unsorted without windows is left out, so no
    /// line has the star.
    #[test]
    fn list_leaves_out_an_empty_unsorted() {
        let mut snapshot = snapshot();
        snapshot.unsorted.listed = false;
        snapshot.unsorted.windows = 0;
        snapshot.contexts[0].windows = 1;
        snapshot.contexts[0].apps.truncate(1);
        snapshot.contexts[1].number = None;
        assert_eq!(
            "* 1 Comms       1 window  WhatsApp\n\
            \x20   Relax       2 windows  WhatsApp, Google Chrome\n\
            \x20   Everything\n",
            run_with(&["list"], Response::Contexts(snapshot.clone())).out
        );
        snapshot.active = ContextKey::Unsorted;
        assert_eq!(
            "  1 Comms       1 window  WhatsApp\n\
            \x20   Relax       2 windows  WhatsApp, Google Chrome\n\
            \x20   Everything\n",
            run_with(&["list"], Response::Contexts(snapshot)).out
        );
    }

    /// The JSON is the shape in the spec's "Command line" section.
    #[test]
    fn list_json_prints_the_spec_shape() {
        let ran = run_with(&["list", "--json"], Response::Contexts(snapshot()));

        let printed: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(
            json!({
              "scope": "global",
              "active": "Comms",
              "screens": [{ "id": 1, "active": "Comms" }],
              "contexts": [
                { "id": 1, "name": "Comms", "number": 1, "active": true,
                  "apps": ["WhatsApp", "Microsoft Teams"], "windows": 2,
                  "members": [
                    { "record": 0, "app": "WhatsApp", "title": "WhatsApp",
                      "window": { "pid": 903, "idx": 9201 } },
                    { "record": 1, "app": "Microsoft Teams", "title": "Team",
                      "window": { "pid": 904, "idx": 9202 } },
                    { "record": 2, "app": "Mail", "title": "Inbox", "window": null }
                  ] },
                { "id": 2, "name": "Relax", "number": 2, "active": false,
                  "apps": ["WhatsApp", "Google Chrome"], "windows": 2,
                  "members": [] }
              ],
              "unsorted": 3
            }),
            printed
        );
        assert_eq!("", ran.err);
        assert_eq!(0, ran.status);
    }

    #[test]
    fn current_prints_the_active_context() {
        let ran = run_with(&["current"], Response::Contexts(snapshot().current()));
        assert_eq!("Comms\n", ran.out);
        assert_eq!(0, ran.status);
        assert_eq!(vec![ContextRequest::Current], ran.requests);

        let mut everything = snapshot();
        everything.active = ContextKey::Everything;
        everything.screens[0].shows = ContextKey::Everything;
        let ran = run_with(&["current"], Response::Contexts(everything.current()));
        assert_eq!("Everything\n", ran.out);

        let ran = run_with(&["current", "--json"], Response::Contexts(snapshot().current()));
        let printed: serde_json::Value = serde_json::from_str(&ran.out).unwrap();
        assert_eq!(json!("Comms"), printed["contexts"][0]["name"]);
        assert_eq!(1, printed["contexts"].as_array().unwrap().len());
    }

    /// A command sends its request, asks for its result after a pause, and
    /// prints nothing when the command ran.
    #[test]
    fn commands_send_their_request_and_print_nothing() {
        for (args, command) in [
            (
                &["create", "Client work"][..],
                ContextCommand::CreateContext("Client work".into()),
            ),
            (&["switch", "2"], switch(ContextRef::Number(2))),
            (&["switch", "cli"], switch(ContextRef::Name("cli".into()))),
            (&["everything"], ContextCommand::ShowEverything),
        ] {
            let ran = run_with(args, Response::Success);
            assert_eq!(("", "", 0), (&*ran.out, &*ran.err, ran.status), "{args:?}");
            assert_eq!(vec![command], ran.commands());
            assert_eq!(1, ran.asks());
            assert_eq!(vec![RESULT_POLL_INTERVAL], ran.pauses);
        }
    }

    /// M6. Every subcommand that changes a context sends its command, waits
    /// for its result, and prints nothing on success. A number outside 1 to
    /// 9 fails before anything is sent.
    #[test]
    fn the_new_subcommands_send_their_command_and_print_nothing() {
        for (args, command) in [
            (&["previous"][..], ContextCommand::PreviousContext),
            (
                &["move", "Comms"],
                ContextCommand::MoveWindowToContext(ContextRef::Name("Comms".into())),
            ),
            (&["remove"], ContextCommand::RemoveWindowFromContext),
            (
                &["rename", "2", "Client work"],
                ContextCommand::RenameContext {
                    context: ContextRef::Number(2),
                    name: "Client work".into(),
                },
            ),
            (
                &["delete", "Comms"],
                ContextCommand::DeleteContext(ContextRef::Name("Comms".into())),
            ),
            (
                &["number", "Comms", "3"],
                ContextCommand::SetContextNumber {
                    context: ContextRef::Name("Comms".into()),
                    number: 3,
                },
            ),
            (&["pin"], ContextCommand::ToggleWindowPinned),
        ] {
            let ran = run_with(args, Response::Success);
            assert_eq!((0, "", ""), (ran.status, &*ran.out, &*ran.err), "{args:?}");
            assert_eq!(vec![command], ran.commands(), "{args:?}");
            assert_eq!(1, ran.asks(), "{args:?}");
            assert_eq!(vec![RESULT_POLL_INTERVAL], ran.pauses, "{args:?}");
        }

        for number in ["0", "10", "255"] {
            let ran = run_with(&["number", "Comms", number], Response::Success);
            assert_eq!(
                (1, "", format!("Context numbers go from 1 to 9, not {number}\n")),
                (ran.status, &*ran.out, ran.err),
                "{number}"
            );
            assert!(ran.requests.is_empty(), "{number}");
        }
    }

    /// M6. `list --json` prints the index of each member record, and
    /// `context forget` sends that index with the record's app and title,
    /// read from the snapshot, so the reactor can check that it still names
    /// the same record.
    #[test]
    fn a_forget_index_is_the_record_index_of_list_json() {
        let printed = printed_json(&["list", "--json"], snapshot());
        assert_eq!(json!(2), printed["contexts"][0]["members"][2]["record"]);
        assert_eq!(json!("Inbox"), printed["contexts"][0]["members"][2]["title"]);
        assert_eq!(json!(null), printed["contexts"][0]["members"][2]["window"]);

        let ran = run_forget(&["forget", "Comms", "2"], vec![Response::Success]);
        assert_eq!((0, "", ""), (ran.status, &*ran.out, &*ran.err));
        assert_eq!(Some(&ContextRequest::List), ran.requests.first());
        assert_eq!(
            vec![ContextCommand::RemoveRecord {
                context: ContextRef::Id(id(1)),
                record: RecordRef {
                    record: 2,
                    app: "Mail".into(),
                    title: "Inbox".into(),
                },
            }],
            ran.commands()
        );
    }

    /// M6. `forget` checks the index against the snapshot before it sends:
    /// a record whose window is open, an index off the end, a built-in,
    /// and a name that matches nothing fail without sending a command.
    #[test]
    fn forget_checks_the_record_against_the_snapshot_first() {
        for (args, reason) in [
            (
                &["forget", "Comms", "0"][..],
                "The member's window is open; remove the window instead",
            ),
            (&["forget", "Comms", "9"], "No such member record"),
            (
                &["forget", "Unsorted", "0"],
                "Only a named context has member records",
            ),
            (&["forget", "nothing", "0"], "No context matches \"nothing\""),
        ] {
            let ran = run_forget(args, vec![Response::Success]);
            assert_eq!(
                (1, "", format!("{reason}\n")),
                (ran.status, &*ran.out, ran.err),
                "{args:?}"
            );
            assert_eq!(vec![ContextRequest::List], ran.requests, "{args:?}");
        }
    }

    /// M6. The reactor checks the record again, so a list that changed
    /// between the snapshot and the command fails instead of removing
    /// another record. Its reason goes to stderr with status 1.
    #[test]
    fn forget_prints_the_reactors_reason_when_the_record_changed() {
        let reason = "The member record changed since it was listed; list the contexts again";
        let ran = run_forget(
            &["forget", "Comms", "2"],
            vec![Response::Pending, Response::Error(reason.into())],
        );

        assert_eq!(2, ran.asks());
        assert_eq!((1, "", format!("{reason}\n")), (ran.status, &*ran.out, ran.err));
    }

    /// M6. The reactor's reason for a command that did nothing goes to
    /// stderr, and the exit status is 1, for every new subcommand.
    #[test]
    fn a_new_subcommand_that_did_nothing_prints_the_reason() {
        let reason = "Only a named context can be deleted";
        for args in [
            &["previous"][..],
            &["move", "Comms"],
            &["remove"],
            &["rename", "Comms", "Work"],
            &["delete", "Unsorted"],
            &["number", "Comms", "3"],
            &["pin"],
        ] {
            let ran = run_with_results(args, vec![Response::Error(reason.into())]);
            assert_eq!(
                (1, "", format!("{reason}\n")),
                (ran.status, &*ran.out, ran.err),
                "{args:?}"
            );
        }
    }

    /// A command asks for its result every 20 ms until the reactor has run
    /// it, and then exits with status 0.
    #[test]
    fn a_command_waits_for_its_result() {
        let ran = run_with_results(
            &["switch", "Comms"],
            vec![Response::Pending, Response::Pending, Response::Success],
        );

        assert_eq!((0, "", ""), (ran.status, &*ran.out, &*ran.err));
        assert_eq!(3, ran.asks());
        assert_eq!(vec![Duration::from_millis(20); 3], ran.pauses);
        assert_eq!(vec![switch(ContextRef::Name("Comms".into()))], ran.commands());
    }

    /// A command that the reactor ran but that did nothing prints the
    /// reason on stderr and exits with status 1.
    #[test]
    fn a_command_that_did_nothing_prints_the_reason() {
        let reason = "No context matches \"xyz\"";
        let ran = run_with_results(
            &["switch", "xyz"],
            vec![Response::Pending, Response::Error(reason.into())],
        );

        assert_eq!(2, ran.asks());
        assert_eq!((1, "", format!("{reason}\n")), (ran.status, &*ran.out, ran.err));
    }

    /// A switch or a new context while no Space is managed prints the
    /// reactor's reason and exits with status 1.
    #[test]
    fn a_command_while_no_space_is_managed_fails() {
        let reason = "No Space is managed right now";
        for args in [
            &["switch", "Comms"][..],
            &["create", "Work"],
            &["everything"],
        ] {
            let ran = run_with_results(args, vec![Response::Error(reason.into())]);
            assert_eq!(
                (1, "", format!("{reason}\n")),
                (ran.status, &*ran.out, ran.err),
                "{args:?}"
            );
        }
    }

    /// A command whose result doesn't come within about a second says that
    /// Sugarglider did not confirm it, and exits with status 1.
    #[test]
    fn a_command_without_a_result_in_a_second_fails() {
        for args in [
            &["switch", "Comms"][..],
            &["create", "Work"],
            &["everything"],
        ] {
            let ran = run_with_results(args, vec![Response::Pending]);

            assert_eq!(
                (1, "", "Sugarglider did not confirm the command\n"),
                (ran.status, &*ran.out, &*ran.err),
                "{args:?}"
            );
            assert_eq!(50, ran.asks());
            assert_eq!(Duration::from_secs(1), ran.pauses.iter().sum::<Duration>());
        }
    }

    /// A reply to the ask for a result that doesn't fit fails with status 1.
    #[test]
    fn a_result_that_does_not_fit_fails() {
        let ran = run_with_results(&["everything"], vec![Response::Contexts(snapshot())]);

        assert_eq!((1, ""), (ran.status, &*ran.out));
        assert!(
            ran.err.starts_with("Unexpected reply from Sugarglider: "),
            "{}",
            ran.err
        );
        assert_eq!(1, ran.asks());
    }

    /// Failures print the reason to stderr and exit with status 1.
    #[test]
    fn failures_print_the_reason_and_exit_with_1() {
        let error = Response::Error("No context has the number 3".into());
        let not_running = run_raw(&["list"], None);
        let old_server = run_raw(&["list"], Some(Vec::new()));
        let garbage = run_raw(&["current"], Some(b"Nonsense".to_vec()));
        let out_of_range = run_with(&["switch", "12"], Response::Success);
        for (ran, reason) in [
            (
                run_with(&["switch", "3"], error),
                "No context has the number 3\n",
            ),
            (not_running, "Sugarglider isn't running.\n"),
            (
                old_server,
                "The running Sugarglider doesn't support this command. Restart it.\n",
            ),
            (
                out_of_range,
                "Context numbers go from 1 to 9, not 12. For a context with that name, add \
                 --name.\n",
            ),
            (
                run_with(&["list"], Response::Success),
                "Unexpected reply from Sugarglider: Success\n",
            ),
        ] {
            assert_eq!((1, "", reason), (ran.status, &*ran.out, &*ran.err));
        }
        let newer = run_raw(&["everything"], Some(b"Queued(3)".to_vec()));
        for ran in [garbage, newer] {
            assert_eq!((1, ""), (ran.status, &*ran.out));
            assert!(
                ran.err.starts_with(
                    "Could not read the reply from Sugarglider, which may be another version. \
                     Restart it. ("
                ),
                "{}",
                ran.err
            );
        }
    }

    /// A number out of range fails before anything is sent.
    #[test]
    fn a_bad_number_sends_nothing() {
        assert!(run_with(&["switch", "0"], Response::Success).requests.is_empty());
    }

    /// The requests of Sugarglider before contexts, as an older server reads
    /// them.
    #[derive(Serialize, Deserialize, Debug)]
    enum RequestBeforeContexts {
        Ping(String),
        UpdateConfig(Config),
        Service(ServiceRequestBeforeContexts),
        SetEnabled(bool),
    }

    #[derive(Serialize, Deserialize, Debug)]
    enum ServiceRequestBeforeContexts {
        Install,
        Uninstall,
    }

    /// The replies of Sugarglider before contexts.
    #[derive(Serialize, Deserialize, Debug)]
    enum ResponseBeforeContexts {
        Pong(String),
        Success,
    }

    /// How a server from before contexts answers a message: with nothing
    /// when it can't read the request.
    fn older_server_reply(message: &[u8]) -> Vec<u8> {
        let reply = match ron::de::from_bytes::<RequestBeforeContexts>(message) {
            Ok(RequestBeforeContexts::Ping(text)) => {
                ResponseBeforeContexts::Pong(text.chars().rev().collect())
            }
            Ok(_) => ResponseBeforeContexts::Success,
            Err(_) => return Vec::new(),
        };
        ron::ser::to_string(&reply).unwrap().into_bytes()
    }

    struct OlderServer {
        messages: usize,
    }

    impl Transport for &mut OlderServer {
        fn request(&mut self, message: &[u8]) -> Result<Vec<u8>, SendError> {
            self.messages += 1;
            Ok(older_server_reply(message))
        }
    }

    /// Every subcommand, with each output form.
    const EVERY_SUBCOMMAND: [&[&str]; 19] = [
        &["list"],
        &["list", "--json"],
        &["current"],
        &["current", "--json"],
        &["create", "Client work"],
        &["switch", "2"],
        &["switch", "cli"],
        &["switch", "--name", "2024"],
        &["add", "Comms"],
        &["add", "--name", "3"],
        &["everything"],
        &["previous"],
        &["move", "Comms"],
        &["remove"],
        &["rename", "Comms", "Client work"],
        &["delete", "Comms"],
        &["number", "Comms", "3"],
        &["pin"],
        &["forget", "Comms", "0"],
    ];

    /// I4. A server from before contexts can't read any context request and
    /// replies with nothing. Every subcommand then says to restart it, on
    /// stderr, and exits with status 1.
    #[test]
    fn every_subcommand_asks_to_restart_an_older_server() {
        for args in EVERY_SUBCOMMAND {
            let command = parse(args).unwrap();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut server = OlderServer { messages: 0 };
            let status = run(&command, || Some(&mut server), &mut out, &mut err);
            assert_eq!(
                (1, "", format!("{OLD_SERVER}\n")),
                (
                    status,
                    &*String::from_utf8(out).unwrap(),
                    String::from_utf8(err).unwrap()
                ),
                "{args:?}"
            );
            assert_eq!(1, server.messages, "{args:?}");
        }
        assert_eq!(
            "The running Sugarglider doesn't support this command. Restart it.",
            OLD_SERVER
        );
    }

    /// I4, through a real message port. A server from before contexts
    /// returns no data, and the client reads that as an empty reply, not as
    /// a failure to reach the server.
    #[test]
    fn an_older_server_behind_a_real_port_gets_the_restart_message() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let name = format!("com.test.sugarglider.context_cli.{}.{nanos}", std::process::id());
        let messages = Arc::new(Mutex::new(0));
        let counter = messages.clone();
        let _server = LocalMessagePort::new(&name, move |_, message| {
            *counter.lock().unwrap() += 1;
            older_server_reply(message)
        })
        .unwrap();
        let client = thread::spawn(move || {
            let command = parse(&["list"]).unwrap();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let connect = || RemoteMessagePort::new(&name).ok().map(|port| Client { port });
            let status = run(&command, connect, &mut out, &mut err);
            (
                status,
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        });
        let start = Instant::now();
        while !client.is_finished() && start.elapsed() < Duration::from_secs(5) {
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.05, false);
        }

        let (status, out, err) = client.join().unwrap();
        assert_eq!((1, "", format!("{OLD_SERVER}\n")), (status, &*out, err));
        assert_eq!(1, *messages.lock().unwrap());
    }

    /// A server that can't be reached.
    struct Unreachable(fn() -> SendError);

    impl Transport for Unreachable {
        fn request(&mut self, _: &[u8]) -> Result<Vec<u8>, SendError> {
            Err((self.0)())
        }
    }

    /// A server that runs but doesn't answer gives the reason on stderr and
    /// status 1.
    #[test]
    fn a_server_that_does_not_answer_fails_with_the_reason() {
        for (error, reason) in [
            (
                (|| SendError::Timeout) as fn() -> SendError,
                "Could not reach Sugarglider: Message send timed out\n",
            ),
            (
                || SendError::InvalidPort,
                "Could not reach Sugarglider: Message port is invalid\n",
            ),
            (
                || SendError::SendFailed(-2),
                "Could not reach Sugarglider: Message send failed with code -2\n",
            ),
        ] {
            for args in EVERY_SUBCOMMAND {
                let command = parse(args).unwrap();
                let (mut out, mut err) = (Vec::new(), Vec::new());
                let status = run(&command, || Some(Unreachable(error)), &mut out, &mut err);
                assert_eq!(
                    (1, "", reason),
                    (
                        status,
                        &*String::from_utf8(out).unwrap(),
                        &*String::from_utf8(err).unwrap()
                    ),
                    "{args:?}"
                );
            }
        }
    }

    /// Every subcommand fails with the server's reason on stderr and status
    /// 1 when the feature is off, and prints nothing on stdout.
    #[test]
    fn every_subcommand_fails_with_the_reason_when_contexts_are_off() {
        let reason = "Contexts are off. Turn them on with enable = true under \
                      [settings.experimental.contexts] in the config file.";
        for args in EVERY_SUBCOMMAND {
            let ran = run_with(args, Response::Error(reason.into()));
            assert_eq!(
                (1, "", format!("{reason}\n")),
                (ran.status, &*ran.out, ran.err),
                "{args:?}"
            );
            assert_eq!(1, ran.requests.len());
        }
    }

    /// A reply that doesn't fit the subcommand fails with status 1.
    #[test]
    fn a_reply_that_does_not_fit_the_subcommand_fails() {
        let unexpected = "Unexpected reply from Sugarglider: ";
        for (args, reply, reason) in [
            (&["current"][..], Response::Success, "Success"),
            (&["list", "--json"], Response::Pong("x".into()), "Pong(\"x\")"),
            (&["create", "X"], Response::Pong("x".into()), "Pong(\"x\")"),
            (&["everything"], Response::Pong("x".into()), "Pong(\"x\")"),
            (
                &["forget", "Comms", "2"],
                Response::Pong("x".into()),
                "Pong(\"x\")",
            ),
        ] {
            let ran = run_with(args, reply);
            assert_eq!(
                (1, "", format!("{unexpected}{reason}\n")),
                (ran.status, &*ran.out, ran.err),
                "{args:?}"
            );
        }
        let ran = run_with(&["switch", "cli"], Response::Contexts(snapshot()));
        assert_eq!((1, ""), (ran.status, &*ran.out));
        assert!(ran.err.starts_with(unexpected), "{}", ran.err);
    }

    /// Two screens that show Everything, a context without a number, and
    /// no unsorted window.
    fn everything_on_two_screens() -> ContextsSnapshot {
        let mut snapshot = snapshot();
        snapshot.active = ContextKey::Everything;
        snapshot.screens = vec![
            ScreenContext {
                id: 1,
                shows: ContextKey::Everything,
            },
            ScreenContext {
                id: 2,
                shows: ContextKey::Everything,
            },
        ];
        snapshot.contexts[1].number = None;
        snapshot.contexts[1].apps.clear();
        snapshot.contexts[1].windows = 0;
        snapshot.unsorted.listed = false;
        snapshot.unsorted.windows = 0;
        snapshot
    }

    /// Runs the command and returns what it printed as JSON. It prints one
    /// JSON value and a newline on stdout, and nothing on stderr.
    fn printed_json(args: &[&str], snapshot: ContextsSnapshot) -> serde_json::Value {
        let ran = run_with(args, Response::Contexts(snapshot));
        assert_eq!((0, ""), (ran.status, &*ran.err));
        assert!(ran.out.ends_with("}\n"), "{}", ran.out);
        let mut values = serde_json::Deserializer::from_str(&ran.out).into_iter();
        let value = values.next().unwrap().unwrap();
        assert!(values.next().is_none(), "{}", ran.out);
        value
    }

    /// The JSON is the shape in the spec's "Command line" section when no
    /// named context is active: a context without a number has a null
    /// number, every screen is listed, and each names what it shows. Every
    /// member record is listed with the index that `forget` takes.
    #[test]
    fn list_json_under_everything_and_without_a_number() {
        assert_eq!(
            json!({
              "scope": "global",
              "active": "Everything",
              "screens": [
                { "id": 1, "active": "Everything" },
                { "id": 2, "active": "Everything" }
              ],
              "contexts": [
                { "id": 1, "name": "Comms", "number": 1, "active": false,
                  "apps": ["WhatsApp", "Microsoft Teams"], "windows": 2,
                  "members": [
                    { "record": 0, "app": "WhatsApp", "title": "WhatsApp",
                      "window": { "pid": 903, "idx": 9201 } },
                    { "record": 1, "app": "Microsoft Teams", "title": "Team",
                      "window": { "pid": 904, "idx": 9202 } },
                    { "record": 2, "app": "Mail", "title": "Inbox", "window": null }
                  ] },
                { "id": 2, "name": "Relax", "number": null, "active": false,
                  "apps": [], "windows": 0, "members": [] }
              ],
              "unsorted": 0
            }),
            printed_json(&["list", "--json"], everything_on_two_screens())
        );

        let mut unsorted = snapshot();
        unsorted.active = ContextKey::Unsorted;
        unsorted.screens[0].shows = ContextKey::Unsorted;
        let printed = printed_json(&["list", "--json"], unsorted);
        assert_eq!(json!("Unsorted"), printed["active"]);
        assert_eq!(json!([{ "id": 1, "active": "Unsorted" }]), printed["screens"]);
        assert_eq!(
            json!([false, false]),
            json!([
                printed["contexts"][0]["active"],
                printed["contexts"][1]["active"]
            ])
        );

        let mut none = ContextsSnapshot::off();
        none.enabled = true;
        none.screens = vec![ScreenContext {
            id: 1,
            shows: ContextKey::Everything,
        }];
        assert_eq!(
            json!({
              "scope": "global",
              "active": "Everything",
              "screens": [{ "id": 1, "active": "Everything" }],
              "contexts": [],
              "unsorted": 0
            }),
            printed_json(&["list", "--json"], none)
        );
    }

    /// `current --json` prints the snapshot shape with only the active
    /// context, or with none under Everything.
    #[test]
    fn current_json_prints_the_spec_shape_with_the_active_context() {
        assert_eq!(
            json!({
              "scope": "global",
              "active": "Comms",
              "screens": [{ "id": 1, "active": "Comms" }],
              "contexts": [
                { "id": 1, "name": "Comms", "number": 1, "active": true,
                  "apps": ["WhatsApp", "Microsoft Teams"], "windows": 2,
                  "members": [
                    { "record": 0, "app": "WhatsApp", "title": "WhatsApp",
                      "window": { "pid": 903, "idx": 9201 } },
                    { "record": 1, "app": "Microsoft Teams", "title": "Team",
                      "window": { "pid": 904, "idx": 9202 } },
                    { "record": 2, "app": "Mail", "title": "Inbox", "window": null }
                  ] }
              ],
              "unsorted": 3
            }),
            printed_json(&["current", "--json"], snapshot().current())
        );
        assert_eq!(
            json!({
              "scope": "global",
              "active": "Everything",
              "screens": [
                { "id": 1, "active": "Everything" },
                { "id": 2, "active": "Everything" }
              ],
              "contexts": [],
              "unsorted": 0
            }),
            printed_json(&["current", "--json"], everything_on_two_screens().current())
        );
    }

    /// While no screen shows a managed Space, as when Sugarglider is paused,
    /// `screens` is empty, and the top-level `active` still names the active
    /// context, Everything and Unsorted included.
    #[test]
    fn json_names_the_active_context_while_no_space_is_managed() {
        for active in [
            ContextKey::Named(id(1)),
            ContextKey::Unsorted,
            ContextKey::Everything,
        ] {
            let mut paused = snapshot();
            paused.active = active;
            paused.screens.clear();
            let name = paused.name(active).unwrap().to_string();
            for (args, reply) in [
                (&["list", "--json"][..], paused.clone()),
                (&["current", "--json"], paused.current()),
            ] {
                let printed = printed_json(args, reply);
                assert_eq!(json!(name), printed["active"], "{args:?}");
                assert_eq!(json!([]), printed["screens"], "{args:?}");
            }
        }
    }

    /// Names line up by characters, not bytes. A context with no open window
    /// shows "0 windows" and no apps, and the star marks Everything when it
    /// is active. No line ends in spaces.
    #[test]
    fn list_lines_up_names_by_characters_and_marks_everything() {
        let mut snapshot = everything_on_two_screens();
        snapshot.contexts[0].name = "Café".into();
        snapshot.contexts[0].number = Some(9);
        snapshot.contexts[0].apps = vec!["Zed".into()];
        snapshot.contexts[0].windows = 1;
        snapshot.contexts[1].name = "Ünïcode wörk".into();

        let ran = run_with(&["list"], Response::Contexts(snapshot));

        assert_eq!(
            "  9 Café          1 window  Zed\n\
            \x20   Ünïcode wörk  0 windows\n\
            *   Everything\n",
            ran.out
        );
        assert_eq!((0, ""), (ran.status, &*ran.err));
    }

    /// `current` names Unsorted when it is active.
    #[test]
    fn current_prints_unsorted() {
        let mut snapshot = snapshot();
        snapshot.active = ContextKey::Unsorted;
        snapshot.screens[0].shows = ContextKey::Unsorted;
        let ran = run_with(&["current"], Response::Contexts(snapshot.current()));
        assert_eq!((0, "Unsorted\n", ""), (ran.status, &*ran.out, &*ran.err));
    }

    /// The Raycast script exactly as the spec ships it.
    const SHIPPED_SCRIPT: &str = "\
#!/bin/bash
# @raycast.schemaVersion 1
# @raycast.title Switch Context
# @raycast.mode compact
# @raycast.packageName Sugarglider
# @raycast.argument1 { \"type\": \"text\", \"placeholder\": \"Context\" }
set -euo pipefail
/usr/local/bin/sugarglider context switch \"$1\" 2>&1
";

    /// M6. The Raycast script command is the one the spec ships, is
    /// executable, and is a valid bash script.
    #[test]
    fn the_switch_context_script_is_the_shipped_one_and_valid_bash() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("contrib/raycast/switch-context.sh");

        assert_eq!(SHIPPED_SCRIPT, fs::read_to_string(&script).unwrap());
        let mode = fs::metadata(&script).unwrap().permissions().mode();
        assert_ne!(0, mode & 0o111, "{} is not executable", script.display());

        let status = Command::new("bash").arg("-n").arg(&script).status().unwrap();
        assert!(status.success(), "bash -n failed for {}", script.display());
    }

    /// A query is sent as it was typed: a number from 1 to 9, with or
    /// without spaces and leading zeros, or any other text as a name.
    #[test]
    fn a_query_is_sent_as_a_number_or_as_the_text_typed() {
        for (query, reference) in [
            ("1", ContextRef::Number(1)),
            (" 3 ", ContextRef::Number(3)),
            ("007", ContextRef::Number(7)),
            ("Client work", ContextRef::Name("Client work".into())),
            (" cli ", ContextRef::Name(" cli ".into())),
            ("+3", ContextRef::Name("+3".into())),
            ("3.0", ContextRef::Name("3.0".into())),
            ("٣", ContextRef::Name("٣".into())),
            ("", ContextRef::Name(String::new())),
        ] {
            let ran = run_with(&["switch", query], Response::Success);
            assert_eq!((0, "", ""), (ran.status, &*ran.out, &*ran.err), "{query:?}");
            assert_eq!(vec![switch(reference)], ran.commands(), "{query:?}");
        }
        for query in ["00", "256", "99999999999"] {
            let ran = run_with(&["switch", query], Response::Success);
            assert_eq!(
                (
                    1,
                    "",
                    format!(
                        "Context numbers go from 1 to 9, not {query}. For a context with that \
                         name, add --name.\n"
                    )
                ),
                (ran.status, &*ran.out, ran.err)
            );
            assert!(ran.requests.is_empty());
        }
    }

    /// The name of a new context is sent as it was typed, and the reason
    /// the reactor refuses it goes to stderr.
    #[test]
    fn create_sends_the_name_and_prints_a_refusal() {
        let ran = run_with(&["create", " Café \"A\" "], Response::Success);
        assert_eq!((0, "", ""), (ran.status, &*ran.out, &*ran.err));
        assert_eq!(
            vec![ContextCommand::CreateContext(" Café \"A\" ".into())],
            ran.commands()
        );

        let reason = "\"Everything\" is a reserved name";
        let ran = run_with_results(&["create", "Everything"], vec![Response::Error(reason.into())]);
        assert_eq!((1, "", format!("{reason}\n")), (ran.status, &*ran.out, ran.err));
    }

    /// M5c. `sugarglider context add <query>` adds the focused window to a
    /// context.
    #[test]
    fn add_sends_one_command_for_the_focused_window() {
        let parsed = parse(&["add", "Comms"]);
        assert!(parsed.is_ok(), "{parsed:?}");
        let ran = run_with(&["add", "Comms"], Response::Success);
        assert_eq!((0, "", ""), (ran.status, &*ran.out, &*ran.err));
        assert_eq!(
            vec![ContextCommand::AddWindowToContext(ContextRef::Name(
                "Comms".into()
            ))],
            ran.commands()
        );
        let ran = run_with(&["add", "2"], Response::Success);
        assert_eq!(
            vec![ContextCommand::AddWindowToContext(ContextRef::Number(2))],
            ran.commands()
        );
    }

    /// A context whose name is a number, such as "2024" or "3", is reached
    /// with `--name`, which sends the text as a name. Without it, the
    /// command says to add it.
    #[test]
    fn a_name_made_of_digits_is_sent_with_name() {
        for (args, command) in [
            (
                &["switch", "--name", "2024"][..],
                switch(ContextRef::Name("2024".into())),
            ),
            (&["switch", "--name", "3"], switch(ContextRef::Name("3".into()))),
            (
                &["add", "--name", "2024"],
                ContextCommand::AddWindowToContext(ContextRef::Name("2024".into())),
            ),
        ] {
            let ran = run_with(args, Response::Success);
            assert_eq!((0, "", ""), (ran.status, &*ran.out, &*ran.err), "{args:?}");
            assert_eq!(vec![command], ran.commands(), "{args:?}");
        }

        let ran = run_with(&["switch", "2024"], Response::Success);
        assert_eq!(
            (
                1,
                "Context numbers go from 1 to 9, not 2024. For a context with that name, add \
                 --name.\n"
            ),
            (ran.status, &*ran.err)
        );
        assert!(ran.requests.is_empty());
    }
}
