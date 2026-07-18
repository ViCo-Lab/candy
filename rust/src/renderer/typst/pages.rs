//! Per-page sequential playback for *cross-page scenes*.
//!
//! When a scene's natural Typst layout overflows a single page, the mobjects
//! stay in **one** scene (shared ownership, shared timeline) but are laid out
//! across the overflow pages. The renderer then plays those pages **in
//! sequence** on a single-page canvas (it does *not* grow the canvas): each
//! page has its own independent timeline, and the other pages' timelines stay
//! frozen until the current page finishes and the renderer auto-advances to the
//! next page.
//!
//! This module owns the *page schedule*: the mapping from global time `t` to
//! "which page is currently playing" for each scene. The [`Renderer`] in
//! `mod.rs` consults [`PageScheduler::active_page_of`] while drawing and skips
//! every mobject that does not belong to the active page.

use std::collections::{HashMap, HashSet};

use crate::core::ast::{Action, Label, Scene};

/// One contiguous slice of a scene's timeline that plays on a single page.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageSegment {
    /// The page (0-based) shown during this slice.
    pub page: usize,
    /// Global start time (ms) of this slice (inclusive).
    pub start_ms: u32,
    /// Global end time (ms) of this slice (exclusive).
    pub end_ms: u32,
}

/// Default dwell (ms) given to a page that received no animation of its own,
/// so the renderer still advances to it in order after the animated pages.
const DEFAULT_PAGE_MS: u32 = 800;

/// Builds and answers queries about per-scene page playback schedules.
///
/// Constructed once in [`Renderer::ensure_natural`](super::Renderer::ensure_natural)
/// from the natural layout (`page_of`) and the scene timeline (`slides`).
pub(crate) struct PageScheduler {
    /// label -> the page (0-based) its natural layout landed on.
    page_of: HashMap<Label, usize>,
    /// Per-scene page playback schedule: the ordered list of page-segments that
    /// make up the scene's timeline.
    page_schedules: HashMap<usize, Vec<PageSegment>>,
}

impl PageScheduler {
    /// An empty scheduler (used as the `Renderer` placeholder before
    /// [`Renderer::ensure_natural`] rebuilds it from the real layout).
    pub(crate) fn empty() -> Self {
        Self {
            page_of: HashMap::new(),
            page_schedules: HashMap::new(),
        }
    }

    /// Build the scheduler from a natural-layout map (`page_of`) and the number
    /// of pages each scene spilled onto.
    pub(crate) fn build(
        scene: &Scene,
        page_of: HashMap<Label, usize>,
        page_counts: &HashMap<usize, usize>,
    ) -> Self {
        let mut page_schedules: HashMap<usize, Vec<PageSegment>> = HashMap::new();
        let scene_ids: Vec<usize> = if scene.scenes.is_empty() {
            vec![0]
        } else {
            scene.scenes.iter().map(|s| s.id).collect()
        };
        for sid in scene_ids {
            let segs = Self::scene_segments(scene, &page_of, sid, page_counts);
            page_schedules.insert(sid, segs);
        }
        Self {
            page_of,
            page_schedules,
        }
    }

    /// The page (0-based) a label's natural layout landed on, if known.
    pub(crate) fn page_of(&self, label: &Label) -> Option<usize> {
        self.page_of.get(label).copied()
    }

    /// The page (0-based) that is actively playing at global time `t` within
    /// scene `sid`. Mobjects on other pages are frozen (not drawn) until this
    /// page finishes and the renderer auto-advances.
    pub(crate) fn active_page_of(&self, sid: usize, t: u32) -> usize {
        match self.page_schedules.get(&sid) {
            Some(segs) if !segs.is_empty() => {
                for seg in segs {
                    if t >= seg.start_ms && t < seg.end_ms {
                        return seg.page;
                    }
                }
                segs.last().unwrap().page
            }
            _ => 0,
        }
    }

    /// Partition one scene's timeline into ordered page-segments.
    fn scene_segments(
        scene: &Scene,
        page_of: &HashMap<Label, usize>,
        sid: usize,
        page_counts: &HashMap<usize, usize>,
    ) -> Vec<PageSegment> {
        // In a multi-scene document a slide belongs to this scene only if the
        // scene is active at the slide's midpoint.
        let in_scene = |mid: u32| -> bool {
            if scene.scenes.is_empty() {
                true
            } else {
                scene.active_scene_at(mid) == sid
            }
        };
        let mut ptr: u32 = 0;
        let mut segs: Vec<PageSegment> = Vec::new();
        let mut cur_page: Option<usize> = None;
        let mut seg_start: Option<u32> = None;
        let mut seg_page: usize = 0;
        for slide in &scene.slides {
            let start = ptr;
            let end = ptr + slide.duration_ms;
            ptr = end;
            if !in_scene(start + slide.duration_ms / 2) {
                continue;
            }
            // Page of this slide = the page of its first targeted mobject, or
            // the inherited current page for untargeted slides (pauses / camera
            // moves), defaulting to page 0.
            let page = slide
                .actions
                .iter()
                .filter_map(|a: &Action| a.target().and_then(|l| page_of.get(l).copied()))
                .next()
                .or(cur_page)
                .unwrap_or(0);
            match seg_start {
                None => {
                    seg_start = Some(start);
                    seg_page = page;
                }
                Some(s) => {
                    if page != seg_page {
                        segs.push(PageSegment {
                            page: seg_page,
                            start_ms: s,
                            end_ms: start,
                        });
                        seg_start = Some(start);
                        seg_page = page;
                    }
                }
            }
            cur_page = Some(page);
        }
        if let Some(s) = seg_start {
            segs.push(PageSegment {
                page: seg_page,
                start_ms: s,
                end_ms: ptr,
            });
        }
        // Pages that received no animation still need a window so the renderer
        // advances to them in order (after the animated pages). Give each a
        // short default dwell.
        let max_page = page_counts.get(&sid).copied().unwrap_or(1).max(1);
        let present: HashSet<usize> = segs.iter().map(|s| s.page).collect();
        let mut missing: Vec<usize> = (0..max_page).filter(|p| !present.contains(p)).collect();
        missing.sort_unstable();
        let mut t = ptr;
        for p in missing {
            segs.push(PageSegment {
                page: p,
                start_ms: t,
                end_ms: t + DEFAULT_PAGE_MS,
            });
            t += DEFAULT_PAGE_MS;
        }
        segs
    }
}
