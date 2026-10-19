use super::entry::*;
use super::history::LaunchHistory;
use super::settings::*;
use crate::app::{App, AppExt, AppName, AppSender, JsonAppResult};
use crate::fuzzy_search::FuzzySearch;
use crate::mode::{HideOnDrop, HideOnDropExt};
use crate::ui;
use crate::ui::index_model::IndexModel;
use anyhow::anyhow;
use slint::{ComponentHandle, Model, ModelExt, ModelRc, SharedString, VecModel};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::prelude::CommandExt;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;

pub(super) type LauncherEntriesModel = Rc<IndexModel<EntryId, LauncherEntry>>;

#[derive(Debug, Clone)]
pub enum Message {
    QuerySet(String),
    Launch(EntryId),
    /// Launch one of the additional desktop actions of an entry, by index.
    LaunchAction(EntryId, usize),
    /// Open the user-specific override of an entry in a text editor, creating it if necessary.
    EditEntry(EntryId),
    NewEntries(Vec<(EntryId, Arc<DesktopEntry>)>),
    /// An icon load for an entry finished; `None` if the icon couldn't be loaded.
    IconLoaded(EntryId, Option<IconPixels>),
    /// All desktop entries have been found and sent as [Message::NewEntries].
    EntriesScanned,
    /// The window has rendered a frame.
    FrameRendered,
    TransparencySet(f32),
    SearchUpdated,
}

// Time spent in hot paths during startup, reported once the launcher has fully loaded.
static NEW_ENTRY_TIME: crate::timing::Accumulator = crate::timing::Accumulator::new();
static SEARCH_UPDATE_TIME: crate::timing::Accumulator = crate::timing::Accumulator::new();
static TO_SLINT_TIME: crate::timing::Accumulator = crate::timing::Accumulator::new();

/// Tracks how far the launcher is with loading, for timing purposes.
#[derive(Default)]
struct LoadProgress {
    first_frame: Option<std::time::Duration>,
    entries_scanned: Option<std::time::Duration>,
    pending_icons: usize,
    fully_loaded: bool,
    frames: u32,
    last_frame: std::time::Duration,
    /// The longest time between two frames, while loading
    max_frame_gap: std::time::Duration,
    /// The summary of load timings, once fully loaded. Reported after the next frame is rendered.
    summary: Option<String>,
    /// In bench mode: the full load summary, and when the test query was started, and whether its
    /// results have been applied to the model.
    bench_query: Option<(String, std::time::Duration, bool)>,
}

/// In bench mode, this query is searched for after loading, to measure the latency of a search.
const BENCH_QUERY: &str = "fire";

pub struct Launcher {
    entries: LauncherEntriesModel,
    main_window: Option<HideOnDrop<ui::LauncherWindow>>,
    sender: AppSender<Message>,
    search: FuzzySearch<1, SearchEntry>,
    bias: LaunchHistory,
    settings: LauncherSettings,
    load: LoadProgress,
}

impl App for Launcher {
    type Message = Message;
    type Output = JsonAppResult<()>;

    const NAME: AppName = AppName::Launcher;

    fn create(message_sender: AppSender<Self::Message>, warmup: bool) -> Self {
        let _create_span = crate::timing::span("launcher create");

        // Start the slow, independent work first, so it runs while the window is being created.
        {
            let message_sender = message_sender.clone();
            let _ = std::thread::spawn(move || scour_desktop_entries(message_sender));
        }
        warm_icon_index();

        // read the bias and settings from persistent state, if any.
        let (bias, settings) = crate::timing::time("read launcher state", || {
            let bias = Self::read_state::<LaunchHistory>().ok().unwrap_or_default();
            let settings = Self::read_state::<LauncherSettings>()
                .unwrap_or_default()
                .sanitize();
            (bias, settings)
        });

        let main_window: Option<HideOnDrop<ui::LauncherWindow>> = if warmup {
            None
        } else {
            crate::timing::time("create launcher window", || {
                Some(ui::LauncherWindow::new().unwrap().hide_on_drop())
            })
        };

        let model: LauncherEntriesModel = Default::default();

        if let Some(main_window) = &main_window {
            // The model passed to the UI is filtered on the `shown` property on LauncherEntryUi,
            // converted to the slint struct that represents each entry.
            let model = model
                .clone()
                .filter(|entry| entry.shown)
                .sort_by(|a, b| a.bias.total_cmp(&b.bias).then(a.score.cmp(&b.score)))
                .reverse()
                .map(|entry| TO_SLINT_TIME.time(|| entry.to_slint()));

            main_window
                .global::<ui::LauncherEntries>()
                .set_entries(ModelRc::new(model));
        }

        let search: FuzzySearch<1, SearchEntry> = crate::timing::time("create fuzzy search", || {
            FuzzySearch::create_with_config({
                let mut config = nucleo::Config::DEFAULT;
                config.prefer_prefix = true;
                config
            })
        });

        {
            let notify = search.notify();
            let sender = message_sender.clone();
            message_sender.spawn(async move {
                loop {
                    notify.acquire().await;

                    sender.send(Message::SearchUpdated)
                }
            });
        }

        // On search query edit
        if let Some(main_window) = &main_window {
            let message_sender = message_sender.clone();
            main_window
                .global::<ui::LauncherSearch>()
                .on_search_edited(move |query| {
                    message_sender.send(Message::QuerySet(query.as_str().to_string()));
                });
        }

        // On escape
        if let Some(main_window) = &main_window {
            let message_sender = message_sender.clone();
            main_window.on_escape_pressed(move || {
                message_sender.finish();
            });
        }

        // On enter (launch)
        if let Some(main_window) = &main_window {
            let message_sender = message_sender.clone();
            main_window.on_launch(move |id| {
                if id < 0 {
                    return;
                }

                message_sender.send(Message::Launch(EntryId(id as usize)))
            });
        }

        // Context menu
        if let Some(main_window) = &main_window {
            let context_menu = main_window.global::<ui::LauncherContextMenu>();

            // The menu's contents are prepared synchronously, right before it's shown.
            let entries = model.clone();
            let window = main_window.as_weak();
            context_menu.on_prepare(move |id| {
                let Some(window) = window.upgrade() else {
                    return;
                };
                let Some(entry) = usize::try_from(id)
                    .ok()
                    .and_then(|id| entries.get_value_of_key(&EntryId(id)))
                else {
                    return;
                };

                let desktop = &entry.desktop;
                let context_menu = window.global::<ui::LauncherContextMenu>();
                context_menu.set_entry_name(desktop.name.clone());
                context_menu.set_has_override(desktop.has_override());
                let actions: Vec<SharedString> =
                    desktop.actions.iter().map(|a| a.name.clone()).collect();
                context_menu.set_actions(ModelRc::new(VecModel::from(actions)));
            });

            let sender = message_sender.clone();
            context_menu.on_edit(move |id| {
                if let Ok(id) = usize::try_from(id) {
                    sender.send(Message::EditEntry(EntryId(id)));
                }
            });

            let sender = message_sender.clone();
            context_menu.on_launch_action(move |id, action| {
                if let (Ok(id), Ok(action)) = (usize::try_from(id), usize::try_from(action)) {
                    sender.send(Message::LaunchAction(EntryId(id), action));
                }
            });
        }

        if let Some(main_window) = &main_window {
            let message_sender = message_sender.clone();
            main_window.on_transparency_changed(move |transparency| {
                message_sender.send(Message::TransparencySet(transparency));
            });
        }

        if let Some(main_window) = &main_window {
            // report rendered frames, for timing purposes
            let sender = message_sender.clone();
            let _ = main_window
                .window()
                .set_rendering_notifier(move |state, _| match state {
                    slint::RenderingState::RenderingSetup => {
                        crate::timing::milestone("rendering set up")
                    }
                    slint::RenderingState::BeforeRendering => {
                        crate::timing::milestone("before rendering")
                    }
                    slint::RenderingState::AfterRendering => {
                        crate::timing::milestone("after rendering");
                        sender.send(Message::FrameRendered);
                    }
                    _ => {}
                });

            crate::timing::time("show launcher window", || main_window.show().unwrap());
        }

        let mut launcher = Launcher {
            entries: model,
            bias,
            search,
            main_window,
            sender: message_sender,
            settings,
            load: Default::default(),
        };

        launcher.apply_settings();

        launcher
    }

    fn on_message(&mut self, message: Self::Message) {
        match message {
            Message::QuerySet(query) => {
                self.search.search::<0>(query);
            }
            Message::Launch(entry_id) => {
                if let Some(LauncherEntry { desktop, .. }) =
                    self.entries.get_value_of_key(&entry_id)
                {
                    self.launch_with_bias(&desktop, &desktop.exec);
                }
            }
            Message::LaunchAction(entry_id, action) => {
                if let Some(LauncherEntry { desktop, .. }) =
                    self.entries.get_value_of_key(&entry_id)
                    && let Some(action) = desktop.actions.get(action)
                {
                    self.launch_with_bias(&desktop, &action.exec);
                }
            }
            Message::EditEntry(entry_id) => {
                if let Some(LauncherEntry { desktop, .. }) =
                    self.entries.get_value_of_key(&entry_id)
                {
                    if let Err(e) = edit_override(&desktop) {
                        log::error!("couldn't edit desktop entry override: {e}");
                    }
                    self.sender.finish();
                }
            }
            Message::NewEntries(entries) => NEW_ENTRY_TIME.time(|| {
                for (id, entry) in entries {
                    self.search.push(SearchEntry {
                        for_id: id,
                        text: entry.name.clone(),
                    });
                    let entry = self.launcher_entry_for_desktop(id, entry);
                    self.entries.insert(id, entry);
                }
            }),
            Message::IconLoaded(id, icon) => {
                self.load.pending_icons -= 1;
                if let Some(icon) = icon {
                    self.entries.mutate_by_key(&id, |_, _, v| {
                        v.icon = Some(Rc::new(LazyImage::new(icon)));
                    });
                }
                self.check_fully_loaded();
            }
            Message::EntriesScanned => {
                crate::timing::milestone("entries scanned");
                self.load.entries_scanned = Some(crate::timing::since_start());
                self.check_fully_loaded();
            }
            Message::FrameRendered => {
                let now = crate::timing::since_start();
                let load = &mut self.load;
                if load.first_frame.is_none() {
                    crate::timing::milestone("first frame rendered");
                    load.first_frame = Some(now);
                } else if !load.fully_loaded {
                    load.max_frame_gap = load.max_frame_gap.max(now - load.last_frame);
                }
                load.frames += 1;
                load.last_frame = now;

                if let Some((summary, started, true)) = &load.bench_query {
                    // the test query's results have been rendered: report everything.
                    println!(
                        "POLYMODO_BENCH {summary} query_frame={:.2}",
                        ms(now - *started)
                    );
                    self.sender.finish();
                } else if let Some(summary) = load.summary.take() {
                    // this is the first frame after loading fully
                    crate::timing::milestone("final frame rendered");
                    let summary = format!(
                        "{summary} to_slint={:.2}/{} final_frame={:.2} frames={} max_frame_gap={:.2}",
                        ms(TO_SLINT_TIME.total()),
                        TO_SLINT_TIME.count(),
                        ms(now),
                        load.frames,
                        ms(load.max_frame_gap)
                    );
                    tracing::debug!(target: "polymodo::timing", "{summary}");

                    if crate::timing::bench_mode() {
                        // continue by measuring the latency of a search query
                        load.bench_query = Some((summary, now, false));
                        self.search.search::<0>(BENCH_QUERY);
                    }
                } else {
                    self.check_fully_loaded();
                }
            }
            Message::SearchUpdated => SEARCH_UPDATE_TIME.time(|| {
                self.search.tick();
                if !self.search.take_changed() {
                    // nucleo notifies for every item pushed; most of those don't change matches.
                    return;
                }

                // map each matched entry to its score: better matches come first
                let matches = self.search.get_matches();
                let match_count = matches.len();
                let scores: HashMap<EntryId, u32> = matches
                    .into_iter()
                    .enumerate()
                    .map(|(pos, entry)| (entry.for_id, (match_count - pos) as u32))
                    .collect();

                self.entries.mutate_all(|_, entry_id, v| {
                    let score = scores.get(entry_id).copied();
                    v.shown = score.is_some();
                    v.score = score.unwrap_or_default();
                });

                if let Some((_, _, applied)) = &mut self.load.bench_query {
                    *applied = true;
                }
            }),
            Message::TransparencySet(trans) => {
                self.settings.transparency = trans;
            }
        }
    }

    fn stop(self) -> Self::Output {
        // save settings, then quit
        if let Err(e) = Self::write_state(&self.settings) {
            log::error!("couldn't write settings: {e}");
        }

        JsonAppResult(())
    }
}

impl Launcher {
    /// Once everything has loaded, report the timings. In bench mode, also quit.
    fn check_fully_loaded(&mut self) {
        let LoadProgress {
            first_frame: Some(first_frame),
            entries_scanned: Some(entries_scanned),
            pending_icons: 0,
            fully_loaded: false,
            ..
        } = self.load
        else {
            return;
        };
        // in warmup mode, no frames are rendered, so we don't get here.
        let Some(window) = &self.main_window else {
            return;
        };

        self.load.fully_loaded = true;
        let fully_loaded = crate::timing::since_start();
        crate::timing::milestone("fully loaded");

        let (lookup, read, decode) = (
            &super::entry::ICON_LOOKUP_TIME,
            &super::entry::ICON_READ_TIME,
            &super::entry::ICON_DECODE_TIME,
        );
        let summary = format!(
            "first_frame={:.2} entries_scanned={:.2} fully_loaded={:.2} entries={} \
             new_entries={:.2}/{} search_update={:.2}/{} \
             icon_lookup={:.2}/{} icon_read={:.2}/{} icon_decode={:.2}/{}",
            ms(first_frame),
            ms(entries_scanned),
            ms(fully_loaded),
            self.entries.row_count(),
            ms(NEW_ENTRY_TIME.total()),
            NEW_ENTRY_TIME.count(),
            ms(SEARCH_UPDATE_TIME.total()),
            SEARCH_UPDATE_TIME.count(),
            ms(lookup.total()),
            lookup.count(),
            ms(read.total()),
            read.count(),
            ms(decode.total()),
            decode.count(),
        );
        // the rest of the summary is completed after the next frame
        self.load.summary = Some(summary);
        window.window().request_redraw();
    }

    /// Launch `exec` (the entry's own, or one of its actions'), counting it towards the bias of
    /// `desktop`, then close the launcher.
    fn launch_with_bias(&mut self, desktop: &DesktopEntry, exec: &str) {
        self.bias.increment_and_decay(desktop.path.clone());
        if let Err(e) = Self::write_state(&self.bias) {
            log::error!("couldn't write launcher bias (scoring): {e}");
        }

        if let Err(e) = launch(desktop, exec, &[]) {
            log::error!("failed to launch: {e}")
        }
        self.sender.finish();
    }

    fn launcher_entry_for_desktop(&mut self, id: EntryId, entry: Arc<DesktopEntry>) -> LauncherEntry {
        // Icon loading is offloaded and cached.
        // if we've already got an icon for this entry, or it has failed before,
        // we don't try again:
        let icon = if let Some(icon_path) = entry.icon.as_deref() {
            if is_icon_cached(icon_path) {
                // great! load_icon won't block:
                smol::block_on(load_icon(icon_path)).map(|icon| Rc::new(LazyImage::new(icon)))
            } else {
                // no cache hit -> we'll have to offload this, and update it later.
                let icon_path = icon_path.to_string();
                let sender = self.sender.clone();

                self.load.pending_icons += 1;
                drop(slint::spawn_local(async move {
                    let icon = load_icon(&icon_path).await;
                    sender.send(Message::IconLoaded(id, icon));
                }));

                None
            }
        } else {
            None // no icon_path, no icon.
        };

        LauncherEntry {
            id,
            shown: true,
            score: 0,
            // the launch history doesn't change while the launcher is open, so compute this once
            bias: self.bias.score(&entry.path),
            desktop: entry,
            icon,
        }
    }

    fn apply_settings(&mut self) {
        let LauncherSettings { transparency } = self.settings;
        let window = &self.main_window;

        if let Some(window) = window {
            window.set_transparency(transparency);
        }
    }
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

#[derive(Default, Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EntryId(pub usize);

pub struct SearchEntry {
    for_id: EntryId,
    text: SharedString,
}

impl crate::fuzzy_search::Row<1> for SearchEntry {
    type Output = String;

    fn columns(&self) -> [Self::Output; 1] {
        [self.text.to_string()]
    }
}

#[derive(Debug, Clone)]
pub struct LauncherEntry {
    id: EntryId,
    /// Whether this entry should be shown in the UI
    shown: bool,
    /// The score this entry got from the fuzzy matcher
    score: u32,
    /// The score this entry got from the launch history
    bias: f32,
    /// The desktop entry this corresponds with
    desktop: Arc<DesktopEntry>,
    /// This entry's rendered icon
    icon: Option<Rc<LazyImage>>,
}

/// An icon whose [slint::Image] is created once and shared between clones, so that the renderer
/// can cache its texture.
#[derive(Debug)]
pub struct LazyImage {
    data: IconPixels,
    decoded: std::cell::OnceCell<slint::Image>,
}

impl LazyImage {
    fn new(data: IconPixels) -> Self {
        Self {
            data,
            decoded: Default::default(),
        }
    }

    fn get(&self) -> slint::Image {
        // slint::Image is reference counted, so this clone is cheap
        self.decoded.get_or_init(|| self.data.to_slint_image()).clone()
    }
}

impl LauncherEntry {
    pub fn to_slint(&self) -> ui::LauncherEntry {
        let icon = self
            .icon
            .as_ref()
            .map(|icon| icon.get())
            .unwrap_or_default();

        ui::LauncherEntry {
            name: self.desktop.name.clone(),
            generic_name: self.desktop.generic_name.clone().unwrap_or_default(),
            description: self.desktop.description.clone().unwrap_or_default(),
            icon,
            id: self.id.0 as i32,
        }
    }
}

/// Create a user-specific override of `desktop` (if it doesn't exist yet), and open it in a
/// text editor.
fn edit_override(desktop: &DesktopEntry) -> anyhow::Result<()> {
    let override_path = desktop
        .override_path()
        .ok_or_else(|| anyhow!("no XDG data home directory"))?;

    if !override_path.is_file() {
        if let Some(parent) = override_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&desktop.path, &override_path)?;
        log::info!(
            "created override of {} at {}",
            desktop.path.display(),
            override_path.display()
        );
    }

    open_in_editor(&override_path)
}

/// Open `path` in the user's default text editor, as determined by `xdg-mime`.
/// Falls back to `$VISUAL`/`$EDITOR` in `$TERMINAL`.
fn open_in_editor(path: &Path) -> anyhow::Result<()> {
    let editor = Command::new("xdg-mime")
        .args(["query", "default", "text/plain"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .and_then(|id| crate::xdg::find_desktop_entry_path(&id))
        .and_then(|editor_path| crate::xdg::load(&editor_path, "").ok());

    if let Some(editor) = editor
        && let Some(exec) = editor.exec.as_deref()
    {
        let editor = DesktopEntry {
            name: editor.name.into(),
            generic_name: None,
            description: None,
            path: editor.source_path,
            relative_path: editor.relative_path,
            source_hash: editor.source_hash,
            exec: exec.to_string(),
            icon: editor.icon,
            actions: vec![],
        };

        return launch(&editor, &editor.exec, &[path]);
    }

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .map_err(|_| anyhow!("no default text editor found, and $VISUAL/$EDITOR are not set"))?;
    let terminal =
        std::env::var("TERMINAL").map_err(|_| anyhow!("$TERMINAL is not set to run {editor} in"))?;

    let editor = DesktopEntry {
        name: editor.clone().into(),
        generic_name: None,
        description: None,
        path: Default::default(),
        relative_path: Default::default(),
        source_hash: 0,
        exec: format!("{terminal} -e {editor} %f"),
        icon: None,
        actions: vec![],
    };

    launch(&editor, &editor.exec, &[path])
}

/// Launch `exec`, an `Exec` string belonging to `desktop` (or one of its actions).
/// Field codes for files and URLs are substituted by `files`.
fn launch(desktop: &DesktopEntry, exec: &str, files: &[&Path]) -> anyhow::Result<()> {
    match fork::fork().map_err(|_| anyhow!("failed to fork process"))? {
        fork::Fork::Child => {
            // detach
            if let Err(e) = nix::unistd::daemon(false, false) {
                log::error!("daemonize failed: {}", e);
            }

            let files: Vec<&str> = files.iter().filter_map(|f| f.to_str()).collect();

            // split exec by spaces
            let mut args = exec
                .split(" ")
                .flat_map(|arg| match arg {
                    // a single file or URL
                    "%f" | "%u" => files.first().copied().into_iter().collect(),
                    // a list of files or URLs
                    "%F" | "%U" => files.clone(),
                    "%i" => match desktop.icon.as_deref() {
                        Some(icon) => vec!["--icon", icon],
                        None => vec![],
                    },
                    "%c" => vec![desktop.name.as_str()],
                    "%k" => {
                        vec![desktop.path.as_os_str().to_str().unwrap_or("")]
                    }
                    // remove empty strings as arguments; these may be left over from
                    //   trailing/subsequent whitespaces, and cause programs to misbehave.
                    "" => {
                        vec![]
                    }
                    _ => vec![arg],
                })
                .map(|arg| {
                    // field codes embedded in an argument are not supported; remove them.
                    ["%f", "%F", "%u", "%U"]
                        .iter()
                        .fold(arg.to_string(), |arg, code| arg.replace(code, ""))
                })
                .filter(|arg| !arg.is_empty())
                .collect::<Vec<_>>();
            // the first "argument" is the program to launch
            let program = args.remove(0);

            log::debug!("launching: prog='{}' args='{}'", program, args.join(" "));

            let error = Command::new(program).args(args).exec(); // this will never return if the exec succeeds

            // but if it did return, log the error and return:
            log::error!("failed to launch: {}", error);
            let _ = std::io::stdout().flush();
            std::process::exit(-1);
        }
        fork::Fork::Parent(pid) => {
            log::info!("Launching {:?} with pid {pid}", desktop.name.as_str());

            let _ = std::io::stdout().flush();
            Ok(())
        }
    }
}
