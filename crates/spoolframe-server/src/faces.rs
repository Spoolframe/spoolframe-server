//! Faces in backdrops (PX-64, experimental `smart-crop`): where the heads
//! are, so an app can place a wide hero's crop by them.
//!
//! The model is YuNet (`face_detection_yunet_2023mar.onnx`, MIT, Shiqi Yu,
//! from OpenCV Zoo), carried as the NNEF that `scripts/yunet-to-nnef` makes
//! of it: `tract-nnef` runs that on the CPU, and nothing here parses ONNX.
//!
//! Each backdrop is scanned once, after its original is cached
//! ([`crate::artwork::ArtworkCache`]) or by [`sweep`] for the backlog, and
//! what was found is kept by artwork id: ids change with the image, so
//! nothing is ever scanned again. Never in the request path: a reply says
//! what is known, and an image not scanned yet has no answer until later.
//!
//! A paired server's backdrop (`fed:`) is scanned here, by this server,
//! when one of its users is first shown it; the origin may not have the
//! feature on. [`sweep`] leaves them out: it would pull every paired
//! server's backdrops across for images nobody may look at.

use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use image::DynamicImage;
use image::imageops::FilterType;
use parking_lot::Mutex;
use spoolframe_catalog::{Catalog, Page};
use spoolframe_protocol::ArtworkFace;
use spoolframe_types::{ArtworkId, ArtworkKind, Namespace};
use tokio::sync::{Semaphore, watch};
use tract_nnef::prelude::*;

use crate::artwork::{ArtworkCache, ArtworkError};
use crate::metrics::Counter;
use crate::store::Store;

const MODEL: &[u8] = include_bytes!("../assets/yunet-2023mar.nnef.tar");

/// The input the model is fixed to, width and height: NNEF's writer has no
/// symbolic size, and one plan for every image bounds a run's time and
/// memory. A 16:9 backdrop fills its width, 640 × 360 of it, as it did
/// when the crop rule was chosen; another shape is scaled to fit inside.
const CANVAS: (u32, u32) = (640, 384);
/// The lowest score of a face kept.
const MIN_SCORE: f32 = 0.6;
/// How much two boxes may overlap and both stay.
const MOST_OVERLAP: f32 = 0.3;
/// YuNet's feature maps, finest first: each cell is this many pixels.
const STRIDES: [usize; 3] = [8, 16, 32];

/// How often every library is walked for backdrops not scanned yet.
const SWEEP_EVERY: Duration = Duration::from_secs(6 * 3600);
/// How many items a walk asks a library for at a time.
const PAGE: u32 = 200;

#[derive(Debug, thiserror::Error)]
pub enum FaceError {
    #[error("the face model: {0}")]
    Model(String),
    #[error(transparent)]
    Image(#[from] ArtworkError),
}

fn model_error(e: impl std::fmt::Display) -> FaceError {
    FaceError::Model(e.to_string())
}

/// The model, ready to run. Costly to make, so make one;
/// [`Detector::detect`] takes `&self` from any thread.
pub struct Detector {
    plan: Arc<TypedRunnableModel>,
}

impl std::fmt::Debug for Detector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Detector").finish_non_exhaustive()
    }
}

impl Detector {
    pub fn new() -> Result<Self, FaceError> {
        let plan = tract_nnef::nnef()
            .model_for_read(&mut &*MODEL)
            .map_err(model_error)?
            .into_optimized()
            .map_err(model_error)?
            .into_runnable()
            .map_err(model_error)?;
        Ok(Self { plan })
    }

    /// [`Self::detect`], of the image in `bytes`.
    pub fn read(&self, bytes: &[u8]) -> Result<Vec<ArtworkFace>, FaceError> {
        self.detect(&crate::artwork::decode(bytes)?)
    }

    /// Every face in `image` scoring [`MIN_SCORE`] or more. Blocks for a
    /// few tens of milliseconds, on one core.
    pub fn detect(&self, image: &DynamicImage) -> Result<Vec<ArtworkFace>, FaceError> {
        let (width, height) = fitted(image.width(), image.height());
        let small = image::imageops::resize(&image.to_rgb8(), width, height, FilterType::Triangle);
        let (columns, rows) = (CANVAS.0 as usize, CANVAS.1 as usize);
        // Black past the image. Planes of blue, green and red, 0 to 255.
        let mut input = vec![0f32; 3 * columns * rows];
        for (x, y, pixel) in small.enumerate_pixels() {
            let at = y as usize * columns + x as usize;
            input[at] = f32::from(pixel[2]);
            input[columns * rows + at] = f32::from(pixel[1]);
            input[2 * columns * rows + at] = f32::from(pixel[0]);
        }
        let input = Tensor::from_shape(&[1, 3, rows, columns], &input).map_err(model_error)?;
        let outputs = self.plan.run(tvec!(input.into())).map_err(model_error)?;
        let maps = outputs
            .iter()
            .map(|t| {
                t.try_as_plain_ram()
                    .and_then(|t| t.as_slice::<f32>())
                    .map_err(model_error)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let maps: &[&[f32]; 12] = maps.as_slice().try_into().map_err(model_error)?;
        let boxes = decode(maps, columns);
        Ok(boxes
            .iter()
            .map(|b| b.as_face(width as f32, height as f32))
            .collect())
    }
}

/// Whether `id` looks like a backdrop by its path, for when only an id is
/// at hand: all of a sample of 944 Plex items' `art` have `/art/`, and no
/// `thumb`. A paired server's id, and another Plex server's, carry the
/// same path after a prefix. [`sweep`] goes by the catalog's kind instead.
pub fn is_backdrop(id: &ArtworkId) -> bool {
    id.key().contains("/art/")
}

/// Scans backdrops and answers what was found.
pub struct Faces {
    detector: Arc<Detector>,
    store: Store,
    /// The artwork cache's resizes: a scan is a decode and a resize too,
    /// and takes its turn with them.
    turns: Arc<Semaphore>,
    /// Being scanned now, so a second caller does not.
    scanning: Mutex<HashSet<ArtworkId>>,
    /// Scans run to the end, for the sweep's log and the tests.
    scanned: Counter,
}

impl std::fmt::Debug for Faces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Faces").finish_non_exhaustive()
    }
}

/// `id`'s place in [`Faces::scanning`], given back when dropped.
struct Claim<'a> {
    faces: &'a Faces,
    id: &'a ArtworkId,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.faces.scanning.lock().remove(self.id);
    }
}

impl Faces {
    /// The model made ready: slow enough, and failing only on a bad
    /// build, to do once at start.
    pub fn new(store: Store, turns: Arc<Semaphore>) -> Result<Self, FaceError> {
        Ok(Self {
            detector: Arc::new(Detector::new()?),
            store,
            turns,
            scanning: Mutex::default(),
            scanned: Counter::default(),
        })
    }

    /// The faces in backdrop `id`, none if it has none; `None` until it
    /// is scanned.
    pub async fn known(&self, id: &ArtworkId) -> Option<Vec<ArtworkFace>> {
        match self.store.artwork_faces(id).await {
            Ok(faces) => faces,
            Err(e) => {
                tracing::warn!(%id, error = %e, "cannot read a backdrop's faces");
                None
            }
        }
    }

    /// Scans run, each at most once per id.
    pub fn scanned(&self) -> u64 {
        self.scanned.get()
    }

    /// Scan the image `id`, in `bytes`, unless that was done or is being
    /// done, and keep what is found. An image that cannot be decoded is
    /// kept as having none: it would fail the same way every sweep. An
    /// image a store or model failure kept from being scanned is left for
    /// the next sweep.
    pub async fn scan(&self, id: &ArtworkId, bytes: Vec<u8>) {
        if !self.scanning.lock().insert(id.clone()) {
            return;
        }
        let _claim = Claim { faces: self, id };
        match self.store.artwork_faces(id).await {
            Ok(None) => {}
            Ok(Some(_)) => return,
            Err(e) => {
                tracing::warn!(%id, error = %e, "cannot read a backdrop's faces");
                return;
            }
        }
        let Ok(_turn) = self.turns.acquire().await else {
            return;
        };
        let detector = self.detector.clone();
        let found = match tokio::task::spawn_blocking(move || detector.read(&bytes)).await {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!(%id, error = %e, "a face scan did not finish");
                return;
            }
        };
        self.scanned.inc();
        let faces = match found {
            Ok(faces) => faces,
            Err(FaceError::Image(e)) => {
                tracing::debug!(%id, error = %e, "a backdrop that does not decode has no faces");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(%id, error = %e, "cannot scan a backdrop for faces");
                return;
            }
        };
        tracing::debug!(%id, faces = faces.len(), "backdrop scanned");
        if let Err(e) = self.store.keep_artwork_faces(id, &faces).await {
            tracing::warn!(%id, error = %e, "cannot keep a backdrop's faces");
        }
    }
}

/// How [`sweep`] goes about it.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    /// Before the first walk: the server has enough to do as it starts.
    pub after_start: Duration,
    /// Between one backdrop and the next. Two a second is about an eighth
    /// of a core while it lasts, an hour and a half for 10,000 items.
    pub between: Duration,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            after_start: Duration::from_secs(60),
            between: Duration::from_millis(500),
        }
    }
}

/// Every backdrop of every library, by the catalog's word on what is one:
/// the items at the top of a library, as a hero shows them. An episode's
/// is its show's. Not a paired server's (see the module's notes).
async fn backdrops(catalog: &dyn Catalog) -> Vec<ArtworkId> {
    let libraries = match catalog.libraries().await {
        Ok(libraries) => libraries,
        Err(e) => {
            tracing::debug!(error = %e, "no libraries to look for backdrops in");
            return Vec::new();
        }
    };
    let mut ids = HashSet::new();
    let mut in_order = Vec::new();
    for library in libraries {
        let mut offset = 0;
        loop {
            let page = Page {
                offset,
                limit: PAGE,
            };
            let page = match catalog.items(&library.id, page).await {
                Ok(page) => page,
                Err(e) => {
                    tracing::debug!(library = %library.id, error = %e, "a library's items are not listed for faces");
                    break;
                }
            };
            let got = page.items.len() as u32;
            for item in &page.items {
                if let Some(art) = item.artwork_of(ArtworkKind::Background)
                    && art.id.namespace() != Namespace::Federated
                    && ids.insert(art.id.clone())
                {
                    in_order.push(art.id.clone());
                }
            }
            offset += got;
            if got == 0 || offset >= page.total {
                break;
            }
        }
    }
    in_order
}

/// Scan the backlog, one backdrop at a time, while `artwork` exists: soon
/// after start, every [`SWEEP_EVERY`], and when the catalog changes, so a
/// new image is often ready before anyone is shown it.
pub async fn sweep(
    artwork: Weak<ArtworkCache>,
    pace: Pace,
    mut changes: Option<watch::Receiver<u64>>,
) {
    tokio::time::sleep(pace.after_start).await;
    loop {
        let Some(catalog) = artwork.upgrade().map(|a| a.catalog()) else {
            return;
        };
        let ids = backdrops(catalog.as_ref()).await;
        drop(catalog);
        let mut new = 0u64;
        for id in ids {
            let Some(cache) = artwork.upgrade() else {
                return;
            };
            if cache.scan_backlog(&id).await {
                new += 1;
                drop(cache);
                tokio::time::sleep(pace.between).await;
            }
        }
        if new > 0 {
            tracing::info!(backdrops = new, "backdrops scanned for faces");
        }
        let changed = async {
            match &mut changes {
                Some(changes) => {
                    if changes.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = tokio::time::sleep(SWEEP_EVERY) => {}
            () = changed => {}
        }
    }
}

/// What `width` × `height` is scaled to: as large as fits [`CANVAS`], up
/// as well as down, and never rounded to nothing.
fn fitted(width: u32, height: u32) -> (u32, u32) {
    let (wide, high) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let scale = (f64::from(CANVAS.0) / wide).min(f64::from(CANVAS.1) / high);
    (
        ((wide * scale).round() as u32).max(1),
        ((high * scale).round() as u32).max(1),
    )
}

/// One box, in pixels of what the model ran on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Found {
    score: f32,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    /// The mean height of the two eyes.
    eyes: f32,
}

impl Found {
    fn as_face(&self, width: f32, height: f32) -> ArtworkFace {
        ArtworkFace {
            left: self.x / width,
            right: (self.x + self.width) / width,
            top: self.y / height,
            bottom: (self.y + self.height) / height,
            eyes: self.eyes / height,
        }
    }

    /// How much of the two boxes is shared, of all they cover.
    fn overlap(&self, other: &Found) -> f32 {
        let wide = (self.x + self.width).min(other.x + other.width) - self.x.max(other.x);
        let high = (self.y + self.height).min(other.y + other.height) - self.y.max(other.y);
        let shared = wide.max(0.0) * high.max(0.0);
        let all = self.width * self.height + other.width * other.height - shared;
        if all > 0.0 { shared / all } else { 0.0 }
    }
}

/// The boxes in `maps`, the model's outputs in its order (class, object,
/// box and keypoints, each of [`STRIDES`]), for an input `columns` wide:
/// those scoring [`MIN_SCORE`] or more, best first, less any that overlap
/// a better one by more than [`MOST_OVERLAP`].
fn decode(maps: &[&[f32]; 12], columns: usize) -> Vec<Found> {
    let mut found = Vec::new();
    for (level, stride) in STRIDES.into_iter().enumerate() {
        let across = columns / stride;
        let (class, object) = (maps[level], maps[3 + level]);
        let (boxes, points) = (maps[6 + level], maps[9 + level]);
        let scale = stride as f32;
        for at in 0..class.len() {
            let score = (class[at].clamp(0.0, 1.0) * object[at].clamp(0.0, 1.0)).sqrt();
            if score < MIN_SCORE {
                continue;
            }
            let (row, column) = ((at / across) as f32, (at % across) as f32);
            let (width, height) = (
                boxes[4 * at + 2].exp() * scale,
                boxes[4 * at + 3].exp() * scale,
            );
            found.push(Found {
                score,
                x: (column + boxes[4 * at]) * scale - width / 2.0,
                y: (row + boxes[4 * at + 1]) * scale - height / 2.0,
                width,
                height,
                // Keypoints are x, y pairs: the right eye, the left, and
                // the nose and mouth corners, unused.
                eyes: (points[10 * at + 1] + points[10 * at + 3]) / 2.0 * scale + row * scale,
            });
        }
    }
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Found> = Vec::new();
    for candidate in found {
        if kept.iter().all(|k| k.overlap(&candidate) <= MOST_OVERLAP) {
            kept.push(candidate);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use async_trait::async_trait;
    use spoolframe_catalog::{CatalogError, Image, ItemPage, ItemWithChildren, SearchQuery};
    use spoolframe_types::{Artwork, Item, Library, LibraryId, MediaId, MediaKind, PersonId};

    use super::*;

    /// Maps of one cell each at stride 8 with `cells` boxes as (score, cx,
    /// cy, log w, log h), an eye height of `cy` + 1, and nothing at the
    /// other strides.
    fn decoded(columns: usize, cells: &[(f32, f32, f32, f32, f32)]) -> Vec<Found> {
        let class: Vec<f32> = cells.iter().map(|c| c.0).collect();
        let boxes: Vec<f32> = cells.iter().flat_map(|c| [c.1, c.2, c.3, c.4]).collect();
        let points: Vec<f32> = cells
            .iter()
            .flat_map(|c| [0.0, c.2 + 1.0, 0.0, c.2 + 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .collect();
        let none: &[f32] = &[];
        let maps = [
            class.as_slice(),
            none,
            none,
            class.as_slice(),
            none,
            none,
            boxes.as_slice(),
            none,
            none,
            points.as_slice(),
            none,
            none,
        ];
        decode(&maps, columns)
    }

    #[test]
    fn a_box_is_placed_by_its_cell_and_stride() {
        // The second cell of a 16 wide row at stride 8: x 8, y 0.
        let found = decoded(16, &[(0.0, 0.0, 0.0, 0.0, 0.0), (0.9, 1.0, 2.0, 1.0, 1.0)]);
        assert_eq!(found.len(), 1);
        let width = std::f32::consts::E * 8.0;
        assert!((found[0].score - 0.9).abs() < 1e-5);
        assert!((found[0].x - (16.0 - width / 2.0)).abs() < 1e-3);
        assert!((found[0].y - (16.0 - width / 2.0)).abs() < 1e-3);
        assert!((found[0].width - width).abs() < 1e-3);
        assert!((found[0].eyes - 24.0).abs() < 1e-3);
    }

    #[test]
    fn a_score_below_the_least_is_dropped_and_the_least_itself_kept() {
        assert!(decoded(16, &[(0.59, 0.0, 0.0, 0.0, 0.0)]).is_empty());
        assert_eq!(decoded(16, &[(0.6, 0.0, 0.0, 0.0, 0.0)]).len(), 1);
    }

    #[test]
    fn of_two_boxes_on_one_face_the_better_stays_and_far_ones_both_do() {
        let found = decoded(
            16,
            &[
                (0.7, 0.0, 0.0, 2.0, 2.0),
                (0.9, 0.0, 0.0, 2.0, 2.0),
                (0.8, 10.0, 0.0, 2.0, 2.0),
            ],
        );
        let scores: Vec<u32> = found
            .iter()
            .map(|f| (f.score * 10.0).round() as u32)
            .collect();
        assert_eq!(scores, [9, 8]);
    }

    #[test]
    fn fractions_are_of_the_image_not_the_padding() {
        let found = Found {
            score: 0.9,
            x: 64.0,
            y: 36.0,
            width: 64.0,
            height: 72.0,
            eyes: 72.0,
        };
        let face = found.as_face(640.0, 360.0);
        assert_eq!(
            face,
            ArtworkFace {
                left: 0.1,
                right: 0.2,
                top: 0.1,
                bottom: 0.3,
                eyes: 0.2
            }
        );
    }

    #[test]
    fn images_are_scaled_to_fit_the_canvas() {
        assert_eq!(fitted(3840, 2160), (640, 360));
        assert_eq!(fitted(320, 180), (640, 360));
        assert_eq!(fitted(1600, 1200), (512, 384));
        assert_eq!(fitted(8, 8192), (1, 384));
        assert_eq!(fitted(8192, 8), (640, 1));
    }

    /// Eileen Collins, the top of NASA's portrait, public domain, as
    /// `skimage.data.astronaut`.
    const PORTRAIT: &[u8] = include_bytes!("../tests/fixtures/astronaut-16x9.jpg");

    fn portrait() -> DynamicImage {
        image::load_from_memory(PORTRAIT).unwrap()
    }

    /// `image` with black added below it until it is `height` tall.
    fn taller(image: &DynamicImage, height: u32) -> DynamicImage {
        let mut out = image::RgbImage::new(image.width(), height);
        image::imageops::replace(&mut out, &image.to_rgb8(), 0, 0);
        out.into()
    }

    fn backdrop(n: u32) -> ArtworkId {
        ArtworkId::new(Namespace::Plex, format!("/library/metadata/{n}/art/1")).unwrap()
    }

    async fn faces(test: &str) -> (Faces, crate::store::tests::TestStore) {
        let store = crate::store::tests::test_store(test).await;
        let faces = Faces::new((*store).clone(), Arc::new(Semaphore::new(2))).unwrap();
        (faces, store)
    }

    #[tokio::test]
    async fn what_a_scan_finds_is_kept_and_nothing_is_scanned_twice() {
        let (faces, _store) = faces("faces-once").await;
        let id = backdrop(1);
        assert_eq!(faces.known(&id).await, None);
        tokio::join!(
            faces.scan(&id, PORTRAIT.to_vec()),
            faces.scan(&id, PORTRAIT.to_vec()),
        );
        faces.scan(&id, PORTRAIT.to_vec()).await;
        assert_eq!(faces.scanned(), 1);
        assert_eq!(faces.known(&id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn none_found_is_an_answer_not_a_reason_to_look_again() {
        let (faces, _store) = faces("faces-none").await;
        let id = backdrop(1);
        let mut blank = std::io::Cursor::new(Vec::new());
        DynamicImage::new_rgb8(320, 180)
            .write_to(&mut blank, image::ImageFormat::Png)
            .unwrap();
        faces.scan(&id, blank.into_inner()).await;
        faces.scan(&id, PORTRAIT.to_vec()).await;
        assert_eq!(faces.known(&id).await, Some(Vec::new()));
        assert_eq!(faces.scanned(), 1);
    }

    #[tokio::test]
    async fn an_image_that_does_not_decode_is_kept_as_having_none() {
        let (faces, _store) = faces("faces-bad").await;
        let id = backdrop(1);
        faces.scan(&id, b"<!doctype html>".to_vec()).await;
        assert_eq!(faces.known(&id).await, Some(Vec::new()));
    }

    #[tokio::test]
    async fn a_scan_waits_its_turn_with_the_resizes() {
        let (faces, _store) = faces("faces-turn").await;
        let held = faces.turns.acquire_many(2).await.unwrap();
        let id = backdrop(1);
        let scan = faces.scan(&id, PORTRAIT.to_vec());
        tokio::pin!(scan);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), scan.as_mut())
                .await
                .is_err()
        );
        drop(held);
        scan.await;
        assert_eq!(faces.scanned(), 1);
    }

    /// One library of `items` items, each with backdrop [`backdrop`] of
    /// its number, and a paired server's item after them.
    struct OneLibrary {
        items: u32,
        asked: Mutex<Vec<ArtworkId>>,
    }

    impl OneLibrary {
        fn item(n: u32, art: ArtworkId) -> Item {
            let mut item = crate::test_support::movie();
            if let Item::Movie(movie) = &mut item {
                movie.id = MediaId::new(Namespace::Plex, MediaKind::Movie, n.to_string()).unwrap();
                movie.artwork.push(Artwork {
                    id: art,
                    kind: ArtworkKind::Background,
                });
            }
            item
        }
    }

    #[async_trait]
    impl Catalog for OneLibrary {
        async fn libraries(&self) -> Result<Vec<Library>, CatalogError> {
            Ok(vec![crate::test_support::library()])
        }
        async fn items(&self, _: &LibraryId, page: Page) -> Result<ItemPage, CatalogError> {
            let foreign = ArtworkId::new(Namespace::Federated, "other/plex:/art/1").unwrap();
            let mut items: Vec<Item> = (0..self.items)
                .map(|n| Self::item(n, backdrop(n)))
                .collect();
            items.push(Self::item(self.items, foreign));
            let total = items.len() as u32;
            Ok(ItemPage {
                items: items
                    .into_iter()
                    .skip(page.offset as usize)
                    .take(page.limit as usize)
                    .collect(),
                total,
            })
        }
        async fn item(&self, _: &MediaId) -> Result<ItemWithChildren, CatalogError> {
            unreachable!()
        }
        async fn external_ids(&self, _: &MediaId) -> Result<Vec<String>, CatalogError> {
            unreachable!()
        }
        async fn search(&self, _: &SearchQuery) -> Result<spoolframe_catalog::Found, CatalogError> {
            unreachable!()
        }
        async fn person_items(&self, _: &PersonId) -> Result<Vec<Item>, CatalogError> {
            unreachable!()
        }
        async fn media_path(&self, _: &MediaId) -> Result<PathBuf, CatalogError> {
            unreachable!()
        }
        async fn recently_added(&self, _: u32) -> Result<Vec<Item>, CatalogError> {
            unreachable!()
        }
        async fn authorize_artwork(&self, _: &ArtworkId) -> Result<(), CatalogError> {
            Ok(())
        }
        async fn artwork(&self, id: &ArtworkId) -> Result<Image, CatalogError> {
            self.asked.lock().push(id.clone());
            Ok(Image {
                bytes: PORTRAIT.to_vec(),
                content_type: "image/jpeg".into(),
            })
        }
    }

    #[tokio::test]
    async fn a_walk_finds_every_page_s_backdrops_but_no_paired_server_s() {
        let catalog = OneLibrary {
            items: PAGE + 2,
            asked: Mutex::default(),
        };
        let found = backdrops(&catalog).await;
        assert_eq!(found.len(), PAGE as usize + 2);
        assert!(found.contains(&backdrop(PAGE + 1)));
        assert!(found.iter().all(|id| id.namespace() == Namespace::Plex));
    }

    #[tokio::test]
    async fn the_sweep_scans_the_backlog() {
        const ITEMS: u32 = 3;
        let catalog = Arc::new(OneLibrary {
            items: ITEMS,
            asked: Mutex::default(),
        });
        let tmp = spoolframe_test_dir::TestDir::new("spoolframe-faces-sweep").await;
        let store = crate::store::tests::test_store("faces-sweep").await;
        let tiers = crate::hybrid::Tiers {
            memory: 1 << 20,
            disk: 1 << 20,
        };
        let cache =
            crate::artwork::ArtworkCache::open(&tmp.as_ref().join("c"), tiers, catalog.clone())
                .await
                .unwrap()
                .with_faces((*store).clone())
                .unwrap();
        let cache = Arc::new(cache);
        let pace = Pace {
            after_start: Duration::ZERO,
            between: Duration::ZERO,
        };
        let sweep = tokio::spawn(sweep(Arc::downgrade(&cache), pace, None));
        let faces = cache.faces().unwrap();
        let last = backdrop(ITEMS - 1);
        // A scan counts before its faces are kept, so wait for both.
        for _ in 0..600 {
            if faces.scanned() == u64::from(ITEMS) && faces.known(&last).await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(faces.scanned(), u64::from(ITEMS));
        assert_eq!(faces.known(&last).await.unwrap().len(), 1);
        assert_eq!(catalog.asked.lock().len(), ITEMS as usize);
        sweep.abort();
    }

    #[test]
    fn a_backdrop_is_known_by_its_path_wherever_it_is_from() {
        let path = |key: &str| ArtworkId::new(Namespace::Plex, key).unwrap();
        assert!(is_backdrop(&path("/library/metadata/1/art/2")));
        assert!(is_backdrop(&path("nas/library/metadata/1/art/2")));
        assert!(!is_backdrop(&path("/library/metadata/1/thumb/2")));
    }

    #[test]
    fn a_portrait_has_one_face_where_the_head_is() {
        let faces = Detector::new().unwrap().detect(&portrait()).unwrap();
        assert_eq!(faces.len(), 1, "{faces:?}");
        let face = faces[0];
        // The photo's head is near (225, 110) of 512 × 288.
        assert!(
            (0.3..0.6).contains(&face.left) && face.left < face.right,
            "{face:?}"
        );
        assert!(
            (0.15..0.3).contains(&face.top) && face.bottom < 0.7,
            "{face:?}"
        );
        assert!(face.top < face.eyes && face.eyes < face.bottom, "{face:?}");
    }

    #[test]
    fn a_blank_image_has_no_faces() {
        let blank = DynamicImage::new_rgb8(1920, 1080);
        assert!(Detector::new().unwrap().detect(&blank).unwrap().is_empty());
    }

    /// Fractions are of the image, not of the black it is run with.
    #[test]
    fn a_taller_image_is_read_too_with_fractions_of_its_own_height() {
        let detector = Detector::new().unwrap();
        let wide = detector.detect(&portrait()).unwrap();
        let tall = detector.detect(&taller(&portrait(), 384)).unwrap();
        assert_eq!(tall.len(), 1, "{tall:?}");
        let eyes = tall[0].eyes * 384.0 / 288.0;
        assert!((eyes - wide[0].eyes).abs() < 0.02, "{tall:?} {wide:?}");
    }

    #[test]
    fn an_absurdly_tall_image_is_no_trouble() {
        let thin = DynamicImage::new_rgb8(8, 8192);
        assert!(Detector::new().unwrap().detect(&thin).unwrap().is_empty());
    }
}
