// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! This tool is used to exercise glide and system APIs during development.

use std::cell::{Cell, RefCell};
use std::env::VarError;
use std::future::Future;
use std::path::PathBuf;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::time::{Duration, Instant};

use accessibility::{AXUIElement, AXUIElementAttributes};
use accessibility_sys::pid_t;
use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use livesplit_hotkey::{ConsumePreference, Modifiers};
use objc2_app_kit::{
    NSRunningApplication, NSScreen, NSWindow, NSWindowNumberListOptions, NSWorkspace,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    CGDisplayBounds, CGMainDisplayID, CGWindowID, CGWindowListCopyWindowInfo, CGWindowListOption,
    kCGNullWindowID,
};
use objc2_foundation::{MainThreadMarker, NSString};
use sugarglider::actor::{self, reactor};
use sugarglider::sys::app::{AXUIElementExt, AppInfo, NSRunningApplicationExt, WindowInfo};
use sugarglider::sys::event::{self, get_mouse_pos};
use sugarglider::sys::executor::Executor;
use sugarglider::sys::screen::{self, ScreenCache};
use sugarglider::sys::window_server::{
    self, SkylightConnection, WindowServerId, get_window, kCGSWindowCreated,
};
use sugarglider::sys::{self};
use tokio::sync::mpsc::{self, UnboundedReceiver, unbounded_channel};
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser)]
struct Opt {
    #[arg(long)]
    bundle: Option<String>,
    #[command(subcommand)]
    command: Command,
    #[arg(long)]
    verbose: bool,
}

#[derive(Subcommand, Clone)]
enum Command {
    #[command(subcommand)]
    List(List),
    #[command(subcommand)]
    App(App),
    #[command(subcommand)]
    WindowServer(WindowServer),
    #[command()]
    Replay(Replay),
    #[command(subcommand)]
    Mouse(Mouse),
    #[command()]
    Inspect,
    /// Print every signal the system exposes about which app and window has
    /// keyboard focus, whenever one of them changes.
    #[command()]
    Focus {
        /// Sampling interval in milliseconds.
        #[arg(long, default_value_t = 200)]
        interval_ms: u64,
    },
    /// Observe native tab identities without selecting tabs or moving windows.
    NativeTabs {
        pid: pid_t,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=1000))]
        samples: u32,
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(50..=10000))]
        interval_ms: u64,
    },
}

#[derive(Subcommand, Clone)]
enum List {
    All,
    Apps,
    Ax,
    Cg,
    Ns,
    Spaces,
}

#[derive(Subcommand, Clone)]
enum App {
    #[command()]
    SetMainWindow {
        pid: pid_t,
        window_server_id: CGWindowID,
        #[arg(long)]
        wait: bool,
    },
    #[command()]
    ReadMainWindow {
        pid: pid_t,
        #[arg(long)]
        wait: bool,
    },
    #[command()]
    Run { pid_or_bundle: String },
}

#[derive(Subcommand, Clone)]
enum WindowServer {
    #[command()]
    List {
        #[arg(short, long)]
        all: bool,
        /// Whether to show the raw window dictionaries. Implies --all.
        #[arg(short, long)]
        raw: bool,
    },
    #[command()]
    Get { id: u32 },
    /// Subscribe to a range of window server notification events and print
    /// each one as it arrives, to find which event a system action produces.
    #[command()]
    Watch {
        #[arg(long, default_value_t = 1)]
        from: u32,
        #[arg(long, default_value_t = 2000)]
        to: u32,
        /// Events to leave unregistered, for when one of them is noisy.
        #[arg(long, value_delimiter = ',')]
        skip: Vec<u32>,
        /// Which windows to request notifications for.
        #[arg(long, value_enum, default_value_t = NotifyWindows::All)]
        windows: NotifyWindows,
        /// Additional window ids to request notifications for.
        #[arg(long, value_delimiter = ',')]
        window: Vec<u32>,
    },
}

/// Which windows to request notifications for. Most per-window events are only
/// delivered for windows the connection has requested; a few, notably window
/// creation, arrive for every window either way.
#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
enum NotifyWindows {
    /// Only the windows named by --window.
    None,
    /// Windows that are on screen when the command starts.
    Existing,
    /// Windows created while the command runs.
    New,
    /// Both existing and new windows.
    All,
}

impl NotifyWindows {
    fn existing(self) -> bool {
        matches!(self, NotifyWindows::Existing | NotifyWindows::All)
    }

    fn new(self) -> bool {
        matches!(self, NotifyWindows::New | NotifyWindows::All)
    }
}

#[derive(Parser, Clone)]
struct Replay {
    path: PathBuf,
}

#[derive(Subcommand, Clone)]
enum Mouse {
    #[command()]
    Clicks,
    #[command()]
    Hide,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Set default log level to info.
    if let Err(VarError::NotPresent) = std::env::var("RUST_LOG") {
        // SAFETY: No other threads running.
        unsafe {
            std::env::set_var("RUST_LOG", "info");
        }
    }
    tracing_subscriber::registry()
        .with(sugarglider::log::tree_layer())
        .with(EnvFilter::from_default_env())
        .init();
    let opt: Opt = Parser::parse();

    match opt.command {
        Command::List(List::Ax) => {
            time("accessibility", || get_windows_with_ax(&opt, false, true)).await
        }
        Command::List(List::Cg) => time("core-graphics", || get_windows_with_cg(&opt, true)).await,
        Command::List(List::Ns) => time("ns-window", || get_windows_with_ns(&opt, true)).await,
        Command::List(List::Apps) => get_apps(&opt),
        Command::List(List::All) => {
            //time("accessibility serial", || get_windows_with_ax(&opt, true)).await;
            time("core-graphics", || get_windows_with_cg(&opt, true)).await;
            time("ns-window", || get_windows_with_ns(&opt, true)).await;
            time("accessibility", || get_windows_with_ax(&opt, false, true)).await;
            time("core-graphics second time", || get_windows_with_cg(&opt, false)).await;
            time("ns-window second time", || get_windows_with_ns(&opt, false)).await;
            time("accessibility second time", || {
                get_windows_with_ax(&opt, false, false)
            })
            .await;
        }
        Command::List(List::Spaces) => {
            println!("Current space: {:?}", screen::diagnostic::cur_space());
            println!("Visible spaces: {:?}", screen::diagnostic::visible_spaces());
            println!("All spaces: {:?}", screen::diagnostic::all_spaces());
            println!(
                "Managed display spaces: {:?}",
                screen::diagnostic::managed_display_spaces()
            );

            dbg!(screen::diagnostic::managed_displays());
            let screens = NSScreen::screens(MainThreadMarker::new().unwrap());
            let frames: Vec<_> = screens.iter().map(|screen| screen.visibleFrame()).collect();
            println!("NSScreen sizes: {frames:?}");

            println!();
            let mtm = MainThreadMarker::new().unwrap();
            let mut sc = ScreenCache::new();
            let ns_screens = screen::get_ns_screens(mtm);
            println!("Frames: {:?}", sc.update_screen_config(ns_screens));
            println!("Spaces: {:?}", sc.get_screen_spaces());
        }
        Command::App(App::SetMainWindow { pid, window_server_id, wait }) => {
            let window = find_window(pid, window_server_id)?;
            if wait {
                println!("Press enter to complete action");
                std::io::stdin().read_line(&mut String::new())?;
                window.set_messaging_timeout(3600.0)?;
            }
            window.set_main(true).context("Failed to set window as main")?;
        }
        Command::App(App::ReadMainWindow { pid, wait }) => {
            let app = AXUIElement::application(pid);
            println!("frontmost = {:?}", &*app.frontmost()?);
            let main_window = if opt.verbose {
                let main_window = app.main_window()?;
                dbg!(&*main_window);
                let main_window_id: WindowServerId = (&*app.main_window()?).try_into()?;
                dbg!(main_window_id);
                main_window
            } else {
                let main_window = app.main_window();
                println!("main_window = {:?}", main_window.as_ref().map(|_| ()));
                // let main_window_id: WindowServerId = (&app.main_window()?).try_into()?;
                // dbg!(main_window_id);
                main_window?
            };
            if wait {
                println!("Press enter to complete action");
                std::io::stdin().read_line(&mut String::new())?;
                app.set_messaging_timeout(3600.0)?;
            }
            dbg!(&*main_window.main()?);
        }
        Command::App(App::Run { pid_or_bundle }) => {
            run_app_actor(pid_or_bundle).await?;
        }
        Command::WindowServer(WindowServer::List { all, raw }) => {
            if raw {
                for window in window_server::get_visible_windows_raw().iter() {
                    println!("{window:?}");
                }
            } else {
                let layer = if all { None } else { Some(0) };
                for window in window_server::get_visible_windows_with_layer(layer) {
                    println!("{window:?}");
                }
            }
        }
        Command::WindowServer(WindowServer::Get { id }) => {
            match window_server::get_window(WindowServerId(id)) {
                Some(win) => println!("{win:?}"),
                None => println!("Could not find window {id}"),
            }
        }
        Command::WindowServer(WindowServer::Watch {
            from,
            to,
            skip,
            windows,
            window,
        }) => watch_notifications(
            from,
            to,
            &skip,
            windows,
            &window,
            MainThreadMarker::new().unwrap(),
        ),
        Command::Replay(Replay { path }) => {
            reactor::replay(&path, |_span, request| {
                info!(?request);
            })?;
        }
        Command::Mouse(Mouse::Clicks) => {
            use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
            use core_graphics::event::{
                CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
            };
            let current = CFRunLoop::get_current();
            match CGEventTap::new(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                vec![CGEventType::LeftMouseUp],
                |_a, _b, d| {
                    println!("{:?}", d.location());
                    core_graphics::event::CallbackResult::Keep
                },
            ) {
                Ok(tap) => unsafe {
                    let loop_source = tap.mach_port().create_runloop_source(0).unwrap();
                    current.add_source(&loop_source, kCFRunLoopCommonModes);
                    tap.enable();
                    CFRunLoop::run_current();
                },
                Err(_) => assert!(false),
            }
        }
        Command::Mouse(Mouse::Hide) => {
            window_server::allow_hide_mouse().unwrap();
            event::hide_mouse().unwrap();

            println!("Press enter to show");
            std::io::stdin().read_line(&mut String::new())?;
            event::show_mouse().unwrap();

            println!("Press enter to exit");
            std::io::stdin().read_line(&mut String::new())?;
        }
        Command::Inspect => inspect(MainThreadMarker::new().unwrap()),
        Command::Focus { interval_ms } => watch_focus(Duration::from_millis(interval_ms)),
        Command::NativeTabs { pid, samples, interval_ms } => {
            let app = AXUIElement::application(pid);
            let mut tracker = sugarglider::model::native_tabs::TabTracker::<
                objc2_core_foundation::CFRetained<AXUIElement>,
            >::default();
            for sample in 0..samples {
                for window in app.windows()?.iter() {
                    let id = WindowServerId::try_from(&*window)?;
                    let wid = actor::app::WindowId::with_wsid(pid, id);
                    let known = tracker.bar_for_window(wid).map(|bar| &**bar);
                    match sys::native_tabs::read(&window, known) {
                        Ok(Some(bar)) => {
                            let group = tracker
                                .observe(wid, bar.element, bar.tabs, bar.selected)
                                .context("invalid native tab identities")?;
                            println!("sample={sample} {}", serde_json::to_string(&group)?);
                        }
                        Ok(None) => {
                            tracker.forget_window(wid);
                            println!("sample={sample} standalone={wid:?}");
                        }
                        Err(error) => {
                            println!("sample={sample} unavailable={wid:?} error={error}");
                        }
                    }
                }
                if sample + 1 < samples {
                    tokio::time::sleep(Duration::from_millis(interval_ms)).await;
                }
            }
        }
    }
    Ok(())
}

fn find_window(
    pid: pid_t,
    window_server_id: CGWindowID,
) -> anyhow::Result<CFRetained<AXUIElement>> {
    let app = AXUIElement::application(pid);
    let windows = app.windows()?;
    let window = windows
        .iter()
        .filter(|w| {
            let id: Result<window_server::WindowServerId, _> = (&**w).try_into();
            id.is_ok_and(|id| id.as_u32() == window_server_id)
        })
        .next()
        .context("Could not find matching window")?;
    Ok(window.clone())
}

/// Subscribes to every window server notification event in a range and prints
/// them as they arrive.
fn watch_notifications(
    from: u32,
    to: u32,
    skip: &[u32],
    windows: NotifyWindows,
    extra_windows: &[u32],
    mtm: MainThreadMarker,
) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    // Notifications are only delivered once AppKit is running its event loop.
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);

    let connection = Rc::new(RefCell::new(SkylightConnection::new(mtm)));
    let requested = Rc::new(Cell::new(0));
    let request = {
        let (connection, requested) = (connection.clone(), requested.clone());
        move |wsid: WindowServerId| match connection.borrow_mut().add_window(wsid) {
            Ok(()) => requested.set(requested.get() + 1),
            Err(e) => println!("Could not request notifications for {wsid:?}: {e}"),
        }
    };

    for window in extra_windows {
        request(WindowServerId::new(*window));
    }
    if windows.existing() {
        for window in window_server::get_visible_window_ids() {
            request(window);
        }
    }
    let new_window_notifier = windows.new().then(|| {
        window_server::on_global_event(kCGSWindowCreated, move |_, data| {
            if let Some(wsid) = notification_window(data) {
                request(wsid);
            }
        })
        .expect("Subscribing to window creation")
    });

    let start = Instant::now();
    let print = move |source: &str, event: u32, data: &[u8]| {
        let bytes = data.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let window = match notification_window(data) {
            Some(wsid) => wsid.as_u32().to_string(),
            None => "-".to_string(),
        };
        println!(
            "[{:>7.2}s] {source} event={event} window={window} len={} bytes={bytes}",
            start.elapsed().as_secs_f64(),
            data.len(),
        );
    };
    let mut notifiers = Vec::new();
    let mut connection_notifiers = Vec::new();
    for event in from..=to {
        if skip.contains(&event) {
            continue;
        }
        if let Ok(notifier) =
            window_server::on_global_event(event, move |event, data| print("global", event, data))
        {
            notifiers.push(notifier);
        }
        if let Ok(notifier) = connection
            .borrow()
            .on_event(event, move |event, data| print("connection", event, data))
        {
            connection_notifiers.push(notifier);
        }
    }
    println!(
        "Subscribed to {} global and {} connection events in {from}..={to}, \
         requesting notifications for {} window(s){}. Ctrl-C to exit.",
        notifiers.len(),
        connection_notifiers.len(),
        requested.get(),
        if new_window_notifier.is_some() {
            " plus new ones"
        } else {
            ""
        },
    );
    app.run();
}

/// Reads the window id out of a notification payload that carries one.
fn notification_window(data: &[u8]) -> Option<WindowServerId> {
    let id = u32::from_ne_bytes(<[u8; 4]>::try_from(data).ok()?);
    Some(WindowServerId::new(id))
}

/// Polls each source of "what has keyboard focus" and prints them when any of
/// them changes.
fn watch_focus(interval: Duration) {
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};

    let system_wide = AXUIElement::system_wide();
    // A hung app must not stall the probe.
    _ = system_wide.set_messaging_timeout(0.25);
    let workspace = NSWorkspace::sharedWorkspace();

    println!("Watching focus. Open Spotlight, a launcher, a menu, etc. Ctrl-C to exit.");
    let start = Instant::now();
    let mut last = String::new();
    let mut query_times = Vec::new();
    let mut last_summary = Instant::now();
    loop {
        let (sample, timings) = sample_focus(&workspace, &system_wide);
        query_times.push(timings.ax_query);
        if sample != last {
            println!(
                "\n[{:>7.2}s] (sls.key_focus {:.2}ms, ax.focus_query {:.2}ms)\n{sample}",
                start.elapsed().as_secs_f64(),
                timings.key_focus.as_secs_f64() * 1000.,
                timings.ax_query.as_secs_f64() * 1000.,
            );
            last = sample;
        }
        if last_summary.elapsed() > Duration::from_secs(5) {
            query_times.sort();
            println!(
                "ax.focus_query over {} samples: min {:.2}ms median {:.2}ms max {:.2}ms",
                query_times.len(),
                query_times[0].as_secs_f64() * 1000.,
                query_times[query_times.len() / 2].as_secs_f64() * 1000.,
                query_times[query_times.len() - 1].as_secs_f64() * 1000.,
            );
            query_times.clear();
            last_summary = Instant::now();
        }
        // NSWorkspace learns about activation from the run loop.
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, interval, false);
    }
}

struct Timings {
    key_focus: Duration,
    ax_query: Duration,
}

/// Returns the sample, and how long the window server and accessibility
/// queries took. The timings are separate so that they don't count as a change
/// in the sample.
fn sample_focus(workspace: &NSWorkspace, system_wide: &AXUIElement) -> (String, Timings) {
    use std::fmt::Write;

    let mut out = String::new();
    let app = |app: Option<objc2::rc::Retained<NSRunningApplication>>| match app {
        None => "<none>".to_string(),
        Some(app) => describe_pid(app.pid()),
    };
    _ = writeln!(
        out,
        "  ws.frontmost      = {}",
        app(workspace.frontmostApplication())
    );
    _ = writeln!(
        out,
        "  ws.menu_bar_owner = {}",
        app(workspace.menuBarOwningApplication())
    );
    let pid = |pid: Option<pid_t>| match pid {
        None => "<error>".to_string(),
        Some(pid) => describe_pid(pid),
    };
    _ = writeln!(
        out,
        "  sls.front_process = {}",
        pid(window_server::get_front_process_pid())
    );
    let sls_start = Instant::now();
    let key_focus = window_server::get_key_focus_pid();
    let sls_time = sls_start.elapsed();
    _ = writeln!(out, "  sls.key_focus     = {}", pid(key_focus));

    // The chain a real implementation would use to answer "which window has
    // keyboard focus", timed on its own.
    let ax_start = Instant::now();
    let focused_wsid = system_wide
        .focused_application()
        .and_then(|app| app.focused_window())
        .and_then(|window| Ok(WindowServerId::try_from(&*window)?));
    let ax_time = ax_start.elapsed();
    _ = writeln!(
        out,
        "  ax.focus_query    = {}",
        match focused_wsid {
            Ok(wsid) => format!("wsid={}", wsid.as_u32()),
            Err(e) => format!("<error: {e}>"),
        }
    );

    match system_wide.focused_application() {
        Err(e) => _ = writeln!(out, "  ax.focused_app    = <error: {e}>"),
        Ok(focused_app) => {
            _ = writeln!(
                out,
                "  ax.focused_app    = {} (AXFrontmost {})",
                match focused_app.pid() {
                    Ok(pid) => describe_pid(pid),
                    Err(e) => format!("<error: {e}>"),
                },
                focused_app
                    .frontmost()
                    .map(|b| b.value().to_string())
                    .unwrap_or_else(|e| format!("<error: {e}>")),
            );
            _ = writeln!(
                out,
                "  ax.focused_window = {}",
                describe_window(focused_app.focused_window())
            );
            _ = writeln!(
                out,
                "  ax.main_window    = {}",
                describe_window(focused_app.main_window())
            );
        }
    }
    let focused_element_window = system_wide.focused_ui_element().and_then(|elem| elem.window());
    _ = writeln!(
        out,
        "  ax.focused_elem   = {}",
        describe_window(focused_element_window)
    );
    (
        out,
        Timings {
            key_focus: sls_time,
            ax_query: ax_time,
        },
    )
}

fn describe_pid(pid: pid_t) -> String {
    let bundle_id = NSRunningApplication::with_process_id(pid)
        .and_then(|app| app.bundle_id())
        .map(|id| id.to_string())
        .unwrap_or_else(|| "<no bundle id>".to_string());
    format!("{pid} {bundle_id}")
}

fn describe_window(window: Result<CFRetained<AXUIElement>, accessibility::Error>) -> String {
    let window = match window {
        Ok(window) => window,
        Err(e) => return format!("<error: {e}>"),
    };
    let attr = |value: Result<CFRetained<objc2_core_foundation::CFString>, _>| {
        value.map(|v| v.to_string()).unwrap_or_else(|_| "<none>".to_string())
    };
    let wsid = WindowServerId::try_from(&*window);
    let level = wsid.as_ref().ok().and_then(|id| get_window(*id)).map(|info| info.layer);
    format!(
        "wsid={} level={} role={}/{} title={:?}",
        wsid.map(|id| id.as_u32().to_string()).unwrap_or_else(|_| "<none>".to_string()),
        level.map(|l| l.to_string()).unwrap_or_else(|| "<none>".to_string()),
        attr(window.role()),
        attr(window.subrole()),
        attr(window.title()),
    )
}

async fn run_app_actor(pid_or_bundle: String) -> anyhow::Result<()> {
    let pid = match pid_or_bundle.parse() {
        Ok(pid) => pid,
        Err(_) => {
            let apps = NSRunningApplication::runningApplicationsWithBundleIdentifier(
                &NSString::from_str(&pid_or_bundle),
            );
            match apps.len() {
                0 => bail!("Could not find any applications with bundle id {pid_or_bundle}"),
                1 => apps.firstObject().unwrap().pid(),
                _ => bail!("Found multiple applications with bundle id {pid_or_bundle}"),
            }
        }
    };
    let info = AppInfo::from(
        &*NSRunningApplication::with_process_id(pid).expect("Could not get running application"),
    );
    let (ws_tx, mut ws_rx) = actor::channel();
    actor::app::spawn_app_thread(pid, info, ws_tx, None);
    while let Some((span, event)) = ws_rx.recv().await {
        let _sp = span.enter();
        info!("{event:?}");
    }
    Ok(())
}

fn inspect(mtm: MainThreadMarker) {
    let (tx, rx) = unbounded_channel();
    let hook =
        livesplit_hotkey::Hook::with_consume_preference(ConsumePreference::MustConsume).unwrap();
    let key = event::Hotkey {
        key_code: event::KeyCode::KeyI,
        modifiers: Modifiers::ALT | Modifiers::SHIFT,
    };
    hook.register(key, move || _ = tx.send(())).unwrap();
    println!("Press {key:?} to inspect the window under the mouse");
    Executor::run(inspect_inner(rx, mtm));
}

async fn inspect_inner(mut rx: UnboundedReceiver<()>, mtm: MainThreadMarker) {
    let mut screen_cache = ScreenCache::new();
    let ns_screens = screen::get_ns_screens(mtm);
    let Some((_, converter)) = screen_cache.update_screen_config(ns_screens) else {
        return;
    };
    while let Some(()) = rx.recv().await {
        println!();
        let Some(pos) = get_mouse_pos(converter) else { continue };
        // This API doesn't always work, but for some reason get_window_at_point
        // *never* works from devtool.
        let mut element: *const accessibility_sys::AXUIElement = ptr::null();
        let err = unsafe {
            AXUIElement::system_wide().as_sys().copy_element_at_position(
                pos.x as f32,
                pos.y as f32,
                NonNull::new_unchecked(&mut element),
            )
        };
        if let Some(err) = accessibility::AXError::from_raw(err) {
            println!("Failed to get element under cursor: {err}");
            continue;
        }
        // SAFETY: `element` is non-null on success, and owned per the copy rule.
        let elem = unsafe {
            CFRetained::cast_unchecked::<AXUIElement>(CFRetained::from_raw(NonNull::new_unchecked(
                element.cast_mut(),
            )))
        };
        let ax_window = match elem.window() {
            Ok(win) => win,
            Err(e) => {
                println!(
                    "Warning: no window for element {element:#?} ({e}); inspecting the element"
                );
                elem.clone()
            }
        };
        println!("{:#?}", ax_window.privacy_sensitive_inspect());
        let Some(info) =
            WindowServerId::try_from(&*ax_window).ok().and_then(|wsid| get_window(wsid))
        else {
            println!("Couldn't get window server info for {element:?}");
            continue;
        };
        println!("{info:#?}");
        if let Some(app) = NSRunningApplication::with_process_id(info.pid)
            && let Some(bundle_id) = app.bundleIdentifier()
        {
            println!("bundle_id: {:?}", bundle_id);
        }
    }
}

async fn get_windows_with_cg(opt: &Opt, print: bool) {
    let windows =
        CGWindowListCopyWindowInfo(CGWindowListOption::OptionOnScreenOnly, kCGNullWindowID)
            .expect("CGWindowListCopyWindowInfo returned NULL");
    if print && opt.verbose {
        println!("{windows:?}");
    }
    if print {
        println!("visible window ids:");
        for window in window_server::get_visible_windows_with_layer(None) {
            if opt.verbose {
                println!("- {window:?}");
            } else {
                println!("- {id:?}, pid={pid:?}", id = window.id, pid = window.pid);
            }
        }
    }
    let display_id = CGMainDisplayID();
    let screen = CGDisplayBounds(display_id);
    if print {
        println!("main display = {screen:?}");
    }
}

async fn get_windows_with_ns(_opt: &Opt, print: bool) {
    let mtm = MainThreadMarker::new().unwrap();
    let windows =
        NSWindow::windowNumbersWithOptions(NSWindowNumberListOptions::AllApplications, mtm);
    if print {
        println!("{windows:?}");
    }
}

async fn get_windows_with_ax(opt: &Opt, serial: bool, print: bool) {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let targeted_list = targeted_ax_list(opt);
    let mut matched_apps = 0;
    for (pid, bundle_id) in sys::app::running_apps(opt.bundle.clone()) {
        matched_apps += 1;
        let sender = sender.clone();
        let verbose = opt.verbose;
        let task = move || {
            let app = AXUIElement::application(pid);
            let windows = get_windows_for_app(app, verbose);
            sender.send((pid, bundle_id, windows)).unwrap()
        };
        if serial {
            task();
        } else {
            tokio::task::spawn_blocking(task);
        }
    }
    drop(sender);
    if targeted_list && matched_apps == 0 {
        println!("{}", ax_list_no_match(opt.bundle.as_deref().unwrap()));
    }
    while let Some((pid, info, windows)) = receiver.recv().await {
        //println!("{info:?}");
        if targeted_list {
            let result = windows.as_ref().map(Vec::len);
            if let Some(diagnostic) = ax_list_diagnostic(pid, &info, result) {
                println!("{diagnostic}");
            }
        }
        match windows {
            Ok(windows) => {
                if print {
                    for (win, dbg) in windows {
                        println!("{win:?} from {}", info.bundle_id.as_deref().unwrap_or("?"));
                        if opt.verbose {
                            println!("=> {dbg}");
                        }
                    }
                }
            }
            Err(_) => (), //println!("  * Error reading windows: {err:?}"),
        }
    }
}

fn targeted_ax_list(opt: &Opt) -> bool {
    opt.bundle.is_some() && matches!(opt.command, Command::List(List::Ax))
}

fn ax_list_no_match(bundle_filter: &str) -> String {
    format!("AX list: no running app matches bundle filter {bundle_filter:?}")
}

fn ax_list_diagnostic(
    pid: pid_t,
    info: &AppInfo,
    result: Result<usize, &accessibility::Error>,
) -> Option<String> {
    let bundle = info.bundle_id.as_deref().unwrap_or("?");
    match result {
        Ok(0) => Some(format!("AX list: pid={pid} bundle={bundle}: 0 windows")),
        Err(err) => Some(format!(
            "AX list: pid={pid} bundle={bundle}: error reading windows: {err:?}"
        )),
        Ok(_) => None,
    }
}

fn get_windows_for_app(
    app: CFRetained<AXUIElement>,
    verbose: bool,
) -> Result<Vec<(WindowInfo, String)>, accessibility::Error> {
    let windows = app.windows()?;
    windows
        .iter()
        .map(|win| {
            Ok((
                WindowInfo::try_from(&*win)?,
                verbose.then(|| format!("{:#?}", &*win)).unwrap_or_default(),
            ))
        })
        .collect()
}

fn get_apps(opt: &Opt) {
    for (pid, _bundle_id) in sys::app::running_apps(opt.bundle.clone()) {
        let app = AXUIElement::application(pid);
        println!("{:#?}", &*app);
    }
}

async fn time<O, F: Future<Output = O>>(desc: &str, f: impl FnOnce() -> F) -> O {
    let start = Instant::now();
    let out = f().await;
    let end = Instant::now();
    println!("{desc} took {:?}", end - start);
    out
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use sugarglider::sys::app::AppInfo;

    use super::{Opt, ax_list_diagnostic, ax_list_no_match, targeted_ax_list};

    #[test]
    fn targeted_ax_list_distinguishes_no_app_error_and_empty_windows() {
        let opt = Opt::try_parse_from(["devtool", "--bundle", "com.apple.TextEdit", "list", "ax"])
            .unwrap();
        assert!(targeted_ax_list(&opt));
        assert!(!targeted_ax_list(
            &Opt::try_parse_from(["devtool", "list", "ax"]).unwrap()
        ));
        assert!(!targeted_ax_list(
            &Opt::try_parse_from(["devtool", "--bundle", "com.apple.TextEdit", "list", "all"])
                .unwrap()
        ));
        assert_eq!(
            ax_list_no_match("com.apple.TextEdit"),
            "AX list: no running app matches bundle filter \"com.apple.TextEdit\""
        );

        let info = AppInfo {
            bundle_id: Some("com.apple.TextEdit".to_owned()),
            localized_name: Some("TextEdit".to_owned()),
        };
        assert_eq!(
            ax_list_diagnostic(33920, &info, Err(&accessibility::Error::NotFound)),
            Some(
                "AX list: pid=33920 bundle=com.apple.TextEdit: error reading windows: NotFound"
                    .to_owned()
            )
        );
        assert_eq!(
            ax_list_diagnostic(33920, &info, Ok(0)),
            Some("AX list: pid=33920 bundle=com.apple.TextEdit: 0 windows".to_owned())
        );
        assert_eq!(ax_list_diagnostic(33920, &info, Ok(1)), None);
    }
}
