//! In-process Typst [`World`](typst::World) implementation shared by the
//! renderer.
//!
//! `WorldState` bundles the standard library, embedded + system fonts, and a
//! project-rooted file resolver (plus `@preview` package resolution). It is
//! built once per [`Renderer`](crate::renderer::typst::Renderer) and reused
//! across every frame compile. `CandyWorld` is a per-compile view that fixes a
//! specific `main` source over the shared state.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use typst::{Library, LibraryExt, World};
use typst_kit::datetime::Time;
use typst_kit::diagnostics::DiagnosticWorld;
use typst_kit::downloader::Downloader;
use typst_kit::files::{FileStore, FsRoot, SystemFiles};
use typst_kit::fonts::FontStore;
use typst_kit::packages::SystemPackages;
use typst_library::diag::FileError;
use typst_library::foundations::{Bytes, Datetime, Duration};
use typst_library::text::Font;
use typst_syntax::{FileId, Source as TypstSource, VirtualRoot};
use typst_utils::LazyHash;

use crate::renderer::typst::lru::LruCache;

/// Capacity of the parsed-source cache (`source_cache`). Bounded so an animated
/// render cannot accumulate one parsed `TypstSource` per frame (OOM). Static /
/// paused object bodies keep a stable key and stay resident; per-frame churn is
/// evicted.
const SOURCE_CACHE_CAP: usize = 1024;

#[cfg(feature = "system-downloader")]
use ureq::Agent;

/// A no-op downloader used when the `system-downloader` feature is disabled.
/// Returns NotFound for every URL, so @preview packages resolve only from
/// the local cache (pre-populated via `typst compile`).
#[cfg(not(feature = "system-downloader"))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct NoDownload;

#[cfg(not(feature = "system-downloader"))]
impl Downloader for NoDownload {
    fn stream(
        &self,
        _key: &dyn std::any::Any,
        _url: &str,
    ) -> std::io::Result<(Option<usize>, Box<dyn std::io::Read>)> {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "candy was built without the 'system-downloader' feature; \
             @preview packages must be pre-cached via 'typst compile'",
        ))
    }
}

/// @preview package downloader backed by `ureq` with the pure-Rust `rustls`
/// TLS backend (replaces typst-kit's `SystemDownloader`, which uses
/// `native-tls` + OpenSSL). This avoids linking the system OpenSSL entirely, so
/// the build stays self-contained and works for both host and cross targets
/// with no OpenSSL dev package and no perl. Root CAs come from the bundled
/// `webpki-roots`, so no system cert store is required.
#[cfg(feature = "system-downloader")]
pub(crate) struct RustlsDownloader {
    agent: Agent,
}

#[cfg(feature = "system-downloader")]
impl RustlsDownloader {
    fn new(user_agent: &str) -> Self {
        Self {
            agent: ureq::AgentBuilder::new().user_agent(user_agent).build(),
        }
    }
}

#[cfg(feature = "system-downloader")]
impl Downloader for RustlsDownloader {
    fn stream(
        &self,
        _key: &dyn std::any::Any,
        url: &str,
    ) -> std::io::Result<(Option<usize>, Box<dyn std::io::Read>)> {
        let response = self.agent.get(url).call().map_err(|err| match err {
            ureq::Error::Status(404, _) => std::io::Error::new(std::io::ErrorKind::NotFound, err),
            err => std::io::Error::other(err),
        })?;
        let content_len: Option<usize> = response
            .header("Content-Length")
            .and_then(|header| header.parse().ok());
        Ok((content_len, Box::new(response.into_reader())))
    }
}

/// Shared, reusable Typst World state (fonts + file resolver + standard
/// library). Built once per [`Renderer`](crate::renderer::typst::Renderer) and
/// reused across every frame compile, so the cost of system font scanning is
/// paid exactly once.
///
/// Mirrors the official `typst` CLI `SystemWorld`: the standard library is
/// built via [`Library::builder`], fonts are the embedded fallbacks plus all
/// system fonts, and the current time is captured once at construction so that
/// `datetime.today()` is stable across every frame of a single render (just
/// like the CLI fixes the time per compilation).
pub(crate) struct WorldState {
    library: LazyHash<Library>,
    fonts: FontStore,
    files: FileStore<SystemFiles>,
    now: Time,
    /// Guards the "time-dependent render is not reproducible" warning so it is
    /// printed at most once per renderer, not once per compiled frame.
    time_warned: AtomicBool,
    /// Parsed-source cache. Typst re-parses its input on every `compile` call,
    /// so the same source string (a static mobject body, the natural-layout
    /// probe, a repeated counter value, …) would be re-parsed N times across an
    /// animation's frames. We memoize the parsed `TypstSource` here (keyed by the
    /// exact source text) so repeated compiles skip the parse and reuse the
    /// already-built AST — this is the "render cache" the per-frame recompiler
    /// relies on. `TypstSource` is `Arc`-backed, so cloning out of the cache is
    /// cheap and shares the parsed tree.
    ///
    /// Bounded LRU: for animated content every frame's source is unique, so an
    /// unbounded `HashMap` would accumulate one parsed source per frame and OOM.
    /// The LRU evicts that churn while keeping static bodies resident — see
    /// [`LruCache`].
    source_cache: Mutex<LruCache<String, TypstSource>>,
}

impl WorldState {
    /// Emit the non-determinism warning at most once for this renderer.
    /// Returns `true` the first time it is called after a time-dependent
    /// compile, so the caller can print the warning exactly once.
    pub(crate) fn note_time_used(&self) -> bool {
        !self.time_warned.swap(true, Ordering::Relaxed)
    }
}

impl WorldState {
    /// Build a World state with:
    /// - the standard Typst library
    /// - embedded fallback fonts + all system fonts
    /// - a project root (the `.tyx` source's parent directory) so local
    ///   `#import "file.typ"` works, and `@preview` packages resolve from
    ///   the local cache (downloading on demand when the
    ///   `system-downloader` feature is enabled)
    /// - the current system time, captured once for `datetime.today()`
    pub(crate) fn new(project_root: PathBuf) -> Self {
        let library = LazyHash::new(Library::builder().build());

        let mut fonts = FontStore::new();
        fonts.extend(typst_kit::fonts::embedded());
        fonts.extend(typst_kit::fonts::system());

        // Package resolver: @preview packages from the local cache, with
        // on-demand download (pure-Rust `rustls` TLS, no OpenSSL) when the
        // `system-downloader` feature is enabled.
        #[cfg(feature = "system-downloader")]
        let packages = SystemPackages::new(RustlsDownloader::new("candy/0.1"));
        #[cfg(not(feature = "system-downloader"))]
        let packages = SystemPackages::new(NoDownload);

        let root = FsRoot::new(project_root);
        let files = FileStore::new(SystemFiles::new(root, packages));

        Self {
            library,
            fonts,
            files,
            now: Time::system(),
            time_warned: AtomicBool::new(false),
            source_cache: Mutex::new(LruCache::with_capacity(SOURCE_CACHE_CAP)),
        }
    }

    /// Parse `src` into a `TypstSource`, memoized by the exact source text.
    ///
    /// Consecutive frames that compile the same source (a static / paused
    /// mobject body, the natural-layout probe, a repeated `ecval` value, …)
    /// reuse the already-parsed AST instead of re-parsing — this is what lets
    /// the per-frame recompiler build up a render cache instead of paying the
    /// full parse cost on every frame. The `WorldState` (fonts + file resolver
    /// + standard library) is already shared across frames via `Arc`; this
    /// cache additionally shares the *parsed* source.
    pub(crate) fn detached_cached(&self, src: &str) -> TypstSource {
        if let Some(cached) = self.source_cache.lock().unwrap().get(src) {
            return cached.clone();
        }
        let parsed = TypstSource::detached(src.to_string());
        self.source_cache
            .lock()
            .unwrap()
            .insert(src.to_string(), parsed.clone());
        parsed
    }

}

/// A per-compile `World` view that borrows the shared [`WorldState`] and
/// fixes a specific `main` source.
pub(crate) struct CandyWorld<'a> {
    pub(crate) state: &'a WorldState,
    pub(crate) main: TypstSource,
    /// Set to `true` the first time [`today`](World::today) is queried during
    /// this compile. When set, the compiled body depends on the wall-clock
    /// time (`datetime.today()`), so the render is *not* reproducible — the
    /// caller emits a warning (see [`CandyWorld::used_time`]).
    time_used: AtomicBool,
}

impl<'a> CandyWorld<'a> {
    /// Construct a per-compile view over the shared state with a fixed `main`
    /// source. The time-usage flag starts cleared.
    pub(crate) fn new(state: &'a WorldState, main: TypstSource) -> Self {
        Self {
            state,
            main,
            time_used: AtomicBool::new(false),
        }
    }

    /// Whether this compile queried the current date/time (`datetime.today()`).
    /// If `true`, the produced document is time-dependent and therefore not
    /// deterministic across renders.
    pub(crate) fn used_time(&self) -> bool {
        self.time_used.load(Ordering::Relaxed)
    }
}

impl<'a> World for CandyWorld<'a> {
    fn library(&self) -> &LazyHash<Library> {
        &self.state.library
    }

    fn book(&self) -> &LazyHash<typst_library::text::FontBook> {
        self.state.fonts.book()
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> Result<TypstSource, FileError> {
        if id == self.main.id() {
            return Ok(self.main.clone());
        }
        // Delegate to the file store — this resolves local imports via FsRoot
        // and package imports via SystemPackages. The store caches, so
        // repeated imports of the same file are cheap.
        self.state.files.source(id)
    }

    fn file(&self, id: FileId) -> Result<Bytes, FileError> {
        self.state.files.file(id)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.state.fonts.font(index)
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        // Record that this compile consulted the wall clock: the resulting
        // document is time-dependent and thus not reproducible.
        self.time_used.store(true, Ordering::Relaxed);
        self.state.now.today(offset)
    }
}

impl<'a> DiagnosticWorld for CandyWorld<'a> {
    fn name(&self, id: FileId) -> String {
        let vpath = id.vpath();
        match id.root() {
            // Project-local files: display the path without the leading slash,
            // matching the official `typst` CLI's user-facing formatting.
            VirtualRoot::Project => vpath.get_without_slash().into(),
            // Package files: `@ns/name:ver/path`.
            VirtualRoot::Package(package) => {
                format!("{package}{}", vpath.get_with_slash())
            }
        }
    }
}
