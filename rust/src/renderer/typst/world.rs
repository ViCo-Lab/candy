//! In-process Typst [`World`](typst::World) implementation shared by the
//! renderer.
//!
//! `WorldState` bundles the standard library, embedded + system fonts, and a
//! project-rooted file resolver (plus `@preview` package resolution). It is
//! built once per [`Renderer`](crate::renderer::typst::Renderer) and reused
//! across every frame compile. `CandyWorld` is a per-compile view that fixes a
//! specific `main` source over the shared state.

use std::path::PathBuf;

use typst::{Library, LibraryExt, World};
use typst_kit::downloader::Downloader;
use typst_kit::files::{FileStore, FsRoot, SystemFiles};
use typst_kit::fonts::FontStore;
use typst_kit::packages::SystemPackages;
use typst_library::diag::FileError;
use typst_library::foundations::{Bytes, Datetime, Duration};
use typst_library::text::Font;
use typst_syntax::{FileId, Source as TypstSource};
use typst_utils::LazyHash;

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
pub(crate) struct WorldState {
    library: LazyHash<Library>,
    fonts: FontStore,
    files: FileStore<SystemFiles>,
}

impl WorldState {
    /// Build a World state with:
    /// - the standard Typst library
    /// - embedded fallback fonts + all system fonts
    /// - a project root (the `.tyx` source's parent directory) so local
    ///   `#import "file.typ"` works, and `@preview` packages resolve from
    ///   the local cache (downloading on demand when the
    ///   `system-downloader` feature is enabled)
    pub(crate) fn new(project_root: PathBuf) -> Self {
        let library = LazyHash::new(Library::default());

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
        }
    }
}

/// A per-compile `World` view that borrows the shared [`WorldState`] and
/// fixes a specific `main` source.
pub(crate) struct CandyWorld<'a> {
    pub(crate) state: &'a WorldState,
    pub(crate) main: TypstSource,
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

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        None
    }
}
