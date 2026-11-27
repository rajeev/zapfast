//! Pictures of a chat made to be read on a phone: one message, or every
//! message from one to the end of the chat, laid out at a phone's width and
//! cut into portrait pages that each hold whole bubbles where they can.
//!
//! The view (`ui::conversation::screenshot`) lays the rows out, and this
//! module decides where the pages break, keeps the capture's state between
//! frames, and writes the pages as PNGs off the interface thread.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::model::{AccountId, ChatId};

/// A page's width in points: a common phone's width, so a page shown full
/// width on a phone draws its text at about the size the phone's own does.
pub const PAGE_WIDTH: f32 = 390.0;
/// A page's tallest height in points, 19.5:9 like most phones held upright.
pub const PAGE_HEIGHT: f32 = 845.0;
/// The strip at the top of each page with the chat's name and the page's
/// number.
pub const HEADER_HEIGHT: f32 = 40.0;
/// Room above and below the rows inside a page.
pub const PAGE_PADDING: f32 = 10.0;
/// How much of a message cut across two pages the second one repeats, so a
/// line of text cut in half at the bottom of one page is whole on the next.
pub const OVERLAP: f32 = 28.0;
/// The most pages one capture makes. WhatsApp sends at most 30 pictures in
/// one go, and more would not be a screenshot to read on a phone anyway.
pub const MAX_PAGES: usize = 30;
/// The most messages one capture lays out.
pub const MAX_MESSAGES: usize = 400;
/// The window's strip above the page, with the progress and Cancel.
pub const STATUS_HEIGHT: f32 = 48.0;

/// Frames a capture lays its rows out before it settles on their heights:
/// pictures and emoji arrive over a few frames and change them.
const MEASURE_FRAMES: u32 = 90;
/// Frames in a row the measured heights must hold before pages are cut.
const STABLE_FRAMES: u32 = 3;
/// Frames a page is drawn before its picture is taken.
const PAGE_FRAMES: u32 = 3;
/// How long a page's picture may take before it is asked for again.
const SHOT_TIMEOUT: Duration = Duration::from_secs(4);
/// How many times a page's picture is asked for before the capture fails.
const SHOT_TRIES: u32 = 3;

/// A vertical extent in the laid-out column of rows, in points from its top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub top: f32,
    pub bottom: f32,
}

impl Span {
    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }
}

/// Cuts the column of `rows` (each a row's extent, in order and not
/// overlapping) into pages at most `height` tall.
///
/// A page ends before the first row that does not fit, so every row that fits
/// on a page is on one page whole. A row taller than a page is cut across as
/// many as it needs, each after the first repeating the last `overlap`
/// points of the one before.
pub fn paginate(rows: &[Span], height: f32, overlap: f32) -> Vec<Span> {
    let mut pages = Vec::new();
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return pages;
    };
    if height <= 0.0 {
        return pages;
    }
    // Repeating more than half a page would leave little new on each.
    let overlap = overlap.clamp(0.0, height / 2.0);
    let mut start = first.top;
    let mut index = 0;
    while let Some(row) = rows.get(index) {
        let limit = start + height;
        if row.bottom <= limit {
            index += 1;
            continue;
        }
        if row.top > start {
            // Something is already on this page: the row starts the next.
            pages.push(Span {
                top: start,
                bottom: row.top,
            });
            start = row.top;
            continue;
        }
        // The row is taller than a page: cut it.
        pages.push(Span {
            top: start,
            bottom: limit,
        });
        start = limit - overlap;
    }
    if last.bottom > start {
        pages.push(Span {
            top: start,
            bottom: last.bottom,
        });
    }
    pages
}

/// What a capture is doing.
pub enum Phase {
    /// Laying the rows out until their heights settle.
    Measuring { frames: u32, stable: u32 },
    /// Drawing page `index` and taking its picture.
    Page {
        index: usize,
        frames: u32,
        asked: Option<Instant>,
        tries: u32,
    },
    /// Writing the pages.
    Saving(mpsc::Receiver<Result<Saved, String>>),
    /// The pages are written and the share dialog shows them.
    Done {
        paths: Vec<PathBuf>,
        thumbnails: Vec<egui::TextureHandle>,
    },
}

/// The pages written, with a small picture of each for the dialog.
pub struct Saved {
    pub paths: Vec<PathBuf>,
    pub thumbnails: Vec<egui::ColorImage>,
}

/// How tall a page's small picture in the dialog is, in pixels.
pub const THUMBNAIL_HEIGHT: u32 = 240;

/// What a frame that drew the current page should do about its picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    /// Nothing yet: the page is settling, or its picture is on its way.
    Wait,
    /// Ask the window for its picture.
    Ask(Shot),
    /// The picture was asked for too many times without an answer.
    GiveUp,
}

/// The data a page's screenshot request carries, so its answer is told
/// apart from any other screenshot's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shot {
    pub generation: u64,
    pub page: usize,
}

/// A capture under way: which messages, and how far it has got.
pub struct Capture {
    pub account: AccountId,
    pub chat: ChatId,
    /// The first message and the last, inclusive.
    pub from: String,
    pub to: String,
    /// More messages were asked for than [`MAX_MESSAGES`] or than fit in
    /// [`MAX_PAGES`], and the capture stops short of the end.
    pub truncated: bool,
    pub generation: u64,
    pub phase: Phase,
    /// The page size in points, chosen when the capture starts.
    pub page: egui::Vec2,
    /// The rows' extents as last laid out.
    pub rows: Vec<Span>,
    /// Where each page starts and ends in the column.
    pub pages: Vec<Span>,
    /// Each page's picture, in order.
    pub images: Vec<egui::ColorImage>,
    /// The page shown full size over the chat picker, once the pages are
    /// saved.
    pub viewing: Option<usize>,
}

impl Capture {
    pub fn new(
        account: AccountId,
        chat: ChatId,
        from: String,
        to: String,
        truncated: bool,
    ) -> Self {
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            account,
            chat,
            from,
            to,
            truncated,
            generation: GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            phase: Phase::Measuring {
                frames: 0,
                stable: 0,
            },
            page: egui::vec2(PAGE_WIDTH, PAGE_HEIGHT),
            rows: Vec::new(),
            pages: Vec::new(),
            images: Vec::new(),
            viewing: None,
        }
    }

    /// The pages saved, once there are any.
    pub fn saved(&self) -> &[PathBuf] {
        match &self.phase {
            Phase::Done { paths, .. } => paths,
            _ => &[],
        }
    }

    /// Shows a saved page full size, or none. A page that isn't there shows
    /// nothing, so stepping past either end closes nothing and opens nothing.
    pub fn view(&mut self, index: Option<usize>) {
        if index.is_none_or(|index| index < self.saved().len()) {
            self.viewing = index;
        }
    }

    /// The tallest stretch of rows a page holds.
    pub fn content_height(&self) -> f32 {
        (self.page.y - HEADER_HEIGHT - 2.0 * PAGE_PADDING).max(1.0)
    }

    /// Records one frame's layout while measuring, and cuts the pages once
    /// it has held still for a few frames (or measuring has taken long
    /// enough). Returns true when the pages are cut.
    pub fn measured(&mut self, rows: Vec<Span>) -> bool {
        let Phase::Measuring { frames, stable } = &mut self.phase else {
            return false;
        };
        *frames += 1;
        if same_layout(&rows, &self.rows) {
            *stable += 1;
        } else {
            *stable = 0;
        }
        self.rows = rows;
        if *stable < STABLE_FRAMES && *frames < MEASURE_FRAMES {
            return false;
        }
        let mut pages = paginate(&self.rows, self.content_height(), OVERLAP);
        if pages.len() > MAX_PAGES {
            pages.truncate(MAX_PAGES);
            self.truncated = true;
        }
        self.pages = pages;
        self.phase = Phase::Page {
            index: 0,
            frames: 0,
            asked: None,
            tries: 0,
        };
        true
    }

    /// Counts a frame that drew the current page, and says whether to ask
    /// for its picture this frame.
    pub fn page_drawn(&mut self, now: Instant) -> Due {
        let generation = self.generation;
        let Phase::Page {
            index,
            frames,
            asked,
            tries,
        } = &mut self.phase
        else {
            return Due::Wait;
        };
        *frames += 1;
        if *frames < PAGE_FRAMES {
            return Due::Wait;
        }
        if asked.is_some_and(|at| now.duration_since(at) < SHOT_TIMEOUT) {
            return Due::Wait;
        }
        if *tries >= SHOT_TRIES {
            return Due::GiveUp;
        }
        *tries += 1;
        *asked = Some(now);
        Due::Ask(Shot {
            generation,
            page: *index,
        })
    }

    /// Takes the picture of a page, if it is the one being drawn. Returns
    /// true when it was the last page.
    pub fn shot(&mut self, shot: Shot, image: egui::ColorImage) -> bool {
        let Phase::Page { index, .. } = &mut self.phase else {
            return false;
        };
        if shot.generation != self.generation || shot.page != *index {
            return false;
        }
        self.images.push(image);
        *index += 1;
        if *index >= self.pages.len() {
            return true;
        }
        self.phase = Phase::Page {
            index: *index,
            frames: 0,
            asked: None,
            tries: 0,
        };
        false
    }

    /// The page being drawn, with its number.
    pub fn current_page(&self) -> Option<(usize, Span)> {
        match &self.phase {
            Phase::Page { index, .. } => self.pages.get(*index).map(|page| (*index, *page)),
            _ => None,
        }
    }

    /// Starts writing the pages taken into `dir`, on a thread of its own.
    pub fn save(&mut self, dir: PathBuf) {
        let images = std::mem::take(&mut self.images);
        let (done, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("screenshot-pages".into())
            .spawn(move || {
                let saved = write_pages(&dir, &images).map(|paths| Saved {
                    paths,
                    thumbnails: images.iter().map(thumbnail).collect(),
                });
                let _ = done.send(saved.map_err(|error| format!("{error:#}")));
            });
        if let Err(error) = spawned {
            let (done, failed) = mpsc::channel();
            let _ = done.send(Err(error.to_string()));
            self.phase = Phase::Saving(failed);
            return;
        }
        self.phase = Phase::Saving(receiver);
    }
}

/// Whether two layouts put every row in the same place, to within a hair.
fn same_layout(a: &[Span], b: &[Span]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| (a.top - b.top).abs() < 0.5 && (a.bottom - b.bottom).abs() < 0.5)
}

/// The folder an account keeps its last screenshots in.
pub fn folder(cache: &Path) -> PathBuf {
    cache.join("screenshots")
}

/// Replaces whatever pages `dir` held with `images`, as opaque PNGs.
pub(crate) fn write_pages(dir: &Path, images: &[egui::ColorImage]) -> anyhow::Result<Vec<PathBuf>> {
    use anyhow::Context;
    // Only the latest pages are kept: they are in the share dialog, or sent.
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("clearing {}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut paths = Vec::with_capacity(images.len());
    for (index, image) in images.iter().enumerate() {
        let [width, height] = image.size;
        // The window's frame may be translucent (vibrancy, Mica); a page is
        // painted opaque, and drops the alpha channel to stay so.
        let pixels: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|pixel| {
                let [r, g, b, _] = pixel.to_srgba_unmultiplied();
                [r, g, b]
            })
            .collect();
        let buffer = image::RgbImage::from_raw(width as u32, height as u32, pixels)
            .context("a page's pixels did not match its size")?;
        let path = dir.join(format!("screenshot-{:02}.png", index + 1));
        buffer
            .save_with_format(&path, image::ImageFormat::Png)
            .with_context(|| format!("writing {}", path.display()))?;
        paths.push(path);
    }
    log::info!("wrote {} screenshot pages", paths.len());
    Ok(paths)
}

/// A page scaled down to [`THUMBNAIL_HEIGHT`].
fn thumbnail(image: &egui::ColorImage) -> egui::ColorImage {
    let [width, height] = image.size;
    let pixels: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_srgba_unmultiplied())
        .collect();
    let Some(buffer) = image::RgbaImage::from_raw(width as u32, height as u32, pixels) else {
        return egui::ColorImage::new([1, 1], vec![egui::Color32::BLACK]);
    };
    let scaled_height = THUMBNAIL_HEIGHT.min(height as u32).max(1);
    let scaled_width =
        ((width as u64 * u64::from(scaled_height)) / (height.max(1) as u64)).max(1) as u32;
    let small = image::imageops::thumbnail(&buffer, scaled_width, scaled_height);
    egui::ColorImage::from_rgba_unmultiplied(
        [small.width() as usize, small.height() as usize],
        small.as_raw(),
    )
}

/// The part of a window's screenshot covering `rect` (in points), clamped
/// to the picture.
pub fn crop(image: &egui::ColorImage, rect: egui::Rect, pixels_per_point: f32) -> egui::ColorImage {
    let [width, height] = image.size;
    let x0 = ((rect.min.x * pixels_per_point).round().max(0.0) as usize).min(width);
    let y0 = ((rect.min.y * pixels_per_point).round().max(0.0) as usize).min(height);
    let x1 = ((rect.max.x * pixels_per_point).round().max(0.0) as usize).clamp(x0, width);
    let y1 = ((rect.max.y * pixels_per_point).round().max(0.0) as usize).clamp(y0, height);
    if x1 == x0 || y1 == y0 {
        return egui::ColorImage::new([1, 1], vec![egui::Color32::BLACK]);
    }
    image.region_by_pixels([x0, y0], [x1 - x0, y1 - y0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_saved_page_can_be_viewed() {
        let mut capture = capture();
        capture.view(Some(0));
        assert_eq!(capture.viewing, None, "nothing is saved while measuring");
        capture.phase = Phase::Done {
            paths: vec![PathBuf::from("1.png"), PathBuf::from("2.png")],
            thumbnails: Vec::new(),
        };
        capture.view(Some(1));
        assert_eq!(capture.viewing, Some(1));
        capture.view(Some(2));
        assert_eq!(capture.viewing, Some(1), "past the last page stays put");
        capture.view(None);
        assert_eq!(capture.viewing, None);
    }

    fn rows(heights: &[f32], gap: f32) -> Vec<Span> {
        let mut top = 0.0;
        heights
            .iter()
            .map(|height| {
                let row = Span {
                    top,
                    bottom: top + height,
                };
                top += height + gap;
                row
            })
            .collect()
    }

    #[test]
    fn nothing_to_lay_out_makes_no_pages() {
        assert!(paginate(&[], 100.0, 10.0).is_empty());
    }

    #[test]
    fn rows_that_fit_make_one_page_as_tall_as_they_are() {
        let rows = rows(&[20.0, 30.0], 5.0);
        assert_eq!(
            paginate(&rows, 100.0, 10.0),
            [Span {
                top: 0.0,
                bottom: 55.0
            }]
        );
    }

    #[test]
    fn a_page_breaks_before_the_row_that_does_not_fit() {
        let rows = rows(&[40.0, 40.0, 40.0, 40.0], 5.0);
        let pages = paginate(&rows, 100.0, 10.0);
        assert_eq!(
            pages,
            [
                Span {
                    top: 0.0,
                    bottom: 90.0
                },
                Span {
                    top: 90.0,
                    bottom: 175.0
                },
            ]
        );
        // Every row lies whole on one page.
        for row in &rows {
            assert!(
                pages
                    .iter()
                    .any(|page| page.top <= row.top && row.bottom <= page.bottom)
            );
        }
        assert!(pages.iter().all(|page| page.height() <= 100.0));
    }

    #[test]
    fn a_row_taller_than_a_page_is_cut_with_an_overlap() {
        let rows = rows(&[30.0, 250.0, 30.0], 0.0);
        let pages = paginate(&rows, 100.0, 10.0);
        assert_eq!(
            pages,
            [
                Span {
                    top: 0.0,
                    bottom: 30.0
                },
                Span {
                    top: 30.0,
                    bottom: 130.0
                },
                Span {
                    top: 120.0,
                    bottom: 220.0
                },
                // The rest of the long row and the short one after it.
                Span {
                    top: 210.0,
                    bottom: 310.0
                },
            ]
        );
        assert!(pages.iter().all(|page| page.height() <= 100.0));
        // The pages cover the whole column.
        assert_eq!(pages.last().unwrap().bottom, 310.0);
    }

    #[test]
    fn a_huge_overlap_still_moves_on() {
        let rows = rows(&[1000.0], 0.0);
        let pages = paginate(&rows, 100.0, 500.0);
        assert!(pages.len() < 30);
        assert_eq!(pages.last().unwrap().bottom, 1000.0);
    }

    fn capture() -> Capture {
        Capture::new(
            AccountId("1".into()),
            "chat".into(),
            "a".into(),
            "b".into(),
            false,
        )
    }

    #[test]
    fn pages_are_cut_once_the_layout_holds_still() {
        let mut capture = capture();
        let layout = rows(&[50.0], 0.0);
        assert!(!capture.measured(rows(&[40.0], 0.0)));
        for _ in 0..STABLE_FRAMES - 1 {
            assert!(!capture.measured(layout.clone()));
        }
        assert!(!capture.measured(layout.clone()));
        assert!(capture.measured(layout));
        assert_eq!(capture.pages.len(), 1);
        assert_eq!(capture.current_page().map(|(index, _)| index), Some(0));
    }

    #[test]
    fn too_many_pages_are_cut_short() {
        let mut capture = capture();
        let layout = rows(&[700.0; MAX_PAGES + 5], 0.0);
        while !capture.measured(layout.clone()) {}
        assert_eq!(capture.pages.len(), MAX_PAGES);
        assert!(capture.truncated);
    }

    #[test]
    fn each_page_is_asked_for_once_drawn_and_its_answer_moves_on() {
        let mut capture = capture();
        let layout = rows(&[700.0, 700.0], 0.0);
        while !capture.measured(layout.clone()) {}
        let now = Instant::now();
        let mut shot = None;
        for _ in 0..PAGE_FRAMES {
            if let Due::Ask(asked) = capture.page_drawn(now) {
                shot = shot.or(Some(asked));
            }
        }
        let shot = shot.expect("a drawn page is asked for");
        assert_eq!(shot.page, 0);
        // Not asked again while the answer is on its way.
        assert_eq!(capture.page_drawn(now), Due::Wait);
        // Another capture's answer is not taken.
        let stray = Shot {
            generation: shot.generation + 1,
            page: 0,
        };
        assert!(!capture.shot(stray, egui::ColorImage::filled([1, 1], egui::Color32::RED)));
        assert!(capture.images.is_empty());
        assert!(!capture.shot(shot, egui::ColorImage::filled([1, 1], egui::Color32::RED)));
        assert_eq!(capture.current_page().map(|(index, _)| index), Some(1));
    }

    #[test]
    fn an_unanswered_page_fails_after_a_few_tries() {
        let mut capture = capture();
        while !capture.measured(rows(&[10.0], 0.0)) {}
        let mut now = Instant::now();
        let mut asked = 0;
        loop {
            match capture.page_drawn(now) {
                Due::Ask(_) => asked += 1,
                Due::Wait => {}
                Due::GiveUp => break,
            }
            now += SHOT_TIMEOUT;
        }
        assert_eq!(asked, SHOT_TRIES);
    }

    #[test]
    fn a_crop_stays_inside_the_picture() {
        let image = egui::ColorImage::filled([100, 50], egui::Color32::WHITE);
        let rect = egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(80.0, 80.0));
        assert_eq!(crop(&image, rect, 1.0).size, [70, 40]);
        assert_eq!(crop(&image, rect, 2.0).size, [80, 30]);
    }

    #[test]
    fn pages_are_written_as_numbered_pngs_replacing_older_ones() {
        let dir = tempfile::tempdir().unwrap();
        let folder = folder(dir.path());
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("screenshot-09.png"), b"old").unwrap();
        let images = vec![egui::ColorImage::filled([4, 6], egui::Color32::BLUE); 2];
        let paths = write_pages(&folder, &images).unwrap();
        assert_eq!(
            paths,
            [
                folder.join("screenshot-01.png"),
                folder.join("screenshot-02.png")
            ]
        );
        assert!(!folder.join("screenshot-09.png").exists());
        let page = image::open(&paths[0]).unwrap();
        assert_eq!((page.width(), page.height()), (4, 6));
    }
}
