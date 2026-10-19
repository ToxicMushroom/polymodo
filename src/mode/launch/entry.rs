use super::*;
use crate::app::AppSender;
use indexmap::IndexMap;
use log::debug;
use once_map::OnceMap;
use slint::SharedString;
use std::cell::LazyCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

type IconPath = String;

static DESKTOP_ENTRIES: Mutex<LazyCell<IndexMap<PathBuf, Arc<DesktopEntry>>>> =
    Mutex::new(LazyCell::new(IndexMap::new));

/// Size of icons shown in the launcher, in pixels
const ICON_SIZE: u32 = 32;
const ICON_SCALE: u32 = 1;

/// An immutable, in-memory index of every icon in the `hicolor` theme (plus standalone icons).
///
/// Icons are only ever looked up in hicolor (which inherits from no other theme), so indexing
/// all of its files up-front makes every lookup a single hash map access, without touching the
/// filesystem, allocating, or locking.
struct IconIndex {
    /// Icon name → every file for that icon, with the size info of the directory it lives in.
    by_name: HashMap<Box<str>, Vec<(DirectorySize, icon::IconFile)>>,
    standalone: HashMap<String, icon::IconFile>,
}

/// The sizing information of an icon theme directory.
#[derive(Copy, Clone)]
struct DirectorySize {
    directory_type: icon::DirectoryType,
    size: u32,
    scale: u32,
    min_size: u32,
    max_size: u32,
    threshold: u32,
}

impl From<&icon::DirectoryIndex> for DirectorySize {
    fn from(dir: &icon::DirectoryIndex) -> Self {
        Self {
            directory_type: dir.directory_type,
            size: dir.size,
            scale: dir.scale,
            min_size: dir.min_size,
            max_size: dir.max_size,
            threshold: dir.threshold,
        }
    }
}

impl DirectorySize {
    /// How far off icons in this directory are from the desired size; 0 if they match.
    /// Mirrors the XDG icon theme specification's `DirectorySizeDistance`.
    fn distance(&self, size: u32, scale: u32) -> u32 {
        use icon::DirectoryType::*;

        let Self {
            directory_type,
            size: dir_size,
            scale: dir_scale,
            min_size,
            max_size,
            threshold,
        } = *self;

        match directory_type {
            _ if dir_scale != scale => u32::MAX,
            Fixed => dir_size.abs_diff(size),
            Scalable if size < min_size => min_size - size,
            Scalable => size.saturating_sub(max_size),
            Threshold if size.abs_diff(dir_size) <= threshold => 0,
            Threshold if size < dir_size => min_size.abs_diff(size),
            Threshold => max_size.abs_diff(size),
        }
    }
}

impl IconIndex {
    fn build() -> Self {
        let _span = crate::timing::span("icon index build");

        let mut icons = icon::Icons::new();
        // don't bother with themes other than hicolor;
        // indexing every installed theme is an order of magnitude more work.
        icons.themes.retain(|name, _| name == "hicolor");

        let mut by_name: HashMap<Box<str>, Vec<_>> = HashMap::new();
        for (_, dir, file) in icons.find_all_icons() {
            by_name
                .entry(file.icon_name().into())
                .or_default()
                .push((DirectorySize::from(dir), file));
        }

        Self {
            by_name,
            standalone: icons.standalone_icons,
        }
    }

    /// Find the best matching file for an icon: one of exactly the right size if possible,
    /// otherwise the closest in size. Falls back to standalone icons.
    fn find(&self, name: &str, size: u32, scale: u32) -> Option<&Path> {
        let themed = self.by_name.get(name).and_then(|files| {
            files
                .iter()
                .find(|(dir, _)| dir.distance(size, scale) == 0)
                .or_else(|| files.iter().min_by_key(|(dir, _)| dir.distance(size, scale)))
        });

        themed
            .map(|(_, file)| file)
            .or_else(|| self.standalone.get(name))
            .map(|file| file.path())
    }
}

static ICON_INDEX: LazyLock<IconIndex> = LazyLock::new(IconIndex::build);

/// Start building the icon index in the background, so that it's ready when icons are needed.
pub fn warm_icon_index() {
    std::thread::spawn(|| LazyLock::force(&ICON_INDEX));
}

/// Time spent looking up icons in the icon theme
pub(super) static ICON_LOOKUP_TIME: crate::timing::Accumulator = crate::timing::Accumulator::new();
/// Time spent reading icon files
pub(super) static ICON_READ_TIME: crate::timing::Accumulator = crate::timing::Accumulator::new();
/// Time spent decoding (and rasterizing) icons
pub(super) static ICON_DECODE_TIME: crate::timing::Accumulator = crate::timing::Accumulator::new();

/// Icons are rasterized at this size (in pixels). This is larger than [ICON_SIZE] so that
/// vector icons stay sharp on scaled displays.
const ICON_RASTER_SIZE: u32 = ICON_SIZE * 2;

// contains a None entry if we tried loading the icon, but failed
static ICONS_RENDERED: LazyLock<OnceMap<IconPath, Box<RenderedIcon>>> = LazyLock::new(OnceMap::new);

// This is just Option, but with variants named for their meaning.
enum RenderedIcon {
    Ok(IconPixels),
    Failed,
}

/// A decoded icon. Cheap to clone, and may be sent across threads.
#[derive(Debug, Clone)]
pub struct IconPixels {
    pixels: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    premultiplied: bool,
}

impl IconPixels {
    pub fn to_slint_image(&self) -> slint::Image {
        // this does not copy the pixels
        if self.premultiplied {
            slint::Image::from_rgba8_premultiplied(self.pixels.clone())
        } else {
            slint::Image::from_rgba8(self.pixels.clone())
        }
    }

    /// Decode an icon file.
    ///
    /// This deliberately doesn't use slint's own image loading, as that (for SVGs) loads every
    /// system font on the calling thread, which is the UI thread.
    fn decode(path: &Path, data: &[u8]) -> Option<Self> {
        let is_svg = path
            .extension()
            .is_some_and(|ext| ext == "svg" || ext == "svgz");

        if is_svg {
            Self::rasterize_svg(data)
        } else {
            let image = image::load_from_memory(data).ok()?.into_rgba8();
            let pixels = slint::SharedPixelBuffer::clone_from_slice(
                image.as_raw(),
                image.width(),
                image.height(),
            );

            Some(Self {
                pixels,
                premultiplied: false,
            })
        }
    }

    fn rasterize_svg(data: &[u8]) -> Option<Self> {
        use resvg::{tiny_skia, usvg};

        // the default options come with an empty font database:
        // icons don't contain text, so there's no need to load system fonts.
        let tree = usvg::Tree::from_data(data, &usvg::Options::default()).ok()?;

        let size = tree.size();
        let scale = ICON_RASTER_SIZE as f32 / size.width().max(size.height());
        let width = (size.width() * scale).ceil() as u32;
        let height = (size.height() * scale).ceil() as u32;

        let mut pixmap = tiny_skia::Pixmap::new(width, height)?;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );

        let pixels = slint::SharedPixelBuffer::clone_from_slice(pixmap.data(), width, height);

        Some(Self {
            pixels,
            premultiplied: true,
        })
    }
}

#[derive(Debug, Clone)]
pub struct DesktopEntry {
    pub name: SharedString,
    pub generic_name: Option<SharedString>,
    pub description: Option<SharedString>,
    pub path: PathBuf,
    /// The path relative to the `applications` directory this entry was found in.
    pub relative_path: PathBuf,
    pub source_hash: u64,
    pub exec: String,
    pub icon: Option<String>,
    pub actions: Vec<DesktopEntryAction>,
}

#[derive(Debug, Clone)]
pub struct DesktopEntryAction {
    pub name: SharedString,
    pub exec: String,
}

impl DesktopEntry {
    /// The path at which a user-specific override of this entry would be located.
    pub fn override_path(&self) -> Option<PathBuf> {
        crate::xdg::user_applications_dir().map(|dir| dir.join(&self.relative_path))
    }

    /// Whether a user-specific override of this entry exists.
    pub fn has_override(&self) -> bool {
        self.override_path().is_some_and(|path| path.is_file())
    }
}

fn next_id() -> EntryId {
    static IDX: AtomicUsize = AtomicUsize::new(0);
    let idx = IDX.fetch_add(1, Ordering::Relaxed);
    EntryId(idx)
}

pub fn scour_desktop_entries(sender: AppSender<Message>) {
    // immediately push cached entries
    {
        let rows = DESKTOP_ENTRIES.lock().unwrap();
        debug!("# Cached desktop entries: {}", rows.len());
        if !rows.is_empty() {
            let cached = rows.values().map(|row| (next_id(), row.clone())).collect();
            sender.send(Message::NewEntries(cached));
        }
    }

    // then start a search for new ones
    let start = Instant::now();
    let entries: Vec<_> = crate::xdg::find_desktop_entries();

    // and add any new ones to the searcher
    {
        let mut cache = DESKTOP_ENTRIES.lock().unwrap();
        let mut new_entries = vec![];
        let mut new_cache = vec![];

        for entry in entries {
            // remove entries that aren't fit for being shown in the launcher
            let Some(exec) = entry.exec else {
                continue;
            };

            // an entry with `NoDisplay=true` does not qualify to be shown in the launcher
            if entry.no_display == Some(true) {
                continue;
            }

            // Does this entry exist in the cache already, unchanged?
            let entry = match cache.get(&entry.source_path) {
                Some(de) if de.source_hash == entry.source_hash => de.clone(),
                // if not, make one!
                _ => {
                    log::trace!("new entry {}", entry.source_path.to_string_lossy());

                    // add a new search entry for this desktop entry.
                    let desktop_entry = Arc::new(DesktopEntry {
                        name: entry.name.into(),
                        generic_name: entry.generic_name.clone().map(Into::into),
                        description: entry.comment.clone().map(Into::into),
                        path: entry.source_path,
                        relative_path: entry.relative_path,
                        source_hash: entry.source_hash,
                        exec,
                        icon: entry.icon,
                        actions: entry
                            .actions
                            .into_iter()
                            .filter_map(|action| {
                                Some(DesktopEntryAction {
                                    name: action.name.into(),
                                    exec: action.exec?,
                                })
                            })
                            .collect(),
                    });

                    // and also add it to the fuzzy searcher
                    new_entries.push((next_id(), desktop_entry.clone()));

                    desktop_entry
                }
            };

            new_cache.push(entry);
        }

        **cache = new_cache
            .into_iter()
            .map(|d| (d.path.clone(), d)) // map each desktop entry to its path
            .collect();

        if !new_entries.is_empty() {
            let time_it_took = Instant::now() - start;
            log::debug!(
                "Took {time_it_took:?} to find {} new entries",
                new_entries.len()
            );

            // send them all at once, so the UI only has to process (and re-sort, re-filter) once
            sender.send(Message::NewEntries(new_entries));
        }
    }

    sender.send(Message::EntriesScanned);
}

pub fn is_icon_cached(icon: &str) -> bool {
    ICONS_RENDERED.get(icon).is_some()
}

/// Try loading an icon, given its name or path. The lookup, I/O and decoding happen on a blocking
/// thread pool.
/// This function utilizes a cache to avoid loading the same icon twice; the cache persists forever.
pub async fn load_icon(icon: &str) -> Option<IconPixels> {
    if let Some(cached) = ICONS_RENDERED.get(icon) {
        return match cached {
            RenderedIcon::Ok(pixels) => Some(pixels.clone()),
            RenderedIcon::Failed => None,
        };
    }

    let pixels = smol::unblock({
        let icon = icon.to_string();
        move || {
            // if `Icon` is an absolute path, the image pointed at should be loaded:
            let path = if icon.starts_with('/') {
                Path::new(&icon)
            } else {
                ICON_LOOKUP_TIME.time(|| ICON_INDEX.find(&icon, ICON_SIZE, ICON_SCALE))?
            };

            let data = ICON_READ_TIME.time(|| std::fs::read(path)).ok()?;
            ICON_DECODE_TIME.time(|| IconPixels::decode(path, &data))
        }
    })
    .await;

    // cache the result, so that any successive fetches for this icon immediately return
    ICONS_RENDERED.insert(icon.to_string(), |_| {
        Box::new(match &pixels {
            Some(pixels) => RenderedIcon::Ok(pixels.clone()),
            None => RenderedIcon::Failed,
        })
    });

    pixels
}
