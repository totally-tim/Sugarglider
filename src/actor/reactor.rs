// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Reactor's job is to maintain coherence between the system and model state.
//!
//! It takes events from the rest of the system and builds a coherent picture of
//! what is going on. It shares this with the layout actor, and reacts to layout
//! changes by sending requests out to the other actors in the system.

mod animation;
mod contexts;
mod contexts_snapshot;
mod create_context;
mod focus;
mod main_window;
mod membership;
mod parking;
mod quit;
mod replay;
mod switcher;

#[cfg(test)]
mod restore_snapshots;
#[cfg(test)]
mod testing;

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use std::{mem, thread};

use animation::{Animation, AnimationManager, Message as AnimationMessage};
use main_window::{FocusSource, MainWindowTracker, RaisedWindow};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use parking::{Parked, ProcessLookup};
use quit::PendingExit;
use redact::Secret;
use replay::LaunchState;
pub use replay::{Record, replay};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tokio::sync::mpsc;
use tracing::{Span, debug, error, info, instrument, trace, warn};

use super::mouse;
use crate::actor::app::{AppInfo, AppThreadHandle, Quiet, Request, WindowId, WindowInfo, pid_t};
use crate::actor::contexts_snapshot::{CommandResult, ContextsSnapshot, RequestId};
use crate::actor::contexts_store::{self, ContextsStore};
use crate::actor::layout::{
    self, DragUpdate, DropAction, LayoutCommand, LayoutEvent, LayoutManager, LayoutWindowInfo,
};
use crate::actor::parked_journal::ParkedJournal;
use crate::actor::raise::{self, RaiseManager, RaiseRequest};
use crate::actor::space_manager::SpaceManager;
use crate::actor::{group_bars, space_manager, status, window_server, wm_controller};
use crate::collections::{HashMap, HashSet};
use crate::config::Config;
use crate::log::{self, MetricsCommand};
use crate::model::NodeId;
use crate::model::contexts::{ContextId, Contexts, Query};
use crate::sys::app::Process;
use crate::sys::event::MouseState;
use crate::sys::executor::Executor;
use crate::sys::geometry::{CGRectDef, CGRectExt, SameAs, round_to_physical};
use crate::sys::screen::{CoordinateConverter, ScreenId, SpaceId};
use crate::sys::timer::Timer;
use crate::sys::window_server::{WindowServerId, WindowServerInfo, WindowsOnScreen};
use crate::ui::swift_bridge;

pub type Sender = crate::actor::Sender<Event>;
pub type Receiver = crate::actor::Receiver<Event>;

pub fn channel() -> (Sender, Receiver) {
    crate::actor::channel()
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug)]
pub enum Event {
    /// Physical display ids in the order of the next screen parameters event.
    DisplayIdsChanged(Vec<u32>),
    /// The screen layout, including resolution, changed. DisplayIdsChanged
    /// precedes it on startup.
    ///
    /// `frames` holds the visible frame of each screen, and `bounds` the full
    /// bounds of its display. The main screen is always first in both lists.
    /// `ids` names each display, main screen first; a recording made before
    /// M9 has none, and the screens then count from 1.
    ///
    /// See the `SpaceChanged` event for an explanation of the other parameters.
    ScreenParametersChanged {
        #[serde_as(as = "Vec<CGRectDef>")]
        frames: Vec<CGRect>,
        #[serde_as(as = "Vec<CGRectDef>")]
        #[serde(default)]
        bounds: Vec<CGRect>,
        #[serde(default)]
        ids: Vec<ScreenId>,
        spaces: Vec<Option<SpaceId>>,
        scale_factors: Vec<f64>,
        converter: CoordinateConverter,
        on_screen: WindowsOnScreen,
    },

    /// The current space changed.
    ///
    /// There is one SpaceId per screen in the last ScreenParametersChanged
    /// event. `None` in the SpaceId vec disables managing windows on that
    /// screen until the next space change.
    ///
    /// WindowsOnScreen is included to avoid doing two updates in rapid
    /// succession. If there are windows we know were destroyed in the new
    /// space we'll start rearranging things, only to do so again if we
    /// discover newly added windows.
    ///
    /// TODO: In the future WindowsOnScreenUpdated should include a mapping
    /// of SpaceId to window list, and Reactor would maintain a list per
    /// space. Then we can update the windows on screen for the space before
    /// sending SpaceChanged.
    SpaceChanged(Vec<Option<SpaceId>>, WindowsOnScreen),

    /// Sugarglider is about to stop managing these Spaces, because it is
    /// turned off or the user turned the Spaces off. Every window on them
    /// shows until the next space change.
    ShowEverythingOn(Vec<SpaceId>),

    /// All running apps at launch have been registered.
    StartupComplete,

    /// An application was launched. This event is also sent for every running
    /// application on startup.
    ///
    /// Both WindowInfo (accessibility) and WindowServerInfo are collected for
    /// any already-open windows when the launch event is sent. Since this
    /// event isn't ordered with respect to the Space events, it is possible to
    /// receive this event for a space we just switched off of.. FIXME. The same
    /// is true of WindowCreated events.
    ApplicationLaunched {
        pid: pid_t,
        info: AppInfo,
        #[serde(skip, default = "replay::deserialize_app_thread_handle")]
        handle: AppThreadHandle,
        is_frontmost: bool,
        main_window: Option<WindowId>,
        visible_windows: Vec<(WindowId, WindowInfo)>,
    },
    ApplicationTerminated(pid_t),
    ApplicationThreadTerminated(pid_t),
    ApplicationActivated(pid_t, Quiet),
    ApplicationDeactivated(pid_t),
    ApplicationGloballyActivated(pid_t),
    ApplicationGloballyDeactivated(pid_t),
    ApplicationMainWindowChanged(pid_t, Option<WindowId>, Quiet),
    /// An app the reactor asked to activate could not be made frontmost.
    ActivateFailed(pid_t),

    WindowsDiscovered {
        pid: pid_t,
        new: Vec<(WindowId, WindowInfo)>,
        known_visible: Vec<WindowId>,
    },
    WindowCreated(WindowId, WindowInfo, MouseState),

    /// Updated list of windows visible on screen from the window server.
    ///
    /// Sent after space changes, app launches, and window creation. When
    /// `pid` is set, only that app's windows changed.
    WindowsOnScreenUpdated {
        pid: Option<pid_t>,
        on_screen: WindowsOnScreen,
    },

    // TODO: Consider replacing with WindowsOnScreenUpdated.
    WindowBecameVisible(WindowId),
    WindowDestroyed(WindowId),
    /// The window's title changed. The title is recorded in the clear.
    WindowTitleChanged(
        WindowId,
        #[serde(serialize_with = "redact::expose_secret")] Secret<String>,
    ),
    WindowFrameChanged(
        WindowId,
        #[serde(with = "CGRectDef")] CGRect,
        TransactionId,
        Requested,
        Option<MouseState>,
    ),

    /// Left mouse button was released.
    ///
    /// Layout changes are suppressed while the button is down so that they
    /// don't interfere with drags. This event is used to update the layout in
    /// case updates were supressed while the button was down.
    ///
    /// FIXME: This can be interleaved incorrectly with the MouseState in app
    /// actor events.
    MouseUp,
    /// The mouse cursor moved over a new window. Only sent if focus-follows-
    /// mouse is enabled.
    ///
    /// The second field is the process the window server was routing keyboard
    /// events to when the mouse moved, if it could be read. It is read in the
    /// mouse actor so it describes the same moment as the mouse position.
    MouseMovedOverWindow(WindowServerId, #[serde(default)] Option<pid_t>),

    /// A raise request completed. Used by the raise manager to track when
    /// all raise requests in a sequence have finished.
    RaiseCompleted {
        window_id: WindowId,
        sequence_id: u64,
    },

    /// A raise request failed. None of its windows will be raised.
    ///
    /// `quiet` is the one the request was made with. A non-quiet request was
    /// meant to move the focus, so the layout has to be reconciled with the
    /// main window that is actually focused.
    RaiseRequestFailed {
        windows: Vec<WindowId>,
        sequence_id: u64,
        quiet: Quiet,
    },

    /// A raise sequence timed out. Used by the raise manager to clean up
    /// pending raises that took too long.
    RaiseTimeout {
        sequence_id: u64,
    },

    /// The raise manager sent the raise that focuses a sequence's window.
    /// The batches before it raise windows quietly; only from here can the
    /// sequence's failures and timeouts end a switch's wait.
    RaiseFocusSent {
        sequence_id: u64,
    },

    LeftMouseDown(
        #[serde(with = "crate::sys::geometry::CGPointDef")] objc2_core_foundation::CGPoint,
        /// The window at the click point, if any. Used to detect clicks on
        /// non-managed windows (like floating panels or Sugarglider's own windows).
        Option<WindowServerId>,
    ),
    LeftMouseDragged(
        #[serde(with = "crate::sys::geometry::CGPointDef")] objc2_core_foundation::CGPoint,
    ),

    ScrollWheel {
        delta_x: f64,
        delta_y: f64,
        alt_held: bool,
    },

    Command(Command),
    /// A context command from the command line. The reactor publishes its
    /// result in the contexts snapshot under the request's id.
    ContextCommandRequested(RequestId, ContextCommand),
    ConfigChanged(Arc<Config>),

    /// The contexts a config reload read when it turned contexts on. A
    /// recording keeps them, so a replay applies the same contexts.
    ContextsRead(Box<Contexts>),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Requested(pub bool);

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Command {
    Layout(LayoutCommand),
    Metrics(MetricsCommand),
    Reactor(ReactorCommand),
    Context(ContextCommand),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ReactorCommand {
    Debug,
    Serialize,
    SaveAndExit,
}

/// Commands that switch between contexts, the named window sets.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ContextCommand {
    /// Shows the context's windows in its layout and parks every other window.
    SwitchContext(ContextRef),
    /// Shows every window in each Space's normal layout.
    ShowEverything,
    /// Switches back to the context used before the current one.
    PreviousContext,
    /// Adds the focused window to the context. It shows where it is until
    /// the next switch.
    AddWindowToContext(ContextRef),
    /// Moves the focused window out of the active context and into this one.
    MoveWindowToContext(ContextRef),
    /// Removes the focused window from the active context.
    RemoveWindowFromContext,
    /// Makes the focused window a member of every context, or stops that.
    ToggleWindowPinned,
    /// Creates a context with this name, whose members are the windows that
    /// show on the visible Spaces, and switches to it.
    CreateContext(String),
    /// Opens the switcher panel, or closes it when it is open.
    OpenContextSwitcher,
    /// Adds a specific window to the context, or the focused window when
    /// `window` is absent in a key binding.
    AddWindow {
        window: Option<WindowId>,
        context: ContextRef,
    },
    /// Moves a specific window out of the active context and into this one.
    MoveWindow {
        window: Option<WindowId>,
        context: ContextRef,
    },
    /// Pins or unpins a specific window.
    TogglePinned { window: Option<WindowId> },
    /// Creates a context from exactly these windows and switches to it.
    CreateContextFromWindows {
        name: String,
        windows: Vec<WindowId>,
    },
    /// Changes a context's member windows and gone-window records.
    EditContext {
        context: ContextRef,
        add: Vec<WindowId>,
        remove: Vec<WindowId>,
        remove_records: Vec<RecordRef>,
    },
    /// Renames a context (R4).
    RenameContext { context: ContextRef, name: String },
    /// Gives a context a number from 1 to 9, away from the context that
    /// holds it (R5).
    SetContextNumber { context: ContextRef, number: u8 },
    /// Deletes a context (R6). Its windows stay open, and the ones that
    /// were only in it become unsorted.
    DeleteContext(ContextRef),
    /// Changes a context's members: removes the records of closed windows,
    /// removes windows at once, and adds windows for the next switch (R37).
    EditContextMembers {
        context: ContextRef,
        #[serde(default)]
        add: Vec<WindowId>,
        #[serde(default)]
        remove: Vec<WindowId>,
        #[serde(default)]
        remove_records: Vec<RecordRef>,
    },
    /// Removes the member record that `record` names, whose window is gone
    /// (R23). The record's app and title must still match, so a list that
    /// shifted since the client read it can't remove another record.
    RemoveRecord {
        context: ContextRef,
        record: RecordRef,
    },
}

/// Names a member record of a context in an edit command: its index in
/// `Context.members`, and the app and title a client read there. The
/// reactor removes the record only while all three still match, because
/// the record can change while the switcher is open (R23).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RecordRef {
    pub record: usize,
    pub app: String,
    pub title: String,
}

/// Names a context in a command.
///
/// A bare integer is always a number from 1 to 9, and a string is a name. An
/// id is tagged, `{ id = 7 }` in TOML and `Id(7)` in RON.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(from = "ContextRefRepr", into = "ContextRefRepr")]
pub enum ContextRef {
    Number(u8),
    Name(String),
    Id(ContextId),
}

impl ContextRef {
    /// The reference as the contexts model resolves it.
    pub fn query(&self) -> Query<'_> {
        match self {
            ContextRef::Number(number) => Query::Number(*number),
            ContextRef::Name(name) => Query::Name(name),
            ContextRef::Id(id) => Query::Id(*id),
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
enum ContextRefRepr {
    Number(u8),
    Name(String),
    Tagged(TaggedContextRef),
}

#[derive(Serialize, Deserialize, Clone)]
enum TaggedContextRef {
    #[serde(rename = "id", alias = "Id")]
    Id(ContextId),
}

impl From<ContextRefRepr> for ContextRef {
    fn from(repr: ContextRefRepr) -> Self {
        match repr {
            ContextRefRepr::Number(number) => ContextRef::Number(number),
            ContextRefRepr::Name(name) => ContextRef::Name(name),
            ContextRefRepr::Tagged(TaggedContextRef::Id(id)) => ContextRef::Id(id),
        }
    }
}

impl From<ContextRef> for ContextRefRepr {
    fn from(reference: ContextRef) -> Self {
        match reference {
            ContextRef::Number(number) => ContextRefRepr::Number(number),
            ContextRef::Name(name) => ContextRefRepr::Name(name),
            ContextRef::Id(id) => ContextRefRepr::Tagged(TaggedContextRef::Id(id)),
        }
    }
}

/// Tracks a potential title bar drag for drag-to-rearrange.
/// We track whether macOS actually moved the window to distinguish between
/// clicks on floating windows (like preferences) and actual title bar drags.
struct TitleBarDrag {
    wid: WindowId,
    node: NodeId,
    frame_changed: bool,
    /// Original window frames before drag, for preview restoration.
    original_frames: HashMap<WindowId, CGRect>,
    /// Last action applied as preview (to detect changes).
    last_preview_action: Option<DropAction>,
    /// Last known mouse position for zone detection.
    last_mouse_position: Option<CGPoint>,
}

pub struct Reactor {
    config: Arc<Config>,
    apps: HashMap<pid_t, AppState>,
    layout: LayoutManager,
    /// One-shot frame targets requested by layout transitions. They are merged
    /// into the next animation alongside the continuously calculated layout.
    pending_frame_overrides: HashMap<WindowId, CGRect>,
    windows: HashMap<WindowId, WindowState>,
    window_server_info: HashMap<WindowServerId, WindowServerInfo>,
    window_ids: HashMap<WindowServerId, WindowId>,
    visible_windows: HashSet<WindowServerId>,
    screens: Vec<Screen>,
    active_screen_idx: Option<u16>,
    main_window_tracker: MainWindowTracker,
    in_drag: bool,
    /// Window being dragged by title bar for drag-to-rearrange.
    /// Tracks the window ID, node, and whether we received a frame change event
    /// confirming macOS actually moved the window.
    title_bar_drag: Option<TitleBarDrag>,
    /// The window the user is currently resizing with the mouse, if any.
    ///
    /// We don't write frames to this window until the resize ends, since a
    /// write mid-drag fights the user's mouse.
    resizing_window: Option<WindowId>,
    /// Recent attempts to write a frame to a window, used to stop fighting apps
    /// that move their windows back.
    frame_attempts: HashMap<WindowId, FrameAttempt>,
    record: Record,
    raise_manager_tx: raise::Sender,
    /// The sequence id of the last raise request.
    raise_sequence: u64,
    animation_tx: Option<animation::Sender>,
    mouse_tx: Option<mouse::Sender>,
    status_tx: Option<status::Sender>,
    group_indicators_tx: group_bars::Sender,
    /// Debug overlay showing drop zones for all windows.
    debug_drop_zones_visible: bool,
    /// Whether all apps have been registered at startup.
    startup_complete: bool,
    /// Windows that accessibility reported as closed but the window server might
    /// still show as visible. These are excluded from the layout even if the
    /// window server reports them. This handles Cmd+W closing where the window
    /// is hidden rather than destroyed.
    hidden_windows: HashSet<WindowServerId>,
    /// Windows parked in a screen corner.
    parked: HashMap<WindowId, Parked>,
    /// Windows a per-screen switch moved off a Space, with the Space they
    /// left, until their frame lands on the new screen. The Space they left
    /// doesn't take them back into its layouts until then.
    moving_away: HashMap<WindowId, SpaceId>,
    /// How many times each parked window was parked again since the last
    /// switch or Space change, to stop fighting an app that moves it back.
    repark_counts: HashMap<WindowId, u32>,
    /// Windows the reactor saw for the first time before the window server
    /// listed them, whose membership waits for the layer the list reports.
    pending_first_seen: HashSet<WindowId>,
    /// The frames of parked windows, on disk before the windows move.
    journal: ParkedJournal,
    /// Finds the process that has a pid now, to tell which journal entries
    /// belong to apps that are still running.
    process_lookup: ProcessLookup,
    /// Windows put back from parking whose next frame write goes out even
    /// when their known frame already matches it.
    forced_writes: HashSet<WindowId>,
    /// The user's contexts and the active context.
    contexts: Contexts,
    /// Where `contexts` are saved.
    contexts_store: ContextsStore,
    /// Whether `contexts_store` is still to be read. It is read when
    /// contexts are on.
    contexts_unread: bool,
    /// Names the boot of the Mac, saved with the contexts.
    boot_id: Option<String>,
    /// A quit that waits for parked windows to come back.
    pending_exit: Option<PendingExit>,
    /// Visible Spaces that show every window until the next space change,
    /// because Sugarglider is about to stop managing them.
    showing_everything: HashSet<SpaceId>,
    /// What the last switch waits for before focus from outside counts
    /// again.
    switch_guard: focus::SwitchGuard,
    /// A window that took focus before the reactor saw it, and how.
    focus_waiting: Option<(WindowId, FocusSource)>,
    /// Windows added to a context since the last switch. They count as
    /// members of the active context until the next switch.
    added_since_switch: HashSet<WindowId>,
    /// Where the layout is saved when Sugarglider quits. `None` saves nothing.
    layout_file: Option<PathBuf>,
    /// Ends the process with an exit code.
    exit: Box<dyn FnMut(i32) + Send>,
    /// The results of the last context commands from the command line,
    /// oldest first, which the contexts snapshot carries.
    command_results: VecDeque<CommandResult>,
    /// Commands whose parking writes still need AX readback before success.
    pending_parking_results: Vec<parking::PendingParkingResult>,
    /// The snapshot of the contexts published last.
    published_contexts: Option<Arc<ContextsSnapshot>>,
    /// Physical display ids in screen order, supplied by SpaceManager.
    display_ids: Vec<u32>,
    /// Shows the switcher panel with its payload, or closes it when it is
    /// open. Tests replace it.
    show_switcher: Box<dyn FnMut(String) + Send>,
    /// Hides the switcher panel if it is open. Tests replace it.
    hide_switcher: Box<dyn FnMut() + Send>,
    /// Where snapshots of the contexts go.
    publish_contexts: Box<dyn FnMut(Arc<ContextsSnapshot>) + Send>,
}

/// How many times in a row we write the same frame to a window before giving
/// up, and how long a pause resets the count.
const MAX_FRAME_ATTEMPTS: u32 = 5;
const FRAME_ATTEMPT_RESET: Duration = Duration::from_secs(2);

/// How many times a window is parked again since the last switch or Space
/// change before Sugarglider leaves it where its app put it. An app that
/// moves its parked window back every time would otherwise make Sugarglider
/// write the corner in a loop.
const MAX_REPARKS: u32 = 5;

/// The smallest size difference that counts as an app refusing to shrink a
/// window. Anything smaller is pixel rounding.
const MIN_SIZE_EPSILON: f64 = 1.0;

#[derive(Debug)]
struct FrameAttempt {
    target: CGRect,
    count: u32,
    last: Instant,
}

/// Keeps `frame` inside `screen`, growing it to `min_size` if the app will not
/// shrink below that.
///
/// A window that doesn't fit its slot overlaps its neighbors instead of
/// spilling past the screen edge. The remaining space is used first, so a
/// window at the right or bottom edge grows toward the middle of the screen.
fn fit_frame_to_screen(frame: CGRect, min_size: CGSize, screen: CGRect) -> CGRect {
    let size = CGSize::new(
        frame.size.width.max(min_size.width).min(screen.size.width),
        frame.size.height.max(min_size.height).min(screen.size.height),
    );
    let max_origin = CGPoint::new(screen.max().x - size.width, screen.max().y - size.height);
    CGRect::new(
        CGPoint::new(
            frame.origin.x.min(max_origin.x).max(screen.min().x),
            frame.origin.y.min(max_origin.y).max(screen.min().y),
        ),
        size,
    )
}

#[derive(Debug)]
struct AppState {
    #[allow(unused)]
    pub info: AppInfo,
    pub handle: AppThreadHandle,
}

/// Extra information about the event a layout response came from.
#[derive(Default)]
struct ResponseContext {
    /// The windows visible on screen, front to back, from a snapshot taken with
    /// the event. `None` if the event didn't come with one.
    ///
    /// Only valid for the event it arrived with: raising windows changes the
    /// order and the window server doesn't report the result.
    visible_window_order: Option<Vec<WindowServerId>>,
    /// Whether the event came from the mouse moving, in which case we don't
    /// warp the mouse to the newly focused window.
    from_mouse: bool,
}

#[derive(Copy, Clone, Debug)]
struct Screen {
    /// The display's id, which names the screen in the saved per-screen
    /// contexts. It stays the same while a display is connected.
    id: ScreenId,
    /// The part of the display that the menu bar and the Dock leave free.
    frame: CGRect,
    /// The whole display, including its menu bar and Dock.
    bounds: CGRect,
    space: Option<SpaceId>,
    scale_factor: f64,
}

/// A per-window counter that tracks the last time the reactor sent a request to
/// change the window frame.
#[derive(Default, Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransactionId(u32);

#[derive(Debug)]
struct WindowState {
    title: Secret<String>,
    /// The last known frame of the window. Always includes the last write.
    ///
    /// This value only updates monotonically with respect to writes; in other
    /// words, we only accept reads when we know they come after the last write.
    frame_monotonic: CGRect,
    is_ax_standard: bool,
    is_resizable: bool,
    ax_role: String,
    ax_subrole: Option<String>,
    last_sent_txid: TransactionId,
    window_server_id: Option<WindowServerId>,
}

impl WindowState {
    #[must_use]
    fn next_txid(&mut self) -> TransactionId {
        self.last_sent_txid.0 += 1;
        self.last_sent_txid
    }
}

impl From<WindowInfo> for WindowState {
    fn from(info: WindowInfo) -> Self {
        WindowState {
            title: info.title,
            frame_monotonic: info.frame,
            is_ax_standard: info.is_standard,
            is_resizable: info.is_resizable,
            ax_role: info.ax_role,
            ax_subrole: info.ax_subrole,
            last_sent_txid: TransactionId::default(),
            window_server_id: info.sys_id,
        }
    }
}

impl Reactor {
    /// Spawn the reactor on a dedicated thread, co-running a `WindowServer` in
    /// the same executor. Use [`channel()`] to create reactor_tx.
    #[expect(clippy::too_many_arguments)]
    pub fn spawn(
        config: Arc<Config>,
        one_space: bool,
        layout: LayoutManager,
        record: Record,
        mouse_tx: mouse::Sender,
        status_tx: status::Sender,
        group_indicators_tx: group_bars::Sender,
        reactor_tx: Sender,
        events: Receiver,
        wm_tx: wm_controller::Sender,
        ws_tx: window_server::Sender,
        ws_rx: window_server::Receiver,
        sm_tx: space_manager::Sender,
        sm_rx: space_manager::Receiver,
        skylight_tx: window_server::SkylightSender,
    ) {
        thread::Builder::new()
            .name("reactor".to_string())
            .spawn(move || {
                let journal =
                    ParkedJournal::open(crate::config::parked_journal_file(), SystemTime::now());
                let mut reactor = Reactor::new(
                    config.clone(),
                    layout,
                    record,
                    group_indicators_tx.clone(),
                    journal,
                );
                reactor.open_contexts(
                    ContextsStore::new(crate::config::contexts_file()),
                    contexts_store::boot_id(),
                    SystemTime::now(),
                );
                reactor.record_launch_state();
                reactor.layout_file = Some(crate::config::restore_file());
                reactor.exit = Box::new(|code| std::process::exit(code));
                let contexts_status_tx = status_tx.clone();
                reactor.publish_contexts = Box::new(move |snapshot| {
                    crate::actor::contexts_snapshot::publish(snapshot.clone());
                    contexts_status_tx.send(status::Event::ContextsChanged(snapshot));
                });
                reactor.publish_contexts_snapshot();
                reactor.mouse_tx.replace(mouse_tx.clone());
                reactor.status_tx.replace(status_tx.clone());
                let space_manager = SpaceManager::new(
                    one_space,
                    config,
                    reactor_tx.clone(),
                    ws_tx,
                    wm_tx.clone(),
                    status_tx,
                    group_indicators_tx,
                    mouse_tx,
                );
                let window_server = window_server::WindowServer::new(sm_tx, wm_tx, skylight_tx);
                Executor::run(async move {
                    tokio::join!(
                        reactor.run(events, reactor_tx),
                        space_manager.run(sm_rx),
                        window_server.run(ws_rx),
                    );
                });
            })
            .unwrap();
    }

    pub fn new(
        config: Arc<Config>,
        mut layout: LayoutManager,
        mut record: Record,
        group_indicators_tx: group_bars::Sender,
        journal: ParkedJournal,
    ) -> Reactor {
        // FIXME: Remove apps that are no longer running from restored state.
        record.start(&config, &layout);
        layout.set_config(&config);
        let (raise_manager_tx, _rx) = mpsc::unbounded_channel();
        Reactor {
            config,
            apps: HashMap::default(),
            layout,
            pending_frame_overrides: HashMap::default(),
            windows: HashMap::default(),
            window_ids: HashMap::default(),
            window_server_info: HashMap::default(),
            visible_windows: HashSet::default(),
            screens: vec![],
            active_screen_idx: None,
            main_window_tracker: MainWindowTracker::default(),
            in_drag: false,
            title_bar_drag: None,
            resizing_window: None,
            frame_attempts: HashMap::default(),
            record,
            raise_manager_tx,
            raise_sequence: 0,
            animation_tx: None,
            mouse_tx: None,
            status_tx: None,
            group_indicators_tx: group_indicators_tx,
            debug_drop_zones_visible: false,
            startup_complete: false,
            hidden_windows: HashSet::default(),
            parked: HashMap::default(),
            moving_away: HashMap::default(),
            repark_counts: HashMap::default(),
            pending_first_seen: HashSet::default(),
            journal,
            process_lookup: Box::new(Process::with_pid),
            forced_writes: HashSet::default(),
            contexts: Contexts::new(),
            contexts_store: ContextsStore::in_memory(),
            contexts_unread: false,
            boot_id: None,
            pending_exit: None,
            showing_everything: HashSet::default(),
            switch_guard: Default::default(),
            focus_waiting: None,
            added_since_switch: HashSet::default(),
            layout_file: None,
            exit: Box::new(|code| info!(code, "Not quitting a reactor that has no exit")),
            command_results: VecDeque::new(),
            pending_parking_results: Vec::new(),
            published_contexts: None,
            display_ids: Vec::new(),
            show_switcher: Box::new(swift_bridge::show_context_switcher),
            hide_switcher: Box::new(swift_bridge::hide_context_switcher),
            publish_contexts: Box::new(|_| {}),
        }
    }

    /// Records the journal, the contexts, and the process checks read at
    /// launch, so that a replay of the recording starts from them.
    fn record_launch_state(&mut self) {
        let processes: Vec<(pid_t, Process)> = self
            .journal
            .entries()
            .iter()
            .map(|entry| (entry.pid, (self.process_lookup)(entry.pid)))
            .collect();
        let state = LaunchState {
            journal: self.journal.entries().to_vec(),
            contexts: (!self.contexts_unread).then(|| self.contexts.clone()),
            processes,
        };
        self.record.launch_state(&self.layout, &state);
    }

    pub async fn run(mut self, events: Receiver, events_tx: Sender) {
        let (raise_manager_tx, raise_manager_rx) = mpsc::unbounded_channel();
        self.raise_manager_tx = raise_manager_tx.clone();
        let (animation_tx, animation_rx) = mpsc::unbounded_channel();
        self.animation_tx = Some(animation_tx);

        let mouse_tx = self.mouse_tx.clone();
        let reactor_task = self.run_reactor_loop(events);
        let raise_manager_task = RaiseManager::run(raise_manager_rx, events_tx, mouse_tx);
        let animation_task = AnimationManager::run(animation_rx);

        let _ = tokio::join!(reactor_task, raise_manager_task, animation_task);
    }

    async fn run_reactor_loop(mut self, mut events: Receiver) {
        // TODO: Accessibility APIs may be too slow for 120Hz; consider screen-capture animation approach.
        let tick_interval = Duration::from_secs_f64(1.0 / 120.0);
        let mut tick_timer = Timer::manual();

        // Periodic timer to refresh visible windows. This catches windows that
        // are closed without sending proper notifications (e.g., Cmd+W in some
        // apps). We poll every 2 seconds as a fallback.
        let visibility_refresh_interval = Duration::from_secs(2);
        let mut visibility_timer = Timer::manual();
        visibility_timer.set_next_fire(visibility_refresh_interval);
        let mut switch_deadline_timer = Timer::manual();

        loop {
            let animating = self.layout.has_active_scroll_animation();
            let switch_deadline = self.next_switch_deadline();
            if let Some(deadline) = switch_deadline {
                switch_deadline_timer
                    .set_next_fire(deadline.saturating_duration_since(Instant::now()));
            }
            tokio::select! {
                event = events.recv() => {
                    let Some((span, event)) = event else { break };
                    let _guard = span.enter();
                    let was_animating = self.layout.has_active_scroll_animation();
                    self.handle_event(event);
                    if !was_animating && self.layout.has_active_scroll_animation() {
                        tick_timer.set_next_fire(Duration::ZERO);
                    }
                }
                _ = tick_timer.next(), if animating => {
                    self.layout.tick_viewports();
                    self.update_layout(&[], true);
                    if self.layout.has_active_scroll_animation() {
                        tick_timer.set_next_fire(tick_interval);
                    }
                }
                _ = visibility_timer.next() => {
                    // Periodically refresh visible windows to detect closed windows.
                    self.update_visible_windows();
                    self.exit_deadline_tick(Instant::now());
                    visibility_timer.set_next_fire(visibility_refresh_interval);
                }
                _ = switch_deadline_timer.next(), if switch_deadline.is_some() => {
                    let now = Instant::now();
                    self.parking_deadline_tick(now);
                    self.guard_deadline_tick(now);
                }
            }
        }
    }

    fn next_switch_deadline(&self) -> Option<Instant> {
        self.next_parking_deadline().into_iter().chain(self.next_guard_deadline()).min()
    }

    fn log_event(&self, event: &Event) {
        match event {
            // Record more noisy events as trace logs instead of debug.
            Event::WindowFrameChanged(..)
            | Event::MouseUp
            | Event::LeftMouseDown(..)
            | Event::LeftMouseDragged(_) => trace!(?event, "Event"),
            _ => debug!(?event, "Event"),
        }
    }

    fn handle_event(&mut self, event: Event) {
        // These come many times a second and change nothing that the
        // contexts snapshot holds, so no snapshot is built for them.
        let pointer = matches!(
            event,
            Event::MouseMovedOverWindow(..)
                | Event::LeftMouseDragged(_)
                | Event::ScrollWheel { .. }
        );
        self.on_event(event);
        self.exit_if_windows_are_back();
        if !pointer {
            self.publish_contexts_snapshot();
        }
    }

    fn on_event(&mut self, event: Event) {
        self.record.on_event(&event);
        self.log_event(&event);
        self.journal.retry_failed_write(Instant::now());
        let animation_focus_wids: Vec<WindowId> = Vec::new();
        let mut is_resize = false;
        let raised_window = self.main_window_tracker.handle_event(&event);
        // An ApplicationActivated that ends a switch's wait for Finder is the
        // switch's own activation, not the user's.
        let ends_finder_wait = match &event {
            Event::ApplicationActivated(pid, _) => self.switch_guard.finder == Some(*pid),
            _ => false,
        };
        match event {
            Event::DisplayIdsChanged(ids) => self.display_ids = ids,
            Event::ApplicationLaunched {
                pid,
                info,
                handle,
                visible_windows,
                is_frontmost: _,
                main_window: _,
            } => {
                // With contexts off, a title change must not reach the window
                // rules, so the app doesn't send one. An app that launches
                // while contexts are on is told to send them; one that
                // launches while they are off already keeps them off.
                if self.contexts_enabled() {
                    _ = handle.send(Request::TrackTitles(true));
                }
                self.apps.insert(pid, AppState { info, handle });
                self.on_windows_discovered(pid, visible_windows, vec![]);
            }
            Event::StartupComplete => {
                self.update_active_screen();
                self.reconcile_cold_scope();
                self.send_layout_event(LayoutEvent::AppsRunningUpdated(
                    self.apps.keys().copied().collect(),
                ));
                self.startup_complete = true;
                self.drop_journal_entries_of_ended_apps();
                // Don't force layout on startup - windows may already be in
                // correct positions from a previous run. Layout will be
                // enforced when something actually changes.
                if self.contexts_in_use() {
                    // The windows open at launch have rejoined their contexts,
                    // and the ones that must not show are parked. When the
                    // window that has the focus is one of them, the switch's
                    // focus step moves the focus off it.
                    self.apply_again_focusing_parked_main();
                }
            }
            Event::ApplicationTerminated(pid) => {
                if let Some(app) = self.apps.get_mut(&pid) {
                    _ = app.handle.send(Request::Terminate);
                }
                self.app_terminated(pid);
            }
            Event::ApplicationThreadTerminated(pid) => {
                self.app_terminated(pid);
                self.guarded_app_gone(pid);
                self.apps.remove(&pid);
                self.moving_away.retain(|wid, _| wid.pid != pid);
                self.forget_parked_app(pid);
                self.send_layout_event(LayoutEvent::AppClosed(pid));
            }
            Event::ApplicationActivated(pid, quiet) => {
                // Also handled by MainWindowTracker.
                if quiet == Quiet::No {
                    self.app_still_running(pid);
                }
                self.app_activated(pid);
            }
            Event::ActivateFailed(pid) => {
                // The app that no window could focus never took focus, so the
                // wait for its activation ends.
                self.app_activated(pid);
            }
            Event::ApplicationDeactivated(..)
            | Event::ApplicationGloballyActivated(..)
            | Event::ApplicationGloballyDeactivated(..) => {
                // Handled by MainWindowTracker.
            }
            Event::ApplicationMainWindowChanged(pid, ..) => {
                // Handled by MainWindowTracker.
                // Also refresh visible windows for this app, as the main window
                // change may have happened because the previous window was closed.
                if let Some(app) = self.apps.get(&pid) {
                    _ = app.handle.send(Request::GetVisibleWindows);
                }
            }
            Event::WindowsDiscovered { pid, new, known_visible } => {
                self.on_windows_discovered(pid, new, known_visible);
            }
            Event::WindowCreated(wid, window, mouse_state) => {
                // TODO: It's possible for a window to be on multiple spaces
                // or move spaces. (Add a test)
                // FIXME: We assume all windows are on the main screen.
                if let Some(wsid) = window.sys_id {
                    self.window_ids.insert(wsid, wid);
                }
                let first_seen = self.windows.insert(wid, window.clone().into()).is_none();
                self.app_still_running(wid.pid);
                if first_seen {
                    let decided = self.windows_first_seen(&[wid]);
                    self.focus_windows_seen(&decided);
                }
                if mouse_state == MouseState::Down {
                    self.in_drag = true;
                    // Suppress updates while left button is pressed in case
                    // a drag is in progress.
                }
            }
            Event::WindowsOnScreenUpdated { pid, on_screen } => match pid {
                Some(pid) => {
                    self.update_partial_window_server_info(on_screen);
                    // Notify the layout manager about visibility changes (e.g.,
                    // when a window is minimized or unminimized). The update
                    // that comes just before an app registers names none of
                    // its windows, so with contexts in use it is not sent:
                    // it would take the app's windows out of the layouts a
                    // restart restored before they can rejoin their contexts.
                    if self.apps.contains_key(&pid) || !self.contexts_in_use() {
                        self.send_visible_windows_to_layout(pid);
                        if self.startup_complete {
                            self.park_what_must_not_show(pid);
                        }
                    }
                }
                None => self.update_complete_window_server_info(on_screen),
            },
            Event::WindowBecameVisible(wid) => {
                if self.window_is_tracked(wid)
                    && let Some(window) = self.windows.get(&wid)
                    && let Some(frame) = self.layout_frame(wid)
                    && let Some(space) = self.best_space_for_window(&frame)
                    && self.reaches_layout(space, wid)
                    && let Some(info) = self.layout_window_info(wid)
                {
                    // Check if there's already a visible window from the same app
                    // with the same frame (indicating this is a tab). If so, don't
                    // add - let the existing window represent this position.
                    // Parked windows share a corner without being tabs.
                    let frame_key = Self::frame_key(&window.frame_monotonic);
                    let dominated_by_existing = self.visible_windows.iter().any(|wsid| {
                        self.window_ids.get(wsid).is_some_and(|other_wid| {
                            *other_wid != wid
                                && other_wid.pid == wid.pid
                                && !self.parked.contains_key(other_wid)
                                && self.windows.get(other_wid).is_some_and(|other_window| {
                                    Self::frame_key(&other_window.frame_monotonic) == frame_key
                                })
                        })
                    });
                    if !dominated_by_existing {
                        self.send_layout_event(LayoutEvent::WindowAdded(space, wid, info));
                    }
                }
            }
            Event::WindowDestroyed(wid) => {
                self.layout.cancel_interactive_state();
                self.in_drag = false;
                self.resizing_window = None;
                // Clean up hidden_windows tracking for this window.
                if let Some(wsid) =
                    self.window_ids.iter().find(|(_, w)| **w == wid).map(|(wsid, _)| *wsid)
                {
                    self.hidden_windows.remove(&wsid);
                }
                // Check if another window will take this window's place (tab sibling)
                // before removing it from self.windows. Parked windows share a
                // corner without being tabs.
                let dominated_by_sibling = !self.parked.contains_key(&wid)
                    && self
                        .windows
                        .get(&wid)
                        .map(|w| {
                            let frame_key = Self::frame_key(&w.frame_monotonic);
                            self.windows.iter().any(|(other_wid, other_window)| {
                                *other_wid != wid
                                    && other_wid.pid == wid.pid
                                    && !self.parked.contains_key(other_wid)
                                    && Self::frame_key(&other_window.frame_monotonic) == frame_key
                            })
                        })
                        .unwrap_or(false);
                let window = self.windows.remove(&wid);
                if window.is_none() {
                    warn!("Got destroyed event for unknown window {wid:?}");
                }
                self.pending_first_seen.remove(&wid);
                self.window_closed(wid);
                self.guarded_window_gone(wid);
                self.frame_attempts.remove(&wid);
                self.moving_away.remove(&wid);
                self.forget_parked_window(wid, window.and_then(|window| window.window_server_id));
                // Only send WindowRemoved if no sibling will take its place.
                // For tabs, the sibling window already represents this position.
                if !dominated_by_sibling {
                    self.send_layout_event(LayoutEvent::WindowRemoved(wid));
                }
            }
            Event::WindowTitleChanged(wid, title) => self.title_changed(wid, title),
            Event::WindowFrameChanged(wid, new_frame, last_seen, requested, mouse_state) => {
                if mouse_state == Some(MouseState::Up) {
                    // The button is up, so any resize we were holding off on is
                    // over, even if we never saw the MouseUp event.
                    self.resizing_window = None;
                }
                if !requested.0 && self.parked.contains_key(&wid) {
                    debug!(
                        ?wid,
                        ?new_frame,
                        "Keeping a parked window's frame change out of the layout"
                    );
                    self.observe_parked(wid, new_frame, last_seen);
                    if self.contexts_enabled() && self.pending_exit.is_none() {
                        // The app may have moved the window out of its corner.
                        self.repark_moved_windows(false);
                    }
                    return;
                }
                if let Some(left) = self.moving_away.get(&wid).copied()
                    && self.best_space_for_window(&new_frame) != Some(left)
                {
                    // A window a per-screen switch moved has arrived when its
                    // frame is no longer on the Space it left.
                    self.moving_away.remove(&wid);
                }
                let sent_txid = self.windows.get(&wid).unwrap().last_sent_txid;
                if last_seen != sent_txid {
                    // Ignore events that happened before the last time we
                    // changed the size or position of this window. Otherwise
                    // we would update the layout model incorrectly.
                    debug!(?last_seen, ?sent_txid, "Ignoring resize");
                    return;
                }
                if requested.0 && !self.park_write_echoed(wid, new_frame, last_seen) {
                    return;
                }
                let window = self.windows.get_mut(&wid).unwrap();
                // The window is at this size now, so its minimum cannot be
                // larger. This also corrects a stale minimum after the app
                // becomes willing to shrink again.
                self.layout.relax_window_min_size(wid, new_frame.size);
                if requested.0 {
                    // An app may refuse to shrink a window below its minimum
                    // size, in which case the frame comes back larger than we
                    // asked for. Remember the size so the layout can give the
                    // window enough room instead of letting it spill off
                    // screen. Correcting the model directly would cause
                    // feedback loops, so only the minimum is kept.
                    let requested_size = window.frame_monotonic.size;
                    let min_size = CGSize::new(
                        if new_frame.size.width > requested_size.width + MIN_SIZE_EPSILON {
                            new_frame.size.width
                        } else {
                            0.0
                        },
                        if new_frame.size.height > requested_size.height + MIN_SIZE_EPSILON {
                            new_frame.size.height
                        } else {
                            0.0
                        },
                    );
                    if min_size.width > 0.0 || min_size.height > 0.0 {
                        let before = self.layout.window_min_size(wid);
                        self.layout.note_window_min_size(wid, min_size);
                        if self.layout.window_min_size(wid) != before && !self.in_drag {
                            // The layout now knows the window needs more room,
                            // so recalculate the frames. During a drag the
                            // correction waits for the end of the drag.
                            self.update_layout(&[], true);
                        }
                    }
                    self.observe_parked(wid, new_frame, last_seen);
                    self.confirm_unparked(wid, new_frame);
                    self.frame_write_echoed(wid);
                    return;
                }
                let old_frame = mem::replace(&mut window.frame_monotonic, new_frame);
                if old_frame == new_frame {
                    return;
                }
                // Track that the window being title-bar-dragged actually moved
                // and compute live preview if enabled
                //
                // Copy screen data first to avoid borrow conflicts
                let screen_data = self.active_screen().copied();
                let live_preview_enabled = self.config.settings.drag_drop.live_preview;

                let titlebar_preview_update = if let Some(ref mut drag) = self.title_bar_drag {
                    if drag.wid == wid {
                        drag.frame_changed = true;

                        // Live preview for title bar drags
                        if live_preview_enabled {
                            if let Some(screen) = screen_data {
                                if let Some(space) = screen.space {
                                    // Use tracked mouse position for zone detection,
                                    // fall back to window center if not available
                                    let position = drag.last_mouse_position.unwrap_or(CGPoint {
                                        x: new_frame.origin.x + new_frame.size.width / 2.0,
                                        y: new_frame.origin.y + new_frame.size.height / 2.0,
                                    });

                                    // Extract data before mutable borrow
                                    let source_wid = drag.wid;
                                    let last_action = drag.last_preview_action;
                                    let original_frames: Vec<_> = drag
                                        .original_frames
                                        .iter()
                                        .map(|(&w, &f)| (w, f))
                                        .collect();

                                    Some((
                                        source_wid,
                                        position,
                                        space,
                                        screen,
                                        last_action,
                                        original_frames,
                                    ))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                // Process preview update after releasing the borrow on title_bar_drag
                if let Some((source_wid, position, space, screen, last_action, original_frames)) =
                    titlebar_preview_update
                {
                    let preview_result = self.layout.compute_titlebar_preview(
                        space,
                        source_wid,
                        position,
                        screen.frame,
                        &self.config,
                    );

                    match (&preview_result, &last_action) {
                        (Some((action, preview_frames)), last) if last.as_ref() != Some(action) => {
                            // Action changed, animate to new preview
                            trace!(?action, "titlebar drag preview changed");
                            if let Some(ref mut drag) = self.title_bar_drag {
                                drag.last_preview_action = Some(*action);
                            }
                            self.animate_to_preview(preview_frames, screen.scale_factor);
                        }
                        (None, Some(_)) => {
                            // Action became None, restore original frames
                            trace!("titlebar drag restore original");
                            if let Some(ref mut drag) = self.title_bar_drag {
                                drag.last_preview_action = None;
                            }
                            self.animate_to_preview(&original_frames, screen.scale_factor);
                        }
                        _ => {
                            // No change
                        }
                    }
                }
                self.send_layout_event(LayoutEvent::WindowFrameChanged { wid, frame: new_frame });
                let old_screen = self.best_screen_idx_for_window(&old_frame);
                let new_screen = self.best_screen_idx_for_window(&new_frame);
                if let Some(old) = old_screen
                    && let Some(new) = new_screen
                    && old != new
                    && let Some(info) = self.layout_window_info(wid)
                {
                    // The window leaves the old Space's layouts whether or not
                    // the new Space's layout may take it.
                    let added =
                        self.screens[new].space.filter(|&space| self.reaches_layout(space, wid));
                    self.send_layout_event(LayoutEvent::WindowSpaceChanged {
                        wid,
                        added,
                        removed: self.screens[old].space,
                        info,
                        contexts_in_use: self.contexts_in_use(),
                    });
                }
                if old_frame.size != new_frame.size {
                    if mouse_state == Some(MouseState::Down) {
                        // A user-driven resize is a deliberate override, so it
                        // releases a size share lock on the window.
                        self.resizing_window = Some(wid);
                        self.layout.release_size_share(wid);
                    }
                    let screens = self
                        .screens
                        .iter()
                        .flat_map(|screen| Some((screen.space?, screen.frame)))
                        .collect::<Vec<_>>();
                    // This event is ignored if the window is not in the layout.
                    self.send_layout_event(LayoutEvent::WindowResized {
                        wid,
                        old_frame,
                        new_frame,
                        screens,
                    });
                    is_resize = true;
                } else if mouse_state == Some(MouseState::Down) {
                    self.in_drag = true;
                }
            }
            Event::ScreenParametersChanged {
                frames,
                bounds,
                ids,
                spaces,
                converter,
                scale_factors,
                on_screen,
            } => {
                info!("screen parameters changed");
                self.showing_everything.retain(|space| spaces.contains(&Some(*space)));
                let visible_window_order = on_screen.visible.clone();
                self.update_complete_window_server_info(on_screen);
                self.screens = frames
                    .into_iter()
                    .zip(spaces.clone())
                    .zip(scale_factors)
                    .enumerate()
                    .map(|(idx, ((frame, space), scale_factor))| Screen {
                        // A recording made before displays had ids numbers
                        // the screens from 1, like the snapshot does.
                        id: ids.get(idx).copied().unwrap_or_else(|| ScreenId::new(idx as u32 + 1)),
                        frame,
                        // Recordings made before displays reported their bounds
                        // have none, so the visible frame stands in.
                        bounds: bounds.get(idx).copied().unwrap_or(frame),
                        space,
                        scale_factor,
                    })
                    .collect();
                if self.screens.iter().all(|screen| screen.space.is_none()) {
                    self.hide_context_switcher();
                }
                let response = self.show_visible_spaces();
                if let Some(response) = response {
                    self.handle_layout_response_with_context(
                        response,
                        ResponseContext {
                            visible_window_order: Some(visible_window_order),
                            ..Default::default()
                        },
                    );
                    for space in self.screens.iter().flat_map(|screen| screen.space) {
                        self.layout.debug_tree_desc(space, "after event", false);
                    }
                }
                self.repark_moved_windows(false);
                self.update_active_screen();
                if self.startup_complete && self.reconcile_cold_scope() && self.contexts_in_use() {
                    self.apply_again_focusing_parked_main();
                }
                // FIXME: Update visible windows if space changed.
                // Forward the event to group_indicators. We serialize these
                // through the reactor instead of delivering directly from
                // wm_controller in order to eliminate possible races with other
                // events sent by the reactor.
                self.group_indicators_tx
                    .send(group_bars::Event::ScreenParametersChanged(spaces, converter));
            }
            Event::SpaceChanged(spaces, on_screen) => {
                let visible_window_order = on_screen.visible.clone();
                self.update_complete_window_server_info(on_screen);
                if spaces.len() != self.screens.len() {
                    warn!(
                        "Ignoring space change event: we have {} spaces, but {} screens",
                        spaces.len(),
                        self.screens.len()
                    );
                    return;
                }
                self.layout.cancel_interactive_state();
                self.in_drag = false;
                self.resizing_window = None;
                info!("space changed");
                self.showing_everything.clear();
                // A Space change is a fresh start for the limit on parking
                // windows again.
                self.repark_counts.clear();
                for (space, screen) in spaces.iter().zip(&mut self.screens) {
                    screen.space = *space;
                }
                if self.screens.iter().all(|screen| screen.space.is_none()) {
                    self.hide_context_switcher();
                }
                let response = self.show_visible_spaces();
                if let Some(response) = response {
                    self.handle_layout_response_with_context(
                        response,
                        ResponseContext {
                            visible_window_order: Some(visible_window_order),
                            ..Default::default()
                        },
                    );
                    for space in self.screens.iter().flat_map(|screen| screen.space) {
                        self.layout.debug_tree_desc(space, "after event", false);
                    }
                }
                if let Some(main_window) = self.main_window() {
                    let spaces = spaces.iter().copied().flatten().collect();
                    self.send_layout_event(LayoutEvent::WindowFocused(spaces, main_window));
                }
                self.update_active_screen();
                self.update_visible_windows();
            }
            Event::ShowEverythingOn(spaces) => self.show_everything_on(&spaces),
            Event::LeftMouseDown(point, window_at_point) => {
                if let Some(screen) = self.active_screen().copied()
                    && let Some(space) = screen.space
                {
                    // Check if the click is on a window that we don't manage as a tiled window.
                    // This includes: Sugarglider's own windows (preferences), floating windows,
                    // ignored windows, and windows from apps we don't track.
                    if let Some(wsid) = window_at_point {
                        // If the clicked window is not in our managed window list, ignore the drag
                        if !self.window_ids.contains_key(&wsid) {
                            return;
                        }
                        // If the clicked window is managed but floating, also ignore
                        if let Some(&wid) = self.window_ids.get(&wsid) {
                            if self.layout.floating_windows_in_space(space).contains(&wid) {
                                return;
                            }
                        }
                    }

                    if let Some((col, win, edges)) =
                        self.layout.hit_test_scroll_edges(space, point, screen.frame, &self.config)
                    {
                        self.layout.begin_interactive_resize(col, win, edges, point);
                        self.in_drag = true;
                    } else if let Some((wid, node)) =
                        self.layout.hit_test_scroll_window(space, point, screen.frame, &self.config)
                    {
                        self.layout.begin_interactive_move(space, wid, node, point);
                        self.in_drag = true;
                    } else if let Some((wid, node)) =
                        self.layout.hit_test_window(space, point, screen.frame, &self.config)
                    {
                        // Start drag-to-rearrange for any tiled window
                        if self.layout.begin_interactive_drag(
                            space,
                            wid,
                            node,
                            point,
                            screen.frame,
                            &self.config,
                        ) {
                            self.in_drag = true;
                        } else if self.config.settings.drag_drop.enable {
                            // Track for title bar drag-to-rearrange
                            // Cache original frames for live preview
                            let original_frames: HashMap<WindowId, CGRect> = self
                                .layout
                                .calculate_layout(space, screen.frame, &self.config)
                                .into_iter()
                                .filter(|(w, _)| *w != wid)
                                .collect();
                            self.title_bar_drag = Some(TitleBarDrag {
                                wid,
                                node,
                                frame_changed: false,
                                original_frames,
                                last_preview_action: None,
                                last_mouse_position: Some(point),
                            });
                            self.in_drag = true;
                        }
                    }
                }
            }
            Event::LeftMouseDragged(point) => {
                // Update mouse position for title bar drags
                if let Some(ref mut drag) = self.title_bar_drag {
                    drag.last_mouse_position = Some(point);
                }

                if let Some(&screen) = self.active_screen() {
                    if screen.space.is_some() {
                        if self.layout.update_interactive_resize(point, screen.frame) {
                            self.update_layout(&[], true);
                        } else if self.layout.update_interactive_move(
                            point,
                            screen.frame,
                            &self.config,
                        ) {
                            self.update_layout(&[], false);
                        } else {
                            let drag_update = self.layout.update_interactive_drag(
                                point,
                                screen.frame,
                                &self.config,
                            );
                            // Only animate preview if live_preview is enabled
                            if self.config.settings.drag_drop.live_preview {
                                match drag_update {
                                    DragUpdate::PreviewChanged {
                                        action, preview_frames, ..
                                    } => {
                                        trace!(?action, "drag preview changed");
                                        self.animate_to_preview(
                                            &preview_frames,
                                            screen.scale_factor,
                                        );
                                    }
                                    DragUpdate::RestoreOriginal { original_frames } => {
                                        trace!("drag restore original");
                                        self.animate_to_preview(
                                            &original_frames,
                                            screen.scale_factor,
                                        );
                                    }
                                    DragUpdate::NoChange => {}
                                }
                            }
                        }
                    }
                }
            }
            Event::MouseUp => {
                if self.layout.has_interactive_state() {
                    if let Some(&screen) = self.active_screen() {
                        if let Some(space) = screen.space {
                            self.layout.end_interactive_resize(space, screen.frame, &self.config);
                            self.layout.end_interactive_move(space, screen.frame, &self.config);
                            // Apply drag-to-rearrange action if any
                            if let Some((source_node, action)) =
                                self.layout.end_interactive_drag(space)
                            {
                                self.layout.apply_drop_action(space, source_node, action);
                                // TODO: Hide drop zone overlay via swift_bridge
                                self.update_layout(&[], false);
                            }
                        }
                    }
                }
                // Handle title bar drags for drag-to-rearrange.
                // Only proceed if we received WindowFrameChanged events for this window,
                // which confirms macOS actually moved it (not just a click on a floating window).
                if let Some(drag) = self.title_bar_drag.take() {
                    if drag.frame_changed {
                        // Use the action from live preview if available, otherwise compute it.
                        let action = if let Some(action) = drag.last_preview_action {
                            Some(action)
                        } else if let Some(window) = self.windows.get(&drag.wid)
                            && let Some(&screen) = self.active_screen()
                            && let Some(space) = screen.space
                        {
                            // Use tracked mouse position, fall back to window center
                            let current_frame = window.frame_monotonic;
                            let position = drag.last_mouse_position.unwrap_or(CGPoint {
                                x: current_frame.origin.x + current_frame.size.width / 2.0,
                                y: current_frame.origin.y + current_frame.size.height / 2.0,
                            });
                            self.layout.compute_drop_action_for_position(
                                space,
                                drag.wid,
                                position,
                                screen.frame,
                                &self.config,
                            )
                        } else {
                            None
                        };

                        if let Some(action) = action {
                            if let Some(&screen) = self.active_screen()
                                && let Some(space) = screen.space
                            {
                                self.layout.apply_drop_action(space, drag.node, action);
                                self.update_layout(&[], false);
                            }
                        }
                    }
                }
                self.in_drag = false;
                self.resizing_window = None;
                // Now re-check the layout.
            }
            Event::MouseMovedOverWindow(wsid, key_focus_pid) => {
                let Some(&wid) = self.window_ids.get(&wsid) else { return };
                let Some(window) = self.windows.get(&wid) else { return };
                let Some(to_space) = self.best_space_for_window(&window.frame_monotonic) else {
                    // The space is disabled.
                    return;
                };
                if !self.reaches_layout(to_space, wid) {
                    return;
                }
                let current_main = match (self.main_window_space(), self.main_window()) {
                    (Some(space), Some(id)) => Some((space, id)),
                    _ => None,
                };
                // Spotlight and similar apps can take keyboard focus without
                // activating, which leaves the main window unchanged. We don't
                // manage these windows and should avoid stealing focus from them.
                if let Some(key_focus_pid) = key_focus_pid
                    && current_main.is_some_and(|(_, main)| main.pid != key_focus_pid)
                {
                    debug!(
                        ?key_focus_pid,
                        ?current_main,
                        "Ignoring mouse move; keyboard focus is elsewhere"
                    );
                    return;
                }
                self.send_layout_event_with_context(
                    LayoutEvent::MouseMovedOverWindow {
                        over: (to_space, wid),
                        // TODO: Track focused window and use it here rather
                        // than main, to avoid stealing focus from a panel (of
                        // the main app; otherwise we would have returned above).
                        current_main,
                    },
                    ResponseContext {
                        from_mouse: true,
                        ..Default::default()
                    },
                );
            }
            Event::RaiseCompleted { window_id, sequence_id } => {
                let msg = raise::Event::RaiseCompleted { window_id, sequence_id };
                _ = self.raise_manager_tx.send((Span::current(), msg));
                self.raise_ended(sequence_id, Some(window_id));
            }
            Event::RaiseRequestFailed { windows, sequence_id, quiet } => {
                let msg = raise::Event::RaiseRequestFailed {
                    windows: windows.clone(),
                    sequence_id,
                };
                _ = self.raise_manager_tx.send((Span::current(), msg));
                self.raise_failed(sequence_id, &windows);
                if quiet == Quiet::No
                    && let Some(main_window) = self.main_window()
                {
                    // The raise didn't move the focus, so bring the layout
                    // selection back to the window that has it.
                    let spaces = self.screens.iter().flat_map(|screen| screen.space).collect();
                    self.send_layout_event(LayoutEvent::WindowFocused(spaces, main_window));
                }
            }
            Event::RaiseTimeout { sequence_id } => {
                let msg = raise::Event::RaiseTimeout { sequence_id };
                _ = self.raise_manager_tx.send((Span::current(), msg));
                self.raise_ended(sequence_id, None);
            }
            Event::RaiseFocusSent { sequence_id } => self.raise_started(sequence_id),
            Event::ScrollWheel { delta_x, delta_y, alt_held } => {
                if !self.config.settings.experimental.scroll.enable {
                    return;
                }
                // TODO: Make the modifier key configurable.
                if !alt_held {
                    return;
                }
                if let Some(&screen) = self.active_screen() {
                    if let Some(space) = screen.space {
                        let scroll_config = &self.config.settings.experimental.scroll;
                        let delta = if delta_x != 0.0 { delta_x } else { delta_y };
                        let response = self.layout.handle_scroll_wheel(
                            space,
                            delta,
                            &screen.frame,
                            scroll_config,
                        );
                        self.handle_layout_response(response);
                    }
                }
            }
            Event::Command(Command::Layout(cmd)) => {
                info!(?cmd);
                let animate = matches!(
                    cmd,
                    LayoutCommand::CleanUpSpace | LayoutCommand::ToggleWindowFloating
                );
                let visible_spaces =
                    self.screens.iter().flat_map(|screen| screen.space).collect::<Vec<_>>();
                // macOS can temporarily have no main window (for example after
                // clicking the desktop). Keep keyboard layout commands usable
                // by targeting the last active screen in that case.
                let command_space = self
                    .main_window_space()
                    .or_else(|| self.active_screen().and_then(|screen| screen.space));
                let response = self.layout.handle_command(command_space, &visible_spaces, cmd);
                self.handle_layout_response(response);
                if animate {
                    if let Some(status_tx) = &self.status_tx {
                        status_tx.send(status::Event::Animate);
                    }
                }
            }
            Event::Command(Command::Metrics(cmd)) => log::handle_command(cmd),
            Event::Command(Command::Context(cmd)) => {
                info!(?cmd);
                self.handle_context_command(cmd);
            }
            Event::ContextCommandRequested(request, cmd) => {
                info!(?request, ?cmd);
                let switches = matches!(
                    cmd,
                    ContextCommand::SwitchContext(_)
                        | ContextCommand::ShowEverything
                        | ContextCommand::PreviousContext
                        | ContextCommand::CreateContext(_)
                        | ContextCommand::CreateContextFromWindows { .. }
                );
                let before = self.pending_parking_writes();
                let result = self.run_context_command(cmd);
                if let Err(reason) = &result {
                    info!(?request, "The context command did nothing: {reason}");
                }
                match result {
                    Ok(()) => {
                        let writes = if switches {
                            self.pending_parking_writes()
                        } else {
                            self.new_parking_writes(&before)
                        };
                        if writes.is_empty() {
                            self.record_command_result(request, None);
                        } else {
                            self.pending_parking_results
                                .push(parking::PendingParkingResult { request, writes });
                        }
                    }
                    Err(reason) => self.record_command_result(request, Some(reason)),
                }
            }
            Event::Command(Command::Reactor(ReactorCommand::Debug)) => {
                for screen in &self.screens {
                    if let Some(space) = screen.space {
                        self.layout.debug_tree_desc(space, "", true);
                    }
                }
                // Toggle debug drop zone overlay
                self.debug_drop_zones_visible = !self.debug_drop_zones_visible;
                if self.debug_drop_zones_visible {
                    self.refresh_debug_drop_zones();
                    info!("Debug drop zones enabled");
                } else {
                    swift_bridge::hide_drop_zones();
                    info!("Debug drop zones disabled");
                }
            }
            Event::Command(Command::Reactor(ReactorCommand::Serialize)) => {
                println!("{}", self.layout.serialize_to_string());
            }
            Event::Command(Command::Reactor(ReactorCommand::SaveAndExit)) => {
                info!("SaveAndExit command received");
                self.save_and_exit(Instant::now());
            }
            Event::ConfigChanged(config) => {
                let contexts_were_enabled = self.contexts_enabled();
                let scope = self.config.settings.experimental.contexts.scope;
                self.layout.set_config(&config);
                self.config = config;
                if self.contexts_enabled() != contexts_were_enabled {
                    self.contexts_turned_on_or_off();
                } else if self.contexts_enabled()
                    && self.config.settings.experimental.contexts.scope != scope
                {
                    self.scope_changed(scope);
                }
            }
            Event::ContextsRead(contexts) => self.contexts_read(*contexts),
        }
        if let Some(RaisedWindow { wid: raised_window, source }) = raised_window {
            let spaces = self.screens.iter().flat_map(|screen| screen.space).collect();
            self.send_layout_event(LayoutEvent::WindowFocused(spaces, raised_window));
            self.update_active_screen();
            if !ends_finder_wait {
                self.focus_changed(raised_window, source);
            }
        }
        if !self.in_drag {
            self.update_layout(&animation_focus_wids, is_resize);
        }
    }

    fn update_complete_window_server_info(&mut self, on_screen: WindowsOnScreen) {
        for info in on_screen.info.iter().filter(|i| i.layer == 0) {
            let Some(&wid) = self.window_ids.get(&info.id) else {
                continue;
            };
            if let Some(parked) = self.parked.get_mut(&wid) {
                parked.observed = info.frame;
                continue;
            }
            let Some(window) = self.windows.get_mut(&wid) else {
                continue;
            };
            // Assume this update comes from after the last write. Typically the
            // window is on a different space than the one we're coming from
            // (unless it's on all spaces).
            //
            // TODO: It is still possible to have a race if we issued resizes on
            // this window that haven't completed yet (e.g. from an earlier
            // animation and a slow app). Consider having the app actor give us
            // updated locations on GetVisibleWindows instead.
            window.frame_monotonic = info.frame;
        }
        self.update_partial_window_server_info(on_screen);
    }

    fn update_partial_window_server_info(&mut self, on_screen: WindowsOnScreen) {
        // The on_screen snapshot always contains the complete list of visible
        // windows, even for partial (per-app) updates. Replace rather than
        // extend to avoid accumulating stale entries.
        self.visible_windows.clear();
        // Filter out windows that accessibility has reported as hidden (closed
        // with Cmd+W). The window server might still show them as visible, but
        // we should not include them in the layout.
        self.visible_windows.extend(
            on_screen.visible.into_iter().filter(|wsid| !self.hidden_windows.contains(wsid)),
        );
        self.window_server_info
            .extend(on_screen.info.into_iter().map(|info| (info.id, info)));
        // The windows waiting for their layer can now have their membership
        // decided, before the caller parks the ones that must not show.
        let decided = self.decide_pending_membership();
        self.focus_windows_seen(&decided);
    }

    fn should_compare_visible_window(&self, wsid: WindowServerId) -> bool {
        let Some(info) = self.window_server_info.get(&wsid) else {
            return false;
        };
        if info.layer != 0 {
            // TODO: Revisit this if LayoutManager starts managing floating windows.
            return false;
        }
        self.screens
            .iter()
            .filter(|screen| screen.space.is_some())
            .map(|screen| screen.frame.intersection(&info.frame).size)
            .any(|size| size.width > 0.0 && size.height > 0.0)
    }

    fn update_visible_windows(&mut self) {
        // TODO: Do this correctly/more optimally using CGWindowListCopyWindowInfo
        // (see notes for on_windows_discovered below).
        for app in self.apps.values_mut() {
            // Errors mean the app terminated (and a termination event
            // is coming); ignore.
            _ = app.handle.send(Request::GetVisibleWindows);
        }
    }

    fn on_windows_discovered(
        &mut self,
        pid: pid_t,
        new: Vec<(WindowId, WindowInfo)>,
        known_visible: Vec<WindowId>,
    ) {
        // Note that we rely on the window server info, not accessibility, to
        // tell us which windows are visible.
        //
        // The accessibility APIs report that there are no visible windows when
        // at a login screen, for instance, but there is not a corresponding
        // system notification to use as context. Even if there were, lining
        // them up with the responses we get from the app would be unreliable.
        //
        // We therefore do not let accessibility `.windows()` results remove
        // known windows from the visible list. Doing so incorrectly would cause
        // us to destroy the layout. We do wait for windows to become initially
        // known to accesibility before adding them to the layout, but that is
        // not generally problematic.
        //
        // HOWEVER, if accessibility reports at least one window for an app
        // (ruling out the login screen case), we can trust it to tell us which
        // windows are closed. This handles Cmd+W window closing where the
        // window server might still report the window briefly.
        //
        // TODO: Notice when returning from the login screen and ask again for
        // undiscovered windows.
        let first_seen: Vec<WindowId> = new
            .iter()
            .map(|&(wid, _)| wid)
            .filter(|wid| !self.windows.contains_key(wid))
            .collect();
        self.window_ids
            .extend(new.iter().flat_map(|(wid, info)| info.sys_id.map(|wsid| (wsid, *wid))));
        self.windows.extend(new.into_iter().map(|(wid, info)| (wid, info.into())));

        // If accessibility reports at least one window, use it to detect closed
        // windows. Mark them as hidden so they stay out of the layout even if
        // the window server reports them as visible later (e.g., on space change).
        if !known_visible.is_empty() {
            let known_set: HashSet<WindowId> = known_visible.into_iter().collect();
            // Find window server IDs for this app's windows that are no longer
            // in the accessibility list - these are "hidden" (closed with Cmd+W).
            for (wsid, wid) in self.window_ids.iter() {
                if wid.pid == pid {
                    if known_set.contains(wid) {
                        // Window is visible in accessibility - remove from hidden set
                        // in case it was previously hidden and has now reappeared.
                        self.hidden_windows.remove(wsid);
                    } else {
                        // Window is not visible in accessibility - mark as hidden.
                        self.hidden_windows.insert(*wsid);
                    }
                }
            }
            // Remove hidden windows from visible_windows.
            self.visible_windows.retain(|wsid| !self.hidden_windows.contains(wsid));
        }

        // The membership of the windows found for the first time is decided
        // before the layout sees them. Windows parked before a restart go
        // back first, so the layout sees them at their frames from before
        // parking. The windows open at launch are parked, if they must not
        // show, when startup completes.
        if !first_seen.is_empty() {
            self.app_still_running(pid);
        }
        self.decide_membership(&first_seen);
        self.restore_from_journal(pid);
        self.send_visible_windows_to_layout(pid);
        if self.startup_complete {
            self.park_what_must_not_show(pid);
        }
        self.focus_windows_seen(&first_seen);
    }

    /// Sends the current list of visible windows for the given app to the
    /// layout manager. Called when windows are discovered or when visibility
    /// changes (e.g., a window is minimized or unminimized).
    fn send_visible_windows_to_layout(&mut self, pid: pid_t) {
        let mut app_windows: BTreeMap<SpaceId, Vec<(WindowId, LayoutWindowInfo)>> = BTreeMap::new();
        // Track frames we've already seen to detect overlapping windows (tabs).
        // Windows with nearly identical frames are likely tabs in a tab group,
        // and we should only include one of them in the layout.
        let mut seen_frames: HashSet<(i32, i32, i32, i32)> = HashSet::default();
        let main_window = self.main_window();

        for wid in self
            .visible_windows
            .iter()
            .flat_map(|wsid| self.window_ids.get(wsid).copied())
            .filter(|wid| wid.pid == pid)
            .filter(|wid| self.window_is_tracked(*wid))
        {
            let Some(window) = self.windows.get(&wid) else { continue };
            let Some(layout_info) = self.layout_window_info(wid) else {
                continue;
            };
            let Some(space) = self.best_space_for_window(&layout_info.frame) else {
                continue;
            };
            // A window a per-screen switch just moved away is not on the
            // Space it left any more, even though its frame still says so
            // until the write lands.
            if self.moving_away.get(&wid) == Some(&space) {
                continue;
            }
            // A parked window stays in the list of a layout it belongs to, at
            // its frame from before parking, so that parking never removes
            // its node.
            let parked = self.parked.contains_key(&wid);
            if !(self.reaches_layout(space, wid) || parked && self.shows_on(space, wid)) {
                continue;
            }
            // Tabs in the same window group will have the same visual frame.
            // Parked windows share a corner without being tabs.
            if !parked {
                let frame_key = Self::frame_key(&window.frame_monotonic);
                // If we've already seen a window with this frame, skip this one
                // unless it's the main window (active tab).
                if seen_frames.contains(&frame_key) {
                    if main_window != Some(wid) {
                        continue;
                    }
                    // This is the main window, remove the previous entry with this frame
                    // and add this one instead.
                    if let Some(windows) = app_windows.get_mut(&space) {
                        windows.retain(|(other, info)| {
                            self.parked.contains_key(other)
                                || Self::frame_key(&info.frame) != frame_key
                        });
                    }
                }
                seen_frames.insert(frame_key);
            }
            app_windows.entry(space).or_default().push((wid, layout_info));
        }
        let screens = self.screens.clone();
        for screen in screens {
            let Some(space) = screen.space else { continue };
            self.send_layout_event(LayoutEvent::WindowsOnScreenUpdated(
                space,
                pid,
                app_windows.remove(&space).unwrap_or_default(),
            ));
        }
        // If it's possible we just added the main window to the layout, make
        // sure the layout knows it's focused.
        if let Some(main_window) = self.main_window() {
            if main_window.pid == pid {
                let spaces = self.screens.iter().flat_map(|screen| screen.space).collect();
                self.send_layout_event(LayoutEvent::WindowFocused(spaces, main_window));
            }
        }
    }

    /// The screen the window overlaps the most, or None if it is not on any
    /// screen. Apps park windows far off screen to hide them, and those belong
    /// to no screen at all.
    fn best_screen_idx_for_window(&self, frame: &CGRect) -> Option<usize> {
        self.screens
            .iter()
            .enumerate()
            .map(|(idx, screen)| (idx, screen.frame.intersection(frame).area()))
            .filter(|&(_, area)| area > 0.0)
            .max_by_key(|&(_, area)| area as i64)
            .map(|(idx, _)| idx)
            // A window with no area intersects nothing, so place it by its midpoint.
            .or_else(|| self.screens.iter().position(|screen| screen.frame.contains(frame.mid())))
    }

    fn best_space_for_window(&self, frame: &CGRect) -> Option<SpaceId> {
        self.screens[self.best_screen_idx_for_window(frame)?].space
    }

    /// The frame that places a window on a screen and a Space, and that the
    /// layout sees. For a parked window, that is its frame from before it was
    /// parked, not its corner.
    fn layout_frame(&self, wid: WindowId) -> Option<CGRect> {
        match self.parked.get(&wid) {
            Some(parked) => Some(parked.before),
            None => Some(self.windows.get(&wid)?.frame_monotonic),
        }
    }

    /// Gathers the window properties the layout uses to classify a window.
    fn layout_window_info(&self, wid: WindowId) -> Option<LayoutWindowInfo> {
        let window = self.windows.get(&wid)?;
        let app = self.apps.get(&wid.pid);
        Some(LayoutWindowInfo {
            frame: self.layout_frame(wid)?,
            bundle_id: app.and_then(|a| a.info.bundle_id.clone()),
            app_name: app.and_then(|a| a.info.localized_name.clone()),
            title: window.title.clone().into(),
            layer: window
                .window_server_id
                .and_then(|wsid| self.window_server_info.get(&wsid))
                .map(|info| info.layer),
            is_standard: window.is_ax_standard,
            is_resizable: window.is_resizable,
            ax_role: window.ax_role.clone(),
            ax_subrole: window.ax_subrole.clone(),
        })
    }

    fn update_active_screen(&mut self) {
        let changed = (|| {
            let screen = self.current_main_screen_index()?;
            Some(self.active_screen_idx.replace(screen as u16) != Some(screen as u16))
        })();
        if changed.unwrap_or(false)
            && let Some(status_tx) = &mut self.status_tx
        {
            status_tx.send(status::Event::FocusedScreenChanged);
        }
    }

    fn current_main_screen_index(&self) -> Option<usize> {
        let frame = self.layout_frame(self.main_window()?)?;
        self.best_screen_idx_for_window(&frame)
    }

    fn active_screen(&self) -> Option<&Screen> {
        self.screens.get(self.active_screen_idx.unwrap_or(0) as usize)
    }

    fn window_is_tracked(&self, _id: WindowId) -> bool {
        // For now we track all windows in the reactor and let the LayoutManager
        // decide what to keep.
        true
    }

    /// Returns the frame key (rounded to integers) for a window frame.
    /// Used to detect windows that share the same visual position (tabs).
    fn frame_key(frame: &CGRect) -> (i32, i32, i32, i32) {
        (
            frame.origin.x.round() as i32,
            frame.origin.y.round() as i32,
            frame.size.width.round() as i32,
            frame.size.height.round() as i32,
        )
    }

    fn send_layout_event(&mut self, event: LayoutEvent) {
        self.send_layout_event_with_context(event, ResponseContext::default());
    }

    fn send_layout_event_with_context(&mut self, event: LayoutEvent, context: ResponseContext) {
        let response = self.layout.handle_event(event);
        self.handle_layout_response_with_context(response, context);
        for space in self.screens.iter().flat_map(|screen| screen.space) {
            self.layout.debug_tree_desc(space, "after event", false);
        }
    }

    /// Handles the layout's response, and returns the sequence id of the
    /// raise request it made, if any.
    fn handle_layout_response(&mut self, response: layout::EventResponse) -> Option<u64> {
        self.handle_layout_response_with_context(response, ResponseContext::default())
    }

    fn handle_layout_response_with_context(
        &mut self,
        mut response: layout::EventResponse,
        ResponseContext {
            visible_window_order,
            from_mouse,
        }: ResponseContext,
    ) -> Option<u64> {
        if let Some(visible_window_order) = visible_window_order {
            response = self.filter_response(response, &visible_window_order);
        }

        let layout::EventResponse {
            frame_overrides,
            raise_windows,
            focus_window,
            focused_window_floating,
            ..
        } = response;
        self.pending_frame_overrides.extend(frame_overrides);

        if let Some(is_floating) = focused_window_floating {
            if let Some(status_tx) = &self.status_tx {
                status_tx.send(status::Event::FocusedWindowFloatingChanged(is_floating));
            }
        }
        if raise_windows.is_empty() && focus_window.is_none() {
            return None;
        }

        let mut app_handles = HashMap::default();
        for &wid in raise_windows.iter().chain(&focus_window) {
            if let Some(app) = self.apps.get(&wid.pid) {
                app_handles.insert(wid.pid, app.handle.clone());
            }
        }

        let mut windows_by_app_and_screen = HashMap::default();
        for &wid in &raise_windows {
            let Some(frame) = self.layout_frame(wid) else { continue };
            windows_by_app_and_screen
                .entry((wid.pid, self.best_space_for_window(&frame)))
                .or_insert(vec![])
                .push(wid);
        }

        let focus_window_with_warp = focus_window.map(|wid| {
            let warp = if self.config.settings.mouse_follows_focus && !from_mouse {
                self.windows.get(&wid).map(|w| w.frame_monotonic.mid())
            } else {
                // We disable warp above if the event itself is caused by mouse
                // movement.
                None
            };
            (wid, warp)
        });

        self.raise_sequence += 1;
        let sequence_id = self.raise_sequence;
        let msg = raise::Event::RaiseRequest(RaiseRequest {
            raise_windows: windows_by_app_and_screen.into_values().collect(),
            focus_window: focus_window_with_warp,
            app_handles,
            sequence_id,
        });

        _ = self.raise_manager_tx.send((Span::current(), msg));
        Some(sequence_id)
    }

    fn filter_response(
        &self,
        mut response: layout::EventResponse,
        mut visible_window_order: &[WindowServerId],
    ) -> layout::EventResponse {
        if let Some(focus) = response.focus_window {
            // Attempt to match out the focus window with the top visible window.
            if let Some(&first) = visible_window_order.first()
                && focus.wsid() == Some(first)
            {
                // Note that we keep the focus window in the request, unless
                // there are no raise windows.
                visible_window_order = &visible_window_order[1..];
            } else {
                return response;
            }
        };
        let desired_visible_wsids = response
            .raise_windows
            .iter()
            .flat_map(|wid| self.windows.get(wid).and_then(|window| window.window_server_id))
            .collect::<HashSet<_>>();
        let current_top_wsids = visible_window_order
            .iter()
            .copied()
            // Filter out off-screen windows and windows on non-zero layers.
            .filter(|wsid| self.should_compare_visible_window(*wsid))
            .take(desired_visible_wsids.len())
            .collect::<HashSet<_>>();

        // Optimize the case where the response is a no-op.
        if current_top_wsids == desired_visible_wsids {
            response.focus_window.take();
            response.raise_windows.clear();
        }
        response
    }

    /// The main window of the active app, if any.
    fn main_window(&self) -> Option<WindowId> {
        self.main_window_tracker.main_window()
    }

    fn main_window_space(&self) -> Option<SpaceId> {
        // TODO: Optimize this with a cache or something.
        self.best_space_for_window(&self.layout_frame(self.main_window()?)?)
    }

    #[instrument(skip(self), fields())]
    pub fn update_layout(&mut self, new_wids: &[WindowId], skip_anim: bool) {
        // Clear title bar drag tracking since layout changes will move windows
        self.title_bar_drag = None;
        let main_window = self.main_window();
        trace!(?main_window);
        let mut anim = Animation::new();
        let mut targets = BTreeMap::new();
        for &screen in &self.screens {
            let Some(space) = screen.space else { continue };
            if !skip_anim {
                self.layout.update_viewport_for_focus(space, screen.frame, &self.config);
            }
            let (result, groups) =
                self.layout.calculate_layout_and_groups(space, screen.frame, &self.config);

            self.group_indicators_tx
                .send(group_bars::Event::GroupsUpdated { space_id: space, groups });

            // Scroll layouts place windows off screen on purpose, so only
            // tiled frames are kept inside the screen.
            let is_scroll = self.layout.is_scroll_space(space);
            for (wid, frame) in result {
                let frame = if is_scroll {
                    frame
                } else {
                    let min_size =
                        self.layout.window_min_size(wid).unwrap_or(CGSize::new(0.0, 0.0));
                    fit_frame_to_screen(frame, min_size, screen.frame)
                };
                targets.insert(wid, (frame, screen.scale_factor));
            }
        }
        for (wid, frame) in mem::take(&mut self.pending_frame_overrides) {
            let scale_factor = self
                .best_screen_idx_for_window(&frame)
                .and_then(|idx| self.screens.get(idx))
                .map_or(1.0, |screen| screen.scale_factor);
            targets.insert(wid, (frame, scale_factor));
        }
        for (wid, (target_frame, scale_factor)) in targets {
            if self.resizing_window == Some(wid) {
                // The user is dragging this window's edge; correct it on mouse
                // up instead.
                // TODO: A pending frame override for this window is dropped
                // here rather than deferred to mouse up.
                continue;
            }
            if self.parked.contains_key(&wid) {
                // A parked window keeps its place in the layout but stays in
                // its corner.
                continue;
            }
            let Some(window) = self.windows.get_mut(&wid) else {
                // If we restored a saved state the window may not be available yet.
                continue;
            };
            let target_frame = round_to_physical(target_frame, scale_factor);
            let current_frame = window.frame_monotonic;
            let forced = self.forced_writes.remove(&wid);
            if target_frame.same_as(current_frame) && !forced {
                continue;
            }
            // Some apps move a window back after we place it, which turns into
            // an event that makes us place it again. Stop writing the frame
            // once it's clear the app won't keep it.
            let now = Instant::now();
            let attempt = self.frame_attempts.entry(wid).or_insert(FrameAttempt {
                target: target_frame,
                count: 0,
                last: now,
            });
            if !attempt.target.same_as(target_frame)
                || now.duration_since(attempt.last) > FRAME_ATTEMPT_RESET
            {
                *attempt = FrameAttempt {
                    target: target_frame,
                    count: 0,
                    last: now,
                };
            }
            attempt.last = now;
            attempt.count = attempt.count.saturating_add(1);
            if attempt.count > MAX_FRAME_ATTEMPTS {
                if attempt.count == MAX_FRAME_ATTEMPTS + 1 {
                    warn!(?wid, ?current_frame, ?target_frame, "Giving up on window frame");
                }
                continue;
            }
            let Some(app) = self.apps.get(&wid.pid) else {
                continue;
            };
            let txid = window.next_txid();
            trace!(?wid, ?current_frame, ?target_frame);
            let is_new = new_wids.contains(&wid);
            anim.add_window(&app.handle, wid, current_frame, target_frame, is_new, txid);
            window.frame_monotonic = target_frame;
        }
        self.forced_writes.clear();
        // If the user is doing something with the mouse we don't want to
        // animate on top of that.
        let skip_anim =
            skip_anim || !self.config.settings.animate || self.layout.has_active_scroll_animation();
        self.send_animation(anim, skip_anim);

        // Refresh debug overlay if visible
        self.refresh_debug_drop_zones();
    }

    /// Hands the frames to the animation manager, which ends any animation in
    /// progress before it writes them. With no animation manager, the final
    /// frames are written at once. `skip_anim` writes the final frames
    /// without animating.
    fn send_animation(&self, anim: Animation, skip_anim: bool) {
        if let Some(tx) = &self.animation_tx
            && !anim.is_empty()
        {
            let message = if skip_anim {
                AnimationMessage::SkipToEnd(anim)
            } else {
                AnimationMessage::Replace(anim)
            };
            if let Err(err) = tx.send(message) {
                error!("Animation manager exited unexpectedly");
                match err.0 {
                    AnimationMessage::Replace(animation) => animation.skip_to_end(),
                    AnimationMessage::SkipToEnd(animation) => animation.skip_to_end(),
                }
            }
        } else {
            anim.skip_to_end();
        }
    }

    /// Animate windows to preview positions during a drag operation.
    ///
    /// This updates `frame_monotonic` to track the preview target positions,
    /// so that subsequent animations (including the final `update_layout` on drop)
    /// can correctly animate from the current position.
    fn animate_to_preview(&mut self, targets: &[(WindowId, CGRect)], scale_factor: f64) {
        let mut anim = Animation::new_preview();

        for &(wid, target_frame) in targets {
            let Some(window) = self.windows.get_mut(&wid) else {
                continue;
            };
            let target_frame = round_to_physical(target_frame, scale_factor);
            let current_frame = window.frame_monotonic;
            if target_frame.same_as(current_frame) {
                continue;
            }
            let Some(app) = self.apps.get(&wid.pid) else {
                continue;
            };
            let txid = window.next_txid();
            anim.add_window(&app.handle, wid, current_frame, target_frame, false, txid);
            // Update frame_monotonic to track where the window will be
            window.frame_monotonic = target_frame;
        }

        if let Some(tx) = &self.animation_tx
            && !anim.is_empty()
        {
            if let Err(err) = tx.send(AnimationMessage::Replace(anim)) {
                error!("Animation manager exited unexpectedly");
                match err.0 {
                    AnimationMessage::Replace(animation) => animation.skip_to_end(),
                    AnimationMessage::SkipToEnd(animation) => animation.skip_to_end(),
                }
            }
        }
    }

    /// Refresh the debug drop zone overlay if it's currently visible.
    fn refresh_debug_drop_zones(&self) {
        if !self.debug_drop_zones_visible {
            return;
        }

        let mut all_zones = Vec::new();
        for (idx, screen) in self.screens.iter().enumerate() {
            if let Some(space) = screen.space {
                let zones = self.layout.generate_debug_drop_zones(
                    space,
                    screen.frame,
                    &self.config,
                    idx as i32,
                );
                debug!(
                    "Debug zones: screen {} space {:?} frame {:?} -> {} zones",
                    idx,
                    space,
                    screen.frame,
                    zones.len()
                );
                all_zones.extend(zones);
            } else {
                debug!("Debug zones: screen {} has no space", idx);
            }
        }
        debug!("Debug zones: total {} zones", all_zones.len());
        swift_bridge::show_drop_zones(&all_zones);
    }
}

#[cfg(test)]
pub mod tests {
    use itertools::Itertools;
    use objc2_core_foundation::{CGPoint, CGSize};
    use test_log::test;

    use super::testing::*;
    use super::*;
    use crate::actor::app::Request;
    use crate::actor::layout::{ActiveContext, LayoutManager, SizeShare};
    use crate::model::Direction;
    use crate::sys::window_server::WindowServerId;

    #[test]
    fn it_ignores_stale_resize_events() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        let requests = apps.requests();
        assert!(!requests.is_empty());
        let events_1 = apps.simulate_events_for_requests(requests);

        reactor.handle_events(apps.make_app(2, make_windows(2)));
        assert!(!apps.requests().is_empty());

        for event in dbg!(events_1) {
            reactor.handle_event(event);
        }
        let requests = apps.requests();
        assert!(
            requests.is_empty(),
            "got requests when there should have been none: {requests:?}"
        );
    }

    #[test]
    fn it_sends_layout_animation_to_manager() {
        let mut apps = Apps::new();
        let (mut reactor, mut animation_rx) =
            Reactor::new_for_test_with_animation(LayoutManager::new_for_test(), true);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);

        assert!(
            apps.requests().is_empty(),
            "layout should be handed to the animation manager, not sent directly to app actors"
        );
        assert!(matches!(
            animation_rx.try_recv(),
            Ok(animation::Message::Replace(_))
        ));
    }

    #[test]
    fn floating_window_restores_its_last_user_frame() {
        use LayoutCommand::ToggleWindowFloating;

        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let wid = WindowId::new(1, 1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(1), Some(wid), true));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);
        reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space], wid));

        // First float restores the frame the window had before it was tiled.
        reactor.handle_event(Event::Command(Command::Layout(ToggleWindowFloating)));
        let initial_frame = CGRect::new(CGPoint::new(100., 100.), CGSize::new(50., 50.));
        let requests = apps.requests();
        assert!(requests.iter().any(|request| {
            matches!(request, Request::SetWindowFrame(request_wid, frame, _) if *request_wid == wid && *frame == initial_frame)
        }), "{requests:?}");
        for event in apps.simulate_events_for_requests(requests) {
            reactor.handle_event(event);
        }

        // A user move/resize while floating becomes the next restore target.
        let updated_frame = CGRect::new(CGPoint::new(300., 400.), CGSize::new(250., 125.));
        reactor.handle_event(Event::WindowFrameChanged(
            wid,
            updated_frame,
            apps.windows[&wid].last_seen_txid,
            Requested(false),
            None,
        ));
        assert_eq!(reactor.layout.floating_restore_frame(wid), Some(updated_frame));
        assert!(apps.requests().is_empty());

        reactor.handle_event(Event::Command(Command::Layout(ToggleWindowFloating)));
        apps.simulate_until_quiet(&mut reactor);
        assert_eq!(reactor.windows[&wid].frame_monotonic, screen);
        reactor.handle_event(Event::Command(Command::Layout(ToggleWindowFloating)));
        let requests = apps.requests();
        assert!(requests.iter().any(|request| {
            matches!(request, Request::SetWindowFrame(request_wid, frame, _) if *request_wid == wid && *frame == updated_frame)
        }), "{requests:?}");
    }

    #[test]
    fn floating_frame_restoration_uses_animation() {
        use LayoutCommand::ToggleWindowFloating;

        let mut apps = Apps::new();
        let (mut reactor, mut animation_rx) =
            Reactor::new_for_test_with_animation(LayoutManager::new_for_test(), true);
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let wid = WindowId::new(1, 1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(1), Some(wid), true));
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_event(Event::StartupComplete);
        let animation::Message::Replace(animation) = animation_rx.try_recv().unwrap() else {
            panic!("expected initial layout animation");
        };
        animation.skip_to_end();
        apps.simulate_until_quiet(&mut reactor);
        reactor.send_layout_event(LayoutEvent::WindowFocused(vec![space], wid));

        reactor.handle_event(Event::Command(Command::Layout(ToggleWindowFloating)));
        assert!(
            apps.requests().is_empty(),
            "restore should be animated, not written directly"
        );
        assert!(matches!(
            animation_rx.try_recv(),
            Ok(animation::Message::Replace(_))
        ));
    }

    #[test]
    fn it_sends_writes_when_stale_read_state_looks_same_as_written_state() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        let events_1 = apps.simulate_events();
        let state_1 = apps.windows.clone();
        assert!(!state_1.is_empty());

        for event in events_1 {
            reactor.handle_event(event);
        }
        assert!(apps.requests().is_empty());

        reactor.handle_events(apps.make_app(2, make_windows(1)));
        let _events_2 = apps.simulate_events();

        reactor.handle_event(Event::WindowDestroyed(WindowId::new(2, 1)));
        let _events_3 = apps.simulate_events();
        let state_3 = apps.windows;

        // These should be the same, because we should have resized the first
        // two windows both at the beginning, and at the end when the third
        // window was destroyed.
        for (wid, state) in dbg!(state_1) {
            assert!(state_3.contains_key(&wid), "{wid:?} not in {state_3:#?}");
            assert_eq!(state.frame, state_3[&wid].frame);
        }
    }

    #[test]
    fn sends_writes_same_as_last_written_state_if_changed_externally() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        let events_1 = apps.simulate_events();
        let state_1 = apps.windows.clone();
        assert!(!state_1.is_empty());

        for event in events_1 {
            reactor.handle_event(event);
        }
        assert!(apps.requests().is_empty());

        // Move a window in an invalid way.
        let wid = WindowId::new(1, 1);
        let old_frame = state_1[&wid].frame;
        reactor.handle_event(Event::WindowFrameChanged(
            wid,
            CGRect::new(
                CGPoint::new(old_frame.origin.x, old_frame.origin.y + 10.),
                old_frame.size,
            ),
            state_1[&wid].last_seen_txid,
            Requested(false),
            None,
        ));

        let requests = apps.requests();
        assert!(!requests.is_empty());
        let _events_2 = apps.simulate_events_for_requests(requests);
        assert_eq!(apps.windows[&wid].frame, old_frame);
    }

    #[test]
    fn it_responds_to_resizes() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(3)));
        reactor.handle_event(Event::StartupComplete);

        let events = apps.simulate_events();
        let windows = apps.windows.clone();
        for event in events {
            reactor.handle_event(event);
        }
        assert!(
            apps.requests().is_empty(),
            "reactor shouldn't react to unsurprising events"
        );

        // Resize the right edge of the middle window.
        let resizing = WindowId::new(1, 2);
        let window = &apps.windows[&resizing];
        let frame = CGRect::new(
            window.frame.origin,
            CGSize::new(window.frame.size.width + 10., window.frame.size.height),
        );
        reactor.handle_event(Event::WindowFrameChanged(
            resizing,
            frame,
            window.last_seen_txid,
            Requested(false),
            None,
        ));

        // Expect the next window to be resized.
        let next = WindowId::new(1, 3);
        let old_frame = windows[&next].frame;
        let requests = apps.requests();
        assert!(!requests.is_empty());
        let _events = apps.simulate_events_for_requests(requests);
        assert_ne!(old_frame, apps.windows[&next].frame);
    }

    #[test]
    fn it_manages_windows_on_enabled_spaces() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(1)));
        reactor.handle_event(Event::StartupComplete);

        let _events = apps.simulate_events();
        assert_eq!(
            full_screen,
            apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
        );
    }

    #[test]
    fn it_stops_writing_a_frame_the_app_keeps_undoing() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![1.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(1)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        // The app moves the window off its tiled position after every write.
        let wid = WindowId::new(1, 1);
        let moved_back = CGRect::new(CGPoint::new(100., 100.), full_screen.size);
        let mut writes = 0;
        let mut writes_in_last_round = 0;
        for _ in 0..MAX_FRAME_ATTEMPTS + 5 {
            reactor.handle_event(Event::WindowFrameChanged(
                wid,
                moved_back,
                apps.windows[&wid].last_seen_txid,
                Requested(false),
                None,
            ));
            let requests = apps.requests();
            writes_in_last_round = requests
                .iter()
                .filter(|request| {
                    matches!(
                        request,
                        Request::SetWindowFrame(..) | Request::AnimationFrame { .. }
                    )
                })
                .count();
            writes += writes_in_last_round;
            _ = apps.simulate_events_for_requests(requests);
        }
        // The initial placement counts toward the limit.
        assert!(writes <= MAX_FRAME_ATTEMPTS as usize, "{writes} writes");
        assert_eq!(0, writes_in_last_round);
    }

    #[test]
    fn windows_parked_off_screen_belong_to_no_screen() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        let off_screen = CGRect::new(CGPoint::new(0., -2000.), CGSize::new(1000., 500.));
        assert_eq!(None, reactor.best_screen_idx_for_window(&off_screen));

        let partly_on_screen = CGRect::new(CGPoint::new(0., -100.), CGSize::new(1000., 500.));
        assert_eq!(Some(0), reactor.best_screen_idx_for_window(&partly_on_screen));

        let no_area = CGRect::new(CGPoint::new(500., 500.), CGSize::new(0., 0.));
        assert_eq!(Some(0), reactor.best_screen_idx_for_window(&no_area));
    }

    #[test]
    fn it_keeps_each_displays_bounds_next_to_its_visible_frame() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let visible = CGRect::new(CGPoint::new(0., 25.), CGSize::new(1000., 975.));
        let bounds = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let event = Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![visible],
            bounds: vec![bounds],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        };
        let event = ron::de::from_str(&ron::ser::to_string(&event).unwrap()).unwrap();
        reactor.handle_event(event);
        assert_eq!(visible, reactor.screens[0].frame);
        assert_eq!(bounds, reactor.screens[0].bounds);
    }

    #[test]
    fn a_recording_without_display_bounds_uses_the_visible_frames() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let visible = CGRect::new(CGPoint::new(0., 25.), CGSize::new(1000., 975.));
        let event = Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![visible],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        };
        let recorded = ron::ser::to_string(&event).unwrap();
        let old_recording = recorded.replace("bounds:[],", "");
        assert_ne!(recorded, old_recording);
        reactor.handle_event(ron::de::from_str(&old_recording).unwrap());
        assert_eq!(visible, reactor.screens[0].bounds);
    }

    #[test]
    fn h1_a_display_without_reported_bounds_uses_its_visible_frame() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let main = CGRect::new(CGPoint::new(0., 25.), CGSize::new(1000., 975.));
        let main_bounds = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let right = CGRect::new(CGPoint::new(1000., 25.), CGSize::new(1000., 975.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![main, right],
            bounds: vec![main_bounds],
            spaces: vec![Some(SpaceId::new(1)), Some(SpaceId::new(2))],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        assert_eq!(
            vec![(main, main_bounds), (right, right)],
            reactor
                .screens
                .iter()
                .map(|screen| (screen.frame, screen.bounds))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_recording_file_made_before_displays_reported_bounds_replays() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("trace.ron");
        let mut config = Config::default();
        config.settings.default_disable = false;
        config.settings.animate = false;
        let (group_indicators_tx, _) = crate::actor::channel();
        let mut reactor = Reactor::new(
            Arc::new(config),
            LayoutManager::new_for_test(),
            Record::new(Some(&path)),
            group_indicators_tx,
            ParkedJournal::in_memory(),
        );
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        let mut apps = Apps::new();
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);
        drop(reactor);

        let recorded = std::fs::read_to_string(&path).unwrap();
        assert_eq!(1, recorded.matches("bounds:[").count());
        let start = recorded.find("bounds:[").unwrap();
        let end = start + recorded[start..].find("],").unwrap() + 2;
        let old_recording = format!("{}{}", &recorded[..start], &recorded[end..]);
        assert!(!old_recording.contains("bounds:"));
        std::fs::write(&path, old_recording).unwrap();

        replay(&path, |_, _| {}).unwrap();
    }

    /// A replay starts from the parked-window journal and the contexts that
    /// the recorded reactor read at launch, so it writes the same frames: it
    /// puts back the window parked before the launch and parks the window
    /// that isn't in the active context.
    #[test]
    fn a_replay_starts_from_the_journal_and_the_contexts_read_at_launch() {
        use crate::actor::contexts_store::ContextsStore;
        use crate::actor::parked_journal::JournalEntry;
        use crate::model::contexts::{ContextKey, Contexts, WindowDesc};

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("trace.ron");
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let mut contexts = Contexts::new();
        let id = contexts.create("C").unwrap();
        let desc = WindowDesc {
            wid: WindowId::new(1, 1),
            bundle_id: Some("com.testapp1".into()),
            app_name: Some("TestApp1".into()),
            title: "Window1".into(),
            window_server_id: Some(WindowServerId::new(1)),
        };
        contexts.add_window(id, &desc).unwrap();
        contexts.switch_to(ContextKey::Named(id)).unwrap();
        let store = ContextsStore::new(dir.path().join("contexts.json"));
        store.save(&contexts, Some("boot")).unwrap();
        let parked_before = CGRect::new(CGPoint::new(500., 0.), CGSize::new(500., 1000.));
        let mut journal =
            ParkedJournal::open(dir.path().join("parked.json"), std::time::SystemTime::now());
        journal
            .record(vec![JournalEntry {
                pid: 1,
                bundle_id: Some("com.testapp1".into()),
                window_server_id: WindowServerId::new(2),
                title: "Window2".into(),
                frame: parked_before.into(),
            }])
            .unwrap();
        let mut config = Config::default();
        config.settings.default_disable = false;
        config.settings.animate = false;
        config.settings.experimental.contexts.enable = true;
        let (group_indicators_tx, _) = crate::actor::channel();
        let mut reactor = Reactor::new(
            Arc::new(config),
            LayoutManager::new_for_test(),
            Record::new(Some(&path)),
            group_indicators_tx,
            ParkedJournal::open(dir.path().join("parked.json"), std::time::SystemTime::now()),
        );
        reactor.process_lookup = Box::new(test_app_process);
        reactor.open_contexts(store, Some("boot".into()), std::time::SystemTime::now());
        reactor.record_launch_state();
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![screen],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![1.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        let mut apps = Apps::new();
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        let frame_writes = |requests: Vec<Request>| -> Vec<(WindowId, CGRect)> {
            requests
                .into_iter()
                .filter_map(|request| match request {
                    Request::SetWindowFrame(wid, frame, _) => Some((wid, frame)),
                    _ => None,
                })
                .collect()
        };
        let recorded = frame_writes(apps.requests());
        assert!(recorded.contains(&(WindowId::new(1, 2), parked_before)));
        assert_eq!(
            vec![WindowId::new(1, 2)],
            reactor.parked.keys().copied().collect::<Vec<_>>()
        );
        drop(reactor);

        let (tx, rx) = std::sync::mpsc::channel();
        replay(&path, move |_, request| _ = tx.send(request)).unwrap();
        let mut replayed = vec![];
        while let Ok(request) = rx.recv_timeout(Duration::from_millis(200)) {
            replayed.push(request);
        }
        assert_eq!(recorded, frame_writes(replayed));
    }

    /// The launch state line keeps the active context, whichever it is.
    #[test]
    fn the_launch_state_line_keeps_every_active_context() {
        use crate::model::contexts::{ContextKey, Contexts};

        let mut contexts = Contexts::new();
        let id = contexts.create("C").unwrap();
        for key in [
            ContextKey::Everything,
            ContextKey::Unsorted,
            ContextKey::Named(id),
        ] {
            contexts.switch_to(key).unwrap();
            let state = LaunchState {
                journal: vec![],
                contexts: Some(contexts.clone()),
                processes: vec![],
            };
            let line = ron::ser::to_string(&state).unwrap();
            let read: LaunchState = ron::de::from_str(&line).unwrap();
            assert_eq!(
                Some(key),
                read.contexts.map(|contexts| contexts.active()),
                "{line}"
            );
        }
    }

    #[test]
    fn it_selects_the_main_window_on_space_enable() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let ws_info = (1..=2)
            .map(|id| WindowServerInfo {
                id: WindowServerId::new(id),
                pid: 1,
                layer: 0,
                frame: CGRect::ZERO,
            })
            .collect::<Vec<_>>();
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![None],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(ws_info.clone()),
        });

        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        reactor.handle_events(apps.simulate_events());

        reactor.handle_event(Event::SpaceChanged(
            vec![Some(SpaceId::new(1))],
            WindowsOnScreen::new(ws_info),
        ));
        reactor.handle_events(apps.simulate_events());
        assert_eq!(
            reactor.layout.selected_window(SpaceId::new(1)),
            Some(WindowId::new(1, 1))
        );
    }

    #[test]
    fn it_surfaces_on_screen_change_when_the_snapshot_is_empty() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 900.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        assert!(raise_manager_rx.try_recv().is_ok());
    }

    #[test]
    fn it_surfaces_on_screen_change_when_the_snapshot_omits_the_windows() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        // The snapshot lists only a window we don't manage, so it says nothing
        // about where the windows we want to raise are.
        let on_screen = vec![WindowServerInfo {
            id: WindowServerId::new(999),
            pid: 2,
            layer: 0,
            frame: CGRect::new(CGPoint::new(0., 0.), CGSize::new(100., 100.)),
        }];
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 900.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(on_screen),
        });
        assert!(raise_manager_rx.try_recv().is_ok());
    }

    #[test]
    fn it_skips_surface_on_screen_change_when_top_layer_order_matches() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        // A screen change that doesn't disturb the stacking shouldn't restack.
        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(
                space,
                CGSize::new(1000., 900.),
                ActiveContext::EVERYTHING,
            ))
            .raise_windows;
        let on_screen = desired
            .iter()
            .map(|wid| WindowServerInfo {
                id: reactor.windows[wid].window_server_id.unwrap(),
                pid: wid.pid,
                layer: 0,
                frame: reactor.windows[wid].frame_monotonic,
            })
            .collect();
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 900.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(on_screen),
        });
        assert!(raise_manager_rx.try_recv().is_err());
    }

    #[test]
    fn it_surfaces_on_screen_change_when_another_window_is_in_front() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        // An unmanaged window is in front of the ones the layout wants on top,
        // so the restack is still needed.
        let mut on_screen = vec![WindowServerInfo {
            id: WindowServerId::new(999),
            pid: 2,
            layer: 0,
            frame: CGRect::new(CGPoint::new(0., 0.), CGSize::new(100., 100.)),
        }];
        on_screen.extend(reactor.windows.iter().map(|(wid, window)| WindowServerInfo {
            id: window.window_server_id.unwrap(),
            pid: wid.pid,
            layer: 0,
            frame: window.frame_monotonic,
        }));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 900.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: WindowsOnScreen::new(on_screen),
        });
        assert!(raise_manager_rx.try_recv().is_ok());
    }

    #[test]
    fn it_skips_surface_when_top_layer_order_matches() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(
                space,
                CGSize::new(1000., 1000.),
                ActiveContext::EVERYTHING,
            ))
            .raise_windows;
        let on_screen = desired
            .iter()
            .map(|wid| WindowServerInfo {
                id: reactor.windows[wid].window_server_id.unwrap(),
                pid: wid.pid,
                layer: 0,
                frame: reactor.windows[wid].frame_monotonic,
            })
            .collect();
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space)],
            WindowsOnScreen::new(on_screen),
        ));

        assert!(raise_manager_rx.try_recv().is_err());
    }

    #[test]
    fn it_skips_surface_when_top_layer_windows_are_reordered() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(
                space,
                CGSize::new(1000., 1000.),
                ActiveContext::EVERYTHING,
            ))
            .raise_windows;
        let on_screen = desired
            .iter()
            .rev()
            .map(|wid| WindowServerInfo {
                id: reactor.windows[wid].window_server_id.unwrap(),
                pid: wid.pid,
                layer: 0,
                frame: reactor.windows[wid].frame_monotonic,
            })
            .collect();
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space)],
            WindowsOnScreen::new(on_screen),
        ));

        assert!(raise_manager_rx.try_recv().is_err());
    }

    #[test]
    fn it_surfaces_top_layer_windows_when_top_managed_set_differs() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(
                space,
                CGSize::new(1000., 1000.),
                ActiveContext::EVERYTHING,
            ))
            .raise_windows;
        let mut on_screen = vec![WindowServerInfo {
            id: WindowServerId::new(90),
            pid: 9,
            layer: 0,
            frame: CGRect::new(CGPoint::new(10., 10.), CGSize::new(100., 100.)),
        }];
        on_screen.extend(desired.iter().map(|wid| WindowServerInfo {
            id: reactor.windows[wid].window_server_id.unwrap(),
            pid: wid.pid,
            layer: 0,
            frame: reactor.windows[wid].frame_monotonic,
        }));
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space)],
            WindowsOnScreen::new(on_screen),
        ));

        let msg = raise_manager_rx.try_recv().expect("Should have sent an event").1;
        match msg {
            raise::Event::RaiseRequest(RaiseRequest {
                raise_windows, focus_window, ..
            }) => {
                assert_eq!(raise_windows, vec![desired]);
                assert!(focus_window.is_none());
            }
            _ => panic!("Unexpected event: {msg:?}"),
        }
    }

    #[test]
    fn filter_response_clears_matching_focus_and_raise_windows() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.screens = vec![Screen {
            id: ScreenId::new(1),
            frame: CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.)),
            bounds: CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.)),
            space: Some(SpaceId::new(1)),
            scale_factor: 2.0,
        }];
        let w1 = WindowId::with_wsid(1, WindowServerId::new(1));
        let w2 = WindowId::with_wsid(1, WindowServerId::new(2));
        reactor.windows.insert(
            w1,
            super::WindowState {
                title: Secret::new(String::new()),
                window_server_id: Some(WindowServerId::new(1)),
                frame_monotonic: CGRect::new(CGPoint::new(0., 0.), CGSize::new(500., 1000.)),
                is_ax_standard: true,
                is_resizable: true,
                ax_role: String::new(),
                ax_subrole: None,
                last_sent_txid: TransactionId::default(),
            },
        );
        reactor.windows.insert(
            w2,
            super::WindowState {
                title: Secret::new(String::new()),
                window_server_id: Some(WindowServerId::new(2)),
                frame_monotonic: CGRect::new(CGPoint::new(500., 0.), CGSize::new(500., 1000.)),
                is_ax_standard: true,
                is_resizable: true,
                ax_role: String::new(),
                ax_subrole: None,
                last_sent_txid: TransactionId::default(),
            },
        );
        reactor.visible_windows =
            [WindowServerId::new(1), WindowServerId::new(2)].into_iter().collect();
        reactor.window_server_info.insert(
            WindowServerId::new(1),
            WindowServerInfo {
                id: WindowServerId::new(1),
                pid: 1,
                layer: 0,
                frame: reactor.windows[&w1].frame_monotonic,
            },
        );
        reactor.window_server_info.insert(
            WindowServerId::new(2),
            WindowServerInfo {
                id: WindowServerId::new(2),
                pid: 1,
                layer: 0,
                frame: reactor.windows[&w2].frame_monotonic,
            },
        );

        let response = reactor.filter_response(
            layout::EventResponse {
                frame_overrides: vec![],
                raise_windows: vec![w2],
                focus_window: Some(w1),
                ..Default::default()
            },
            &[WindowServerId::new(1), WindowServerId::new(2)],
        );

        assert!(response.raise_windows.is_empty());
        assert!(response.focus_window.is_none());
    }

    #[test]
    fn filter_response_keeps_response_when_focus_is_not_frontmost() {
        let reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let w1 = WindowId::with_wsid(1, WindowServerId::new(1));
        let w2 = WindowId::with_wsid(1, WindowServerId::new(2));

        let response = reactor.filter_response(
            layout::EventResponse {
                frame_overrides: vec![],
                raise_windows: vec![w2],
                focus_window: Some(w1),
                ..Default::default()
            },
            &[WindowServerId::new(2), WindowServerId::new(1)],
        );

        assert_eq!(response.raise_windows, vec![w2]);
        assert_eq!(response.focus_window, Some(w1));
    }

    #[test]
    fn it_ignores_unmanaged_and_nonzero_layer_windows_when_comparing_space_order() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        let space = SpaceId::new(1);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(1, make_windows(2), None, false));
        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        let desired = reactor
            .layout
            .handle_event(LayoutEvent::SpaceExposed(
                space,
                CGSize::new(1000., 1000.),
                ActiveContext::EVERYTHING,
            ))
            .raise_windows;
        let mut on_screen = vec![
            WindowServerInfo {
                id: WindowServerId::new(90),
                pid: 9,
                layer: 3,
                frame: CGRect::new(CGPoint::new(10., 10.), CGSize::new(100., 100.)),
            },
            WindowServerInfo {
                id: WindowServerId::new(91),
                pid: 9,
                layer: 0,
                frame: CGRect::new(CGPoint::new(2000., 10.), CGSize::new(100., 100.)),
            },
        ];
        on_screen.extend(desired.iter().map(|wid| WindowServerInfo {
            id: reactor.windows[wid].window_server_id.unwrap(),
            pid: wid.pid,
            layer: 0,
            frame: reactor.windows[wid].frame_monotonic,
        }));
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space)],
            WindowsOnScreen::new(on_screen),
        ));

        assert!(raise_manager_rx.try_recv().is_err());
    }

    #[test]
    fn it_ignores_windows_on_disabled_spaces() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![None],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(1)));

        let state_before = apps.windows.clone();
        let _events = apps.simulate_events();
        assert_eq!(state_before, apps.windows, "Window should not have been moved",);

        // Make sure it doesn't choke on destroyed events for ignored windows.
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 1)));
        reactor.handle_event(Event::WindowCreated(
            WindowId::new(1, 2),
            make_window(2),
            MouseState::Up,
        ));
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 2)));
    }

    #[test]
    fn it_keeps_discovered_windows_on_their_initial_screen() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen1, screen2],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1)), Some(SpaceId::new(2))],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        let mut windows = make_windows(2);
        windows[1].frame.origin = CGPoint::new(1100., 100.);
        reactor.handle_events(apps.make_app(1, windows));
        reactor.handle_event(Event::StartupComplete);

        let _events = apps.simulate_events();
        assert_eq!(
            screen1,
            apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
        );
        assert_eq!(
            screen2,
            apps.windows.get(&WindowId::new(1, 2)).expect("Window was not resized").frame,
        );
    }

    #[test]
    fn it_moves_windows_dragged_between_spaces() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen1, screen2],
            bounds: vec![],
            spaces: vec![Some(space1), Some(space2)],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        // Both windows start on screen1 / space1.
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_events(apps.simulate_events());

        let dragged = WindowId::new(1, 1);
        let space1_windows: Vec<_> = reactor
            .layout
            .calculate_layout(space1, screen1, &reactor.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect();
        assert!(space1_windows.contains(&dragged));
        assert!(reactor.layout.calculate_layout(space2, screen2, &reactor.config).is_empty());

        // Drag window 1 onto screen2, keeping its size the same so this is a
        // pure move (not a resize). The mouse button is still down.
        let frame = apps.windows[&dragged].frame;
        let new_frame = CGRect::new(CGPoint::new(1100., frame.origin.y), frame.size);
        reactor.handle_event(Event::WindowFrameChanged(
            dragged,
            new_frame,
            apps.windows[&dragged].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));

        // The window now belongs to space2's layout and has left space1.
        let space1_windows: Vec<_> = reactor
            .layout
            .calculate_layout(space1, screen1, &reactor.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect();
        let space2_windows: Vec<_> = reactor
            .layout
            .calculate_layout(space2, screen2, &reactor.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect();
        assert!(!space1_windows.contains(&dragged), "{space1_windows:?}");
        assert!(space2_windows.contains(&dragged), "{space2_windows:?}");
        assert!(space1_windows.contains(&WindowId::new(1, 2)));

        // The reactor must not write any frames while the drag is in progress.
        assert!(
            apps.requests().is_empty(),
            "reactor shouldn't move windows mid-drag"
        );

        // Releasing the mouse re-tiles the window onto screen2.
        reactor.handle_event(Event::MouseUp);
        let requests = apps.requests();
        assert!(!requests.is_empty(), "release should re-tile the window");
        let events = apps.simulate_events_for_requests(requests);
        for event in events {
            reactor.handle_event(event);
        }
        assert_eq!(screen2, apps.windows[&dragged].frame);
    }

    #[test]
    fn it_keeps_windows_in_space_on_intra_space_drag() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen1, screen2],
            bounds: vec![],
            spaces: vec![Some(space1), Some(space2)],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_events(apps.simulate_events());

        // Drag a window to a different position but still on screen1 / space1.
        let dragged = WindowId::new(1, 1);
        let frame = apps.windows[&dragged].frame;
        let new_frame = CGRect::new(
            CGPoint::new(frame.origin.x + 20., frame.origin.y + 20.),
            frame.size,
        );
        reactor.handle_event(Event::WindowFrameChanged(
            dragged,
            new_frame,
            apps.windows[&dragged].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));

        // The window stays on space1 and space2 remains empty.
        let space1_windows: Vec<_> = reactor
            .layout
            .calculate_layout(space1, screen1, &reactor.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .collect();
        assert!(space1_windows.contains(&dragged));
        assert!(reactor.layout.calculate_layout(space2, screen2, &reactor.config).is_empty());

        // No frames are written while the mouse button is still down.
        assert!(
            apps.requests().is_empty(),
            "reactor shouldn't move windows mid-drag"
        );
    }

    /// Neighbors still follow along, and the window is corrected on mouse up.
    #[test]
    fn it_doesnt_write_to_a_window_the_user_is_resizing() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        let resized = WindowId::new(1, 1);
        let neighbor = WindowId::new(1, 2);
        let frame = apps.windows[&resized].frame;

        // The user drags the shared edge left, shrinking window 1.
        let new_frame = CGRect::new(
            frame.origin,
            CGSize::new(frame.size.width - 100., frame.size.height),
        );
        apps.windows.get_mut(&resized).unwrap().frame = new_frame;
        reactor.handle_event(Event::WindowFrameChanged(
            resized,
            new_frame,
            apps.windows[&resized].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));

        // The neighbor grows to fill the space, but we leave the dragged window
        // alone.
        let wids = written_wids(apps.requests());
        assert!(wids.contains(&neighbor), "neighbor should follow: {wids:?}");
        assert!(
            !wids.contains(&resized),
            "shouldn't write to the window being resized: {wids:?}"
        );

        // The app settles on a change in three directions at once, which the
        // layout tree refuses to apply. The model and the window now disagree.
        let odd_frame = CGRect::new(
            CGPoint::new(new_frame.origin.x + 10., new_frame.origin.y + 10.),
            CGSize::new(new_frame.size.width - 30., new_frame.size.height - 5.),
        );
        apps.windows.get_mut(&resized).unwrap().frame = odd_frame;
        reactor.handle_event(Event::WindowFrameChanged(
            resized,
            odd_frame,
            apps.windows[&resized].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));
        let wids = written_wids(apps.requests());
        assert!(
            !wids.contains(&resized),
            "shouldn't write to the window being resized: {wids:?}"
        );

        // Releasing the mouse snaps it back to the layout's frame.
        reactor.handle_event(Event::MouseUp);
        let wids = written_wids(apps.requests());
        assert!(
            wids.contains(&resized),
            "release should correct the window: {wids:?}"
        );
    }

    /// The MouseUp event can be lost, e.g. if the event tap is disabled while
    /// the button is down. The mouse state on the next frame change releases
    /// the window instead.
    #[test]
    fn it_stops_suppressing_a_resize_when_the_mouse_up_is_missed() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        let resized = WindowId::new(1, 1);
        let frame = apps.windows[&resized].frame;
        let new_frame = CGRect::new(
            frame.origin,
            CGSize::new(frame.size.width - 100., frame.size.height),
        );
        apps.windows.get_mut(&resized).unwrap().frame = new_frame;
        reactor.handle_event(Event::WindowFrameChanged(
            resized,
            new_frame,
            apps.windows[&resized].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));
        let wids = written_wids(apps.requests());
        assert!(!wids.contains(&resized), "should be suppressed: {wids:?}");

        // No MouseUp arrives, but the next frame change reports the button up
        // and asks for a change the tree can't apply, so the model and the
        // window disagree.
        let odd_frame = CGRect::new(
            CGPoint::new(new_frame.origin.x + 10., new_frame.origin.y + 10.),
            CGSize::new(new_frame.size.width - 30., new_frame.size.height - 5.),
        );
        apps.windows.get_mut(&resized).unwrap().frame = odd_frame;
        reactor.handle_event(Event::WindowFrameChanged(
            resized,
            odd_frame,
            apps.windows[&resized].last_seen_txid,
            Requested(false),
            Some(MouseState::Up),
        ));
        let wids = written_wids(apps.requests());
        assert!(
            wids.contains(&resized),
            "button up should release the window: {wids:?}"
        );
    }

    fn written_wids(requests: Vec<Request>) -> Vec<WindowId> {
        requests
            .into_iter()
            .flat_map(|request| match request {
                Request::SetWindowFrame(wid, _, _) => vec![wid],
                Request::AnimationFrame { wid, .. } => vec![wid],
                _ => vec![],
            })
            .collect()
    }

    #[test]
    fn it_ignores_windows_on_nonzero_layers() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(vec![WindowServerInfo {
                id: WindowServerId::new(1),
                pid: 1,
                layer: 10,
                frame: CGRect::ZERO,
            }]),
        });

        reactor.handle_events(apps.make_app_without_ws_info(1, make_windows(1), None, true));

        let state_before = apps.windows.clone();
        let _events = apps.simulate_events();
        assert_eq!(state_before, apps.windows, "Window should not have been moved",);

        // Make sure it doesn't choke on destroyed events for ignored windows.
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 1)));
        reactor.handle_event(Event::WindowCreated(
            WindowId::new(1, 2),
            make_window(2),
            MouseState::Up,
        ));
        reactor.handle_event(Event::WindowDestroyed(WindowId::new(1, 2)));
    }

    #[test]
    fn handle_layout_response_groups_windows_by_app_and_screen() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;

        let screen1 = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let screen2 = CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen1, screen2],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1)), Some(SpaceId::new(2))],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(2)));

        let mut windows = make_windows(2);
        windows[1].frame.origin = CGPoint::new(1100., 100.);
        reactor.handle_events(apps.make_app(2, windows));

        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}

        reactor.handle_layout_response(layout::EventResponse {
            frame_overrides: vec![],
            raise_windows: vec![
                WindowId::new(1, 1),
                WindowId::new(1, 2),
                WindowId::new(2, 1),
                WindowId::new(2, 2),
            ],
            focus_window: None,
            ..Default::default()
        });
        let msg = raise_manager_rx.try_recv().expect("Should have sent an event").1;
        match msg {
            raise::Event::RaiseRequest(RaiseRequest {
                raise_windows, focus_window, ..
            }) => {
                let raise_windows: HashSet<Vec<WindowId>> = raise_windows.into_iter().collect();
                let expected = [
                    vec![WindowId::new(1, 1), WindowId::new(1, 2)],
                    vec![WindowId::new(2, 1)],
                    vec![WindowId::new(2, 2)],
                ]
                .into_iter()
                .collect();
                assert_eq!(raise_windows, expected);
                assert!(focus_window.is_none());
            }
            _ => panic!("Unexpected event: {msg:?}"),
        }
    }

    #[test]
    fn handle_layout_response_includes_handles_for_raise_and_focus_windows() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;

        reactor.handle_events(apps.make_app(1, make_windows(1)));
        reactor.handle_events(apps.make_app(2, make_windows(1)));

        let _events = apps.simulate_events();
        while raise_manager_rx.try_recv().is_ok() {}
        reactor.handle_layout_response(layout::EventResponse {
            frame_overrides: vec![],
            raise_windows: vec![WindowId::new(1, 1)],
            focus_window: Some(WindowId::new(2, 1)),
            ..Default::default()
        });
        let msg = raise_manager_rx.try_recv().expect("Should have sent an event").1;
        match msg {
            raise::Event::RaiseRequest(RaiseRequest { app_handles, .. }) => {
                assert!(app_handles.contains_key(&1));
                assert!(app_handles.contains_key(&2));
            }
            _ => panic!("Unexpected event: {msg:?}"),
        }
    }

    #[test]
    fn it_preserves_layout_after_login_screen() {
        // TODO: This would be better tested with a more complete simulation.
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(3),
            Some(WindowId::new(1, 1)),
            true,
        ));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        apps.simulate_until_quiet(&mut reactor);
        let default = reactor.layout.calculate_layout(space, full_screen, &reactor.config);

        assert!(reactor.layout.selected_window(space).is_some());
        reactor.handle_event(Event::Command(Command::Layout(LayoutCommand::MoveNode(
            Direction::Up,
        ))));
        apps.simulate_until_quiet(&mut reactor);
        let modified = reactor.layout.calculate_layout(space, full_screen, &reactor.config);
        assert_ne!(default, modified);

        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::ZERO],
            bounds: vec![],
            spaces: vec![None],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(
                (1..=3)
                    .map(|n| WindowServerInfo {
                        pid: 1,
                        id: WindowServerId::new(n),
                        layer: 0,
                        frame: CGRect::ZERO,
                    })
                    .collect(),
            ),
        });
        let requests = apps.requests();
        for request in requests {
            match request {
                Request::GetVisibleWindows => {
                    // Simulate the login screen condition: No windows are
                    // considered visible by the accessibility API, but they are
                    // from the window server API in the event above.
                    reactor.handle_event(Event::WindowsDiscovered {
                        pid: 1,
                        new: vec![],
                        known_visible: vec![],
                    });
                }
                req => {
                    let events = apps.simulate_events_for_requests(vec![req]);
                    for event in events {
                        reactor.handle_event(event);
                    }
                }
            }
        }
        apps.simulate_until_quiet(&mut reactor);

        assert_eq!(
            reactor.layout.calculate_layout(space, full_screen, &reactor.config),
            modified
        );
    }

    #[test]
    fn it_fixes_window_sizes_after_screen_config_changes() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1))],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        reactor.handle_events(apps.make_app(1, make_windows(1)));
        reactor.handle_event(Event::StartupComplete);

        let _events = apps.simulate_events();
        assert_eq!(
            full_screen,
            apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
        );

        // Simulate the system resizing a window after it recognizes an old
        // configurations. Resize events are not sent in this case.
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![
                full_screen,
                CGRect::new(CGPoint::new(1000., 0.), CGSize::new(1000., 1000.)),
            ],
            bounds: vec![],
            spaces: vec![Some(SpaceId::new(1)), None],
            scale_factors: vec![2.0, 2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(Event::WindowsOnScreenUpdated {
            pid: None,
            on_screen: WindowsOnScreen::new(vec![WindowServerInfo {
                id: WindowServerId::new(1),
                pid: 1,
                layer: 0,
                frame: CGRect::new(CGPoint::new(500., 0.), CGSize::new(500., 500.)),
            }]),
        });

        let _events = apps.simulate_events();
        assert_eq!(
            full_screen,
            apps.windows.get(&WindowId::new(1, 1)).expect("Window was not resized").frame,
        );
    }

    #[test]
    fn it_doesnt_crash_after_main_window_closes() {
        use Direction::*;
        use Event::*;
        use LayoutCommand::*;

        use super::Command::*;
        use super::Reactor;
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        reactor.handle_event(ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        assert_eq!(None, reactor.main_window());

        reactor.handle_event(ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));

        reactor.handle_event(WindowDestroyed(WindowId::new(1, 1)));
        reactor.handle_event(Command(Layout(MoveFocus(Left))));
    }

    #[test]
    fn move_focus_uses_active_screen_when_no_window_is_focused() {
        use Direction::*;
        use Event::*;
        use LayoutCommand::*;

        use super::Command::*;
        use super::Reactor;

        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        reactor.handle_event(ScreenParametersChanged {
            ids: vec![],
            frames: vec![CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));
        assert_eq!(reactor.main_window(), Some(WindowId::new(1, 1)));

        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;
        reactor.handle_event(ApplicationGloballyDeactivated(1));
        assert_eq!(reactor.main_window(), None);

        reactor.handle_event(Command(Layout(MoveFocus(Right))));

        let event = raise_manager_rx
            .try_recv()
            .expect("focus command should produce a raise request")
            .1;
        let raise::Event::RaiseRequest(RaiseRequest { focus_window, .. }) = event else {
            panic!("unexpected raise event: {event:?}");
        };
        assert_eq!(focus_window.map(|(wid, _)| wid), Some(WindowId::new(1, 2)));
    }

    #[test]
    fn it_follows_the_mouse_only_when_the_main_window_has_keyboard_focus() {
        use Event::*;

        let mut apps = Apps::new();
        let space = SpaceId::new(1);
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor.handle_event(ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_event(ApplicationGloballyActivated(1));
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(2),
            Some(WindowId::new(1, 1)),
            true,
        ));
        reactor.handle_events(apps.simulate_events());
        assert_eq!(reactor.main_window(), Some(WindowId::new(1, 1)));

        let (raise_manager_tx, mut raise_manager_rx) = mpsc::unbounded_channel();
        reactor.raise_manager_tx = raise_manager_tx;

        // Another process has keyboard focus, as when Spotlight is open.
        reactor.handle_event(MouseMovedOverWindow(WindowServerId::new(2), Some(2)));
        assert!(
            raise_manager_rx.try_recv().is_err(),
            "mouse move should be ignored while another process has keyboard focus"
        );

        // The main window's app has keyboard focus.
        reactor.handle_event(MouseMovedOverWindow(WindowServerId::new(2), Some(1)));
        let event = raise_manager_rx
            .try_recv()
            .expect("mouse move should produce a raise request")
            .1;
        let raise::Event::RaiseRequest(RaiseRequest { focus_window, .. }) = event else {
            panic!("unexpected raise event: {event:?}");
        };
        assert_eq!(focus_window.map(|(wid, _)| wid), Some(WindowId::new(1, 2)));

        // The key focus process could not be read.
        reactor.handle_event(MouseMovedOverWindow(WindowServerId::new(1), None));
        assert!(
            raise_manager_rx.try_recv().is_ok(),
            "mouse move should be followed when the key focus process is unknown"
        );
    }

    #[test]
    fn it_removes_terminated_app_windows_on_startup_complete() {
        use Event::*;

        let mut apps = Apps::new();
        let space = SpaceId::new(1);
        let full_screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));

        // First reactor: simulate the state before shutdown with three apps running
        let mut reactor1 = Reactor::new_for_test(LayoutManager::new_for_test());
        reactor1.handle_event(ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor1.handle_events(apps.make_app(1, make_windows(2)));
        reactor1.handle_events(apps.make_app(2, make_windows(2)));
        reactor1.handle_events(apps.make_app(3, make_windows(1)));
        apps.simulate_until_quiet(&mut reactor1);

        // Verify all 5 windows are in the layout
        let layout_before = reactor1.layout.calculate_layout(space, full_screen, &reactor1.config);
        assert_eq!(layout_before.len(), 5, "Expected 5 windows before shutdown");

        // Serialize the layout to simulate saving state before shutdown
        let serialized_layout = ron::ser::to_string(&reactor1.layout).unwrap();

        // Second reactor: simulate restore after reboot, where app 2 was terminated
        // and doesn't launch again
        let restored_layout: LayoutManager = ron::de::from_str(&serialized_layout).unwrap();
        let mut apps2 = Apps::new();
        let mut reactor2 = Reactor::new_for_test(restored_layout);
        reactor2.handle_event(ScreenParametersChanged {
            ids: vec![],
            frames: vec![full_screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        // Only apps 1 and 3 launch during restore (app 2 was terminated between save and restore)
        reactor2.handle_events(apps2.make_app(1, make_windows(2)));
        reactor2.handle_events(apps2.make_app(3, make_windows(1)));
        apps2.simulate_until_quiet(&mut reactor2);

        // Before StartupComplete, the layout still contains ghost nodes for app 2's windows
        let layout_before_cleanup =
            reactor2.layout.calculate_layout(space, full_screen, &reactor2.config);
        assert_eq!(layout_before_cleanup.len(), 5);

        // Send StartupComplete to trigger cleanup of terminated app windows
        reactor2.handle_event(StartupComplete);

        // After StartupComplete, verify that windows from terminated app 2 are removed
        // but windows from running apps 1 and 3 remain
        let windows_after = reactor2
            .layout
            .calculate_layout(space, full_screen, &reactor2.config)
            .into_iter()
            .map(|(wid, _)| wid)
            .sorted()
            .collect_vec();

        assert_eq!(
            windows_after,
            &[
                WindowId::new(1, 1),
                WindowId::new(1, 2),
                WindowId::new(3, 1),
            ]
        );
    }

    #[test]
    fn no_scroll_animation_when_idle() {
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });

        let mut apps = Apps::new();
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        assert!(
            !reactor.layout.has_active_scroll_animation(),
            "timer should be dormant when no scroll animation is active"
        );
    }

    #[test]
    fn it_locks_and_releases_size_shares_end_to_end() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1200., 1200.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app_with_opts(
            1,
            make_windows(3),
            Some(WindowId::new(1, 1)),
            true,
        ));
        reactor.handle_event(Event::StartupComplete);
        reactor.handle_event(Event::ApplicationGloballyActivated(1));
        apps.simulate_until_quiet(&mut reactor);

        let before = reactor.layout.calculate_layout(space, screen, &reactor.config);
        assert_eq!(before.len(), 3);

        let lock = || {
            Event::Command(Command::Layout(LayoutCommand::SetSizeShare(
                SizeShare::Fraction(0.5),
            )))
        };
        reactor.handle_event(lock());
        assert_eq!(
            vec![
                (
                    WindowId::new(1, 1),
                    CGRect::new(CGPoint::new(0., 0.), CGSize::new(600., 1200.)),
                ),
                (
                    WindowId::new(1, 2),
                    CGRect::new(CGPoint::new(600., 0.), CGSize::new(300., 1200.)),
                ),
                (
                    WindowId::new(1, 3),
                    CGRect::new(CGPoint::new(900., 0.), CGSize::new(300., 1200.)),
                ),
            ],
            reactor.layout.calculate_layout(space, screen, &reactor.config),
        );
        // The app threads must have been asked for the same frames.
        let requests = apps.requests();
        for (wid, frame) in reactor.layout.calculate_layout(space, screen, &reactor.config) {
            assert!(
                requests.iter().any(|request| {
                    matches!(request, Request::SetWindowFrame(request_wid, request_frame, _)
                        if *request_wid == wid && *request_frame == frame)
                }),
                "expected a frame request for {wid:?} at {frame:?}, got {requests:?}"
            );
        }

        // The same binding releases the lock.
        reactor.handle_event(lock());
        assert_eq!(
            before,
            reactor.layout.calculate_layout(space, screen, &reactor.config)
        );
    }

    #[test]
    fn fit_frame_to_screen_keeps_frames_in_view() {
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let no_min = CGSize::new(0., 0.);

        // A frame that fits is unchanged.
        assert_eq!(
            fit_frame_to_screen(
                CGRect::new(CGPoint::new(100., 100.), CGSize::new(200., 200.)),
                no_min,
                screen
            ),
            CGRect::new(CGPoint::new(100., 100.), CGSize::new(200., 200.))
        );

        // A screen that starts at a negative origin keeps the window inside
        // its own bounds.
        let left_screen = CGRect::new(CGPoint::new(-1200., -200.), CGSize::new(1200., 1000.));
        assert_eq!(
            fit_frame_to_screen(
                CGRect::new(CGPoint::new(-1400., -100.), CGSize::new(400., 400.)),
                no_min,
                left_screen
            ),
            CGRect::new(CGPoint::new(-1200., -100.), CGSize::new(400., 400.))
        );

        // Frames past the right and bottom edges are pulled back.
        assert_eq!(
            fit_frame_to_screen(
                CGRect::new(CGPoint::new(900., 900.), CGSize::new(200., 200.)),
                no_min,
                screen
            ),
            CGRect::new(CGPoint::new(800., 800.), CGSize::new(200., 200.))
        );

        // A minimum that doesn't fit the slot grows the window toward the
        // middle of the screen.
        assert_eq!(
            fit_frame_to_screen(
                CGRect::new(CGPoint::new(500., 0.), CGSize::new(500., 1000.)),
                CGSize::new(700., 0.),
                screen
            ),
            CGRect::new(CGPoint::new(300., 0.), CGSize::new(700., 1000.))
        );

        // A minimum larger than the screen is capped and pinned to the top
        // left corner.
        assert_eq!(
            fit_frame_to_screen(
                CGRect::new(CGPoint::new(100., 100.), CGSize::new(100., 100.)),
                CGSize::new(1200., 2000.),
                screen
            ),
            CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.))
        );
    }

    /// A window whose app refuses to shrink below a minimum gets enough room
    /// from its neighbors instead of spilling off the screen.
    #[test]
    fn it_gives_a_min_size_window_room_within_the_screen() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        // The app refuses to make this window narrower than 700 points.
        let constrained = WindowId::new(1, 2);
        apps.windows.get_mut(&constrained).unwrap().min_size = Some(CGSize::new(700.0, 0.0));

        // Nudge the window so the layout writes to it again and sees the clamp.
        let neighbor = WindowId::new(1, 1);
        nudge_window(&mut apps, &mut reactor, constrained);
        apps.simulate_until_quiet(&mut reactor);

        let frame = apps.windows[&constrained].frame;
        assert!(
            (frame.size.width - 700.0).abs() < 0.01,
            "window should keep its minimum width: {frame:?}"
        );
        assert!(
            frame.max().x <= screen.max().x + 0.01,
            "window should stay on screen: {frame:?}"
        );
        let neighbor_frame = apps.windows[&neighbor].frame;
        assert!(
            neighbor_frame.max().x <= frame.min().x + 0.01,
            "neighbor should give up the space: {neighbor_frame:?} {frame:?}"
        );
        assert_eq!(
            reactor.layout.window_min_size(constrained),
            Some(CGSize::new(700.0, 0.0))
        );
    }

    /// The minimum is learned from the app's response to a frame the layout
    /// asked for, without any other event to prompt a write.
    #[test]
    fn it_learns_a_min_size_from_a_rejected_frame() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        let constrained = WindowId::new(1, 2);
        apps.windows.get_mut(&constrained).unwrap().min_size = Some(CGSize::new(700.0, 0.0));

        // The user resizes the neighbor, so the layout writes both frames and
        // the app reports the constrained size back.
        let neighbor = WindowId::new(1, 1);
        apps.windows.get_mut(&neighbor).unwrap().frame.size.width -= 100.0;
        reactor.handle_event(Event::WindowFrameChanged(
            neighbor,
            apps.windows[&neighbor].frame,
            apps.windows[&neighbor].last_seen_txid,
            Requested(false),
            None,
        ));
        apps.simulate_until_quiet(&mut reactor);

        assert_eq!(
            reactor.layout.window_min_size(constrained),
            Some(CGSize::new(700.0, 0.0)),
            "the app's clamp should be learned"
        );
        let frame = apps.windows[&constrained].frame;
        assert!(
            frame.max().x <= screen.max().x + 0.01,
            "window should stay on screen after the correction: {frame:?}"
        );
    }

    /// A learned minimum is lowered again when the app later accepts a smaller
    /// frame, so it doesn't ratchet up forever.
    #[test]
    fn it_relaxes_a_min_size_when_the_app_accepts_less() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        let constrained = WindowId::new(1, 2);
        apps.windows.get_mut(&constrained).unwrap().min_size = Some(CGSize::new(700.0, 0.0));
        let neighbor = WindowId::new(1, 1);
        apps.windows.get_mut(&neighbor).unwrap().frame.size.width -= 100.0;
        reactor.handle_event(Event::WindowFrameChanged(
            neighbor,
            apps.windows[&neighbor].frame,
            apps.windows[&neighbor].last_seen_txid,
            Requested(false),
            None,
        ));
        apps.simulate_until_quiet(&mut reactor);
        assert_eq!(
            reactor.layout.window_min_size(constrained),
            Some(CGSize::new(700.0, 0.0))
        );

        // The app becomes willing to shrink again, and the user drags the
        // window smaller than the learned minimum.
        apps.windows.get_mut(&constrained).unwrap().min_size = None;
        apps.windows.get_mut(&constrained).unwrap().frame.size.width = 500.0;
        reactor.handle_event(Event::WindowFrameChanged(
            constrained,
            apps.windows[&constrained].frame,
            apps.windows[&constrained].last_seen_txid,
            Requested(false),
            Some(MouseState::Down),
        ));
        apps.simulate_until_quiet(&mut reactor);

        assert_eq!(
            reactor.layout.window_min_size(constrained),
            Some(CGSize::new(500.0, 0.0)),
            "the minimum should follow the app's accepted size"
        );
    }

    /// When all windows have minimums that don't fit, they overlap but stay
    /// within the screen instead of spilling past its edges.
    #[test]
    fn it_keeps_constrained_windows_on_screen_when_they_overlap() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space = SpaceId::new(1);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        reactor.handle_event(Event::StartupComplete);
        apps.simulate_until_quiet(&mut reactor);

        let w1 = WindowId::new(1, 1);
        let w2 = WindowId::new(1, 2);
        for wid in [w1, w2] {
            apps.windows.get_mut(&wid).unwrap().min_size = Some(CGSize::new(700.0, 0.0));
        }

        nudge_window(&mut apps, &mut reactor, w1);
        apps.simulate_until_quiet(&mut reactor);

        let f1 = apps.windows[&w1].frame;
        let f2 = apps.windows[&w2].frame;
        for frame in [f1, f2] {
            assert!(
                (frame.size.width - 700.0).abs() < 0.01,
                "window should keep its minimum width: {frame:?}"
            );
            assert!(
                frame.max().x <= screen.max().x + 0.01,
                "window should stay on screen: {frame:?}"
            );
        }
        assert!(
            f2.min().x < f1.max().x && f1.min().x < f2.max().x,
            "windows should overlap instead of spilling off screen: {f1:?} {f2:?}"
        );
    }

    /// R28. Without contexts, every Space the reactor exposes shows
    /// Everything's layout, so no context layout is ever made.
    #[test]
    fn it_shows_everything_on_every_space_without_contexts() {
        let mut apps = Apps::new();
        let mut reactor = Reactor::new_for_test(LayoutManager::new_for_test());
        let space1 = SpaceId::new(1);
        let space2 = SpaceId::new(2);
        let screen = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 1000.));
        let shorter = CGRect::new(CGPoint::new(0., 0.), CGSize::new(1000., 900.));
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![screen],
            bounds: vec![],
            spaces: vec![Some(space1)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        reactor.handle_events(apps.make_app(1, make_windows(2)));
        apps.simulate_until_quiet(&mut reactor);
        reactor.handle_event(Event::SpaceChanged(
            vec![Some(space2)],
            WindowsOnScreen::new(vec![]),
        ));
        apps.simulate_until_quiet(&mut reactor);
        reactor.handle_event(Event::ScreenParametersChanged {
            ids: vec![],
            frames: vec![shorter],
            bounds: vec![],
            spaces: vec![Some(space1)],
            scale_factors: vec![2.0],
            converter: CoordinateConverter::default(),
            on_screen: Default::default(),
        });
        apps.simulate_until_quiet(&mut reactor);

        assert!(reactor.layout.serialize_to_string().contains(",context_layouts:{},"));
        let mut frames = reactor.layout.calculate_layout(space1, shorter, &reactor.config);
        frames.sort_by_key(|&(wid, _)| wid);
        assert_eq!(
            vec![
                (
                    WindowId::new(1, 1),
                    CGRect::new(CGPoint::new(0., 0.), CGSize::new(500., 900.))
                ),
                (
                    WindowId::new(1, 2),
                    CGRect::new(CGPoint::new(500., 0.), CGSize::new(500., 900.))
                ),
            ],
            frames
        );
    }

    /// Moves the window slightly without the mouse, as an app might, so the
    /// next layout pass has something to correct.
    fn nudge_window(apps: &mut Apps, reactor: &mut Reactor, wid: WindowId) {
        let window = apps.windows.get_mut(&wid).unwrap();
        window.frame.origin.x += 10.0;
        let frame = window.frame;
        let last_seen = window.last_seen_txid;
        reactor.handle_event(Event::WindowFrameChanged(
            wid,
            frame,
            last_seen,
            Requested(false),
            None,
        ));
    }

    /// The contexts as the status item's title and menu receive them.
    mod menu_bar {
        use std::sync::{Arc, Mutex};

        use pretty_assertions::assert_eq;
        use test_log::test;

        use super::super::create_context::tests::{Setup, space, wid};
        use super::super::{ContextCommand, ContextRef, Event, Reactor};
        use crate::actor::contexts_snapshot::{ContextsSnapshot, ScreenContext};
        use crate::model::contexts::ContextKey;
        use crate::sys::window_server::{WindowServerId, WindowServerInfo, WindowsOnScreen};
        use crate::ui::status_bar::ContextMenuKeys;
        use crate::ui::status_bar::context_menu::{MenuAction, MenuEntry, context_menu};

        type Published = Arc<Mutex<Vec<Arc<ContextsSnapshot>>>>;

        /// Keeps every snapshot that the reactor publishes from now on.
        fn capture(reactor: &mut Reactor) -> Published {
            let published = Published::default();
            let sink = published.clone();
            reactor.publish_contexts =
                Box::new(move |snapshot| sink.lock().unwrap().push(snapshot));
            published
        }

        fn count(published: &Published) -> usize {
            published.lock().unwrap().len()
        }

        fn last(published: &Published) -> ContextsSnapshot {
            (**published.lock().unwrap().last().unwrap()).clone()
        }

        /// A window server snapshot that lists app 1's windows at their
        /// frames.
        fn listed(s: &Setup, idxs: &[u32]) -> WindowsOnScreen {
            WindowsOnScreen::new(
                idxs.iter()
                    .map(|&idx| WindowServerInfo {
                        id: WindowServerId::new(idx),
                        pid: 1,
                        layer: 0,
                        frame: s.apps.windows[&wid(idx)].frame,
                    })
                    .collect(),
            )
        }

        /// Menu bar, R29. When the windows of an active Unsorted all join a
        /// context, Unsorted isn't listed any more. The menu built from the
        /// published snapshot then offers no Unsorted item, whose switch the
        /// reactor would refuse, and the reactor refuses that switch.
        #[test]
        fn the_menu_offers_no_unsorted_item_while_unsorted_is_not_listed() {
            let mut s = Setup::new(2);
            let work = s.reactor.contexts.create("Unsorted work").unwrap();
            let desc = s.reactor.window_desc(wid(1)).unwrap();
            s.reactor.contexts.add_window(work, &desc).unwrap();
            let unsorted = || ContextCommand::SwitchContext(ContextRef::Name("Unsorted".into()));
            s.run(unsorted());
            assert_eq!(ContextKey::Unsorted, s.reactor.contexts.active());
            assert_eq!(vec![wid(1)], s.parked());
            let desc = s.reactor.window_desc(wid(2)).unwrap();
            s.reactor.contexts.add_window(work, &desc).unwrap();
            let published = capture(&mut s.reactor);

            s.reactor.handle_event(Event::MouseUp);

            let snapshot = last(&published);
            assert_eq!(ContextKey::Unsorted, snapshot.active);
            assert!(!snapshot.unsorted.listed);
            let entries = context_menu(&snapshot, &ContextMenuKeys::default(), |action| {
                action.command().is_some()
            });
            let switches: Vec<(&str, bool)> = entries
                .iter()
                .filter_map(|entry| match entry {
                    MenuEntry::Item(item) if matches!(item.action, MenuAction::Switch(_)) => {
                        Some((item.title.as_str(), item.checked))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                vec![("Unsorted work", false), ("Show Everything", false)],
                switches
            );
            let used = s.reactor.contexts.last_used(ContextKey::Unsorted);
            s.run(unsorted());
            assert_eq!(used, s.reactor.contexts.last_used(ContextKey::Unsorted));
            assert_eq!(vec![wid(1)], s.parked());
        }

        /// Menu bar, with the coordinator's decision for a desktop without
        /// a managed Space. When no screen shows a managed Space any more,
        /// the reactor publishes a snapshot without screens, and the active
        /// context stays, so the title can tell that no Space shows it.
        /// This holds for a Space change from the login window, and for
        /// R33's path, where the Space shows Everything before it is turned
        /// off.
        #[test]
        fn leaving_every_managed_space_publishes_a_snapshot_without_screens() {
            let mut s = Setup::new(2);
            s.create("Work");
            let work = ContextKey::Named(s.id("Work"));
            let shows = |key| vec![ScreenContext { id: 1, shows: key }];
            let all = listed(&s, &[1, 2]);
            let published = capture(&mut s.reactor);

            s.reactor.handle_event(Event::SpaceChanged(vec![None], all.clone()));

            assert_eq!(1, count(&published));
            let login_window = last(&published);
            assert_eq!(Vec::<ScreenContext>::new(), login_window.screens);
            assert_eq!(work, login_window.active);

            s.reactor.handle_event(Event::SpaceChanged(vec![Some(space())], all.clone()));
            s.apps.simulate_until_quiet(&mut s.reactor);
            assert_eq!(shows(work), last(&published).screens);
            s.reactor.handle_event(Event::ShowEverythingOn(vec![space()]));
            s.apps.simulate_until_quiet(&mut s.reactor);
            assert_eq!(shows(ContextKey::Everything), last(&published).screens);
            let before = count(&published);

            s.reactor.handle_event(Event::SpaceChanged(vec![None], all));

            assert_eq!(before + 1, count(&published));
            let turned_off = last(&published);
            assert_eq!(Vec::<ScreenContext>::new(), turned_off.screens);
            assert_eq!(work, turned_off.active);
        }
    }
}
