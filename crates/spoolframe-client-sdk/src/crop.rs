//! Where a wide hero crops a backdrop (experimental `smart-crop`, PX-64).
//!
//! A pure function, so that every app, given the same backdrop and hero
//! shape, shows the same crop. Vertical only: a hero nearer square than
//! the backdrop keeps its whole height, and where it loses its sides is
//! not decided here.

use spoolframe_protocol::ArtworkFace;

/// What a crop keeps of a backdrop, as fractions of its height: from
/// `top`, `height` of it. All of its width.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct BackdropCrop {
    pub top: f64,
    pub height: f64,
}

/// A backdrop and what the server knows of the faces in it
/// ([`crate::Session::backdrop`]).
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Backdrop {
    pub bytes: Vec<u8>,
    /// The faces, for [`backdrop_crop`], empty if it has none. `None`
    /// until the server has scanned it, on a server without
    /// [`crate::Experimental::smart_crop`], and for an image that is no
    /// backdrop: crop it with no faces, and do not keep that crop with
    /// the image, as a later fetch may know better.
    pub faces: Option<Vec<ArtworkFace>>,
}

/// The shape of a backdrop, which is all but always 16:9.
const BACKDROP_RATIO: f64 = 16.0 / 9.0;
/// How much hair is above a box, as a share of the box's height.
const HAIR: f64 = 0.35;
/// How much of a head may be lost, as a share of its box's height.
const SPARED: f64 = 0.1;
/// The places a crop is tried at.
const STEPS: u32 = 200;

/// The crop of a backdrop for a hero `hero_ratio` wide to its 1 high.
///
/// With no faces, the middle. With faces, it starts centred on the median
/// of their eyes, then moves the least that gives the fewest cut heads.
/// A hero no wider than the backdrop keeps all of its height.
pub fn backdrop_crop(faces: &[ArtworkFace], hero_ratio: f64) -> BackdropCrop {
    let height = BACKDROP_RATIO / hero_ratio;
    if !(height.is_finite() && height > 0.0 && height < 1.0) {
        return BackdropCrop {
            top: 0.0,
            height: 1.0,
        };
    }
    let slack = 1.0 - height;
    let faces: Vec<ArtworkFace> = faces.iter().copied().filter(is_sound).collect();
    let Some(line) = eye_line(&faces) else {
        return BackdropCrop {
            top: slack / 2.0,
            height,
        };
    };
    let start = (line - height / 2.0).clamp(0.0, slack);
    let mut best = BackdropCrop { top: start, height };
    let (mut fewest, mut nearest) = (usize::MAX, f64::INFINITY);
    for step in 0..=STEPS {
        let top = slack * f64::from(step) / f64::from(STEPS);
        let crop = BackdropCrop { top, height };
        let cut = cut_heads(&faces, crop);
        let away = (top - start).abs();
        if cut < fewest || (cut == fewest && away < nearest) {
            (best, fewest, nearest) = (crop, cut, away);
        }
    }
    best
}

/// Whether every number in `face` is one: what a server sends is not
/// trusted to be.
fn is_sound(face: &ArtworkFace) -> bool {
    [face.top, face.bottom, face.eyes]
        .iter()
        .all(|n| n.is_finite())
}

/// The median of `faces`' eyes, unweighted; the mean of the middle two of
/// an even number.
fn eye_line(faces: &[ArtworkFace]) -> Option<f64> {
    let mut eyes: Vec<f64> = faces.iter().map(|f| f64::from(f.eyes)).collect();
    eyes.sort_by(f64::total_cmp);
    let middle = eyes.len() / 2;
    match eyes.len() {
        0 => None,
        n if n % 2 == 1 => Some(eyes[middle]),
        _ => Some((eyes[middle - 1] + eyes[middle]) / 2.0),
    }
}

/// How many of `faces` lose their head to `crop`: over the top, hair
/// counted but stopping at the image's edge, or under the bottom.
fn cut_heads(faces: &[ArtworkFace], crop: BackdropCrop) -> usize {
    let is_cut = |face: &&ArtworkFace| {
        let (top, bottom) = (f64::from(face.top), f64::from(face.bottom));
        let spared = SPARED * (bottom - top);
        let hair = (top - HAIR * (bottom - top)).max(0.0);
        hair < crop.top - spared || bottom > crop.top + crop.height + spared
    };
    faces.iter().filter(is_cut).count()
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    fn face(top: f32, bottom: f32, eyes: f32) -> ArtworkFace {
        ArtworkFace {
            left: 0.4,
            right: 0.6,
            top,
            bottom,
            eyes,
        }
    }

    #[derive(Deserialize)]
    struct Sample {
        animation: bool,
        faces: Vec<[f32; 5]>,
        /// The reference's crop by hero ratio.
        top: std::collections::BTreeMap<String, f64>,
    }

    impl Sample {
        fn faces(&self) -> Vec<ArtworkFace> {
            self.faces
                .iter()
                .map(|&[left, right, top, bottom, eyes]| ArtworkFace {
                    left,
                    right,
                    top,
                    bottom,
                    eyes,
                })
                .collect()
        }
    }

    /// The 201 backdrops the rule was chosen on, as the test's page had
    /// them, with no names, and what that page's script made of each.
    fn samples() -> Vec<Sample> {
        serde_json::from_str(include_str!("../tests/fixtures/backdrops-201.json")).unwrap()
    }

    /// Of the images in `kind` with a face, how many lose a head at 3:1.
    fn cut(animation: bool, crop: impl Fn(&[ArtworkFace]) -> BackdropCrop) -> usize {
        samples()
            .iter()
            .filter(|s| s.animation == animation && !s.faces.is_empty())
            .filter(|s| cut_heads(&s.faces(), crop(&s.faces())) > 0)
            .count()
    }

    #[test]
    fn the_sample_loses_as_few_heads_as_the_page_says() {
        assert_eq!(cut(false, |f| backdrop_crop(f, 3.0)), 37);
        assert_eq!(cut(true, |f| backdrop_crop(f, 3.0)), 4);
    }

    #[test]
    fn a_centre_crop_loses_what_the_page_says_it_does() {
        assert_eq!(cut(false, |_| backdrop_crop(&[], 3.0)), 113);
        assert_eq!(cut(true, |_| backdrop_crop(&[], 3.0)), 23);
    }

    #[test]
    fn every_crop_is_the_reference_s_at_every_hero_shape() {
        for sample in samples() {
            for (ratio, top) in &sample.top {
                let crop = backdrop_crop(&sample.faces(), ratio.parse().unwrap());
                assert!((crop.top - top).abs() < 1e-6, "{ratio}: {crop:?} {top}");
            }
        }
    }

    #[test]
    fn no_faces_is_the_middle() {
        let crop = backdrop_crop(&[], 16.0 / 3.0);
        assert!((crop.height - 1.0 / 3.0).abs() < 1e-9);
        assert!((crop.top - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_face_alone_is_centred_on_its_eyes_as_near_as_the_steps_go() {
        let crop = backdrop_crop(&[face(0.4, 0.5, 0.44)], 3.0);
        let height: f64 = 16.0 / 9.0 / 3.0;
        assert!(
            (crop.top - (0.44 - height / 2.0)).abs() <= (1.0 - height) / 200.0,
            "{crop:?}"
        );
    }

    #[test]
    fn the_line_is_the_true_median_of_the_faces_eyes() {
        let at = |eyes: &[f32]| {
            let faces: Vec<ArtworkFace> =
                eyes.iter().map(|&e| face(e - 0.01, e + 0.01, e)).collect();
            eye_line(&faces).unwrap()
        };
        assert!((at(&[0.2, 0.9, 0.3]) - 0.3).abs() < 1e-6);
        assert!(
            (at(&[0.2, 0.4, 0.5, 0.9]) - 0.45).abs() < 1e-6,
            "the middle two's mean"
        );
    }

    /// Centred on the two low faces' eyes, the high face's head is cut;
    /// the crop goes up as far as it must to keep it, and no further.
    #[test]
    fn the_crop_moves_off_the_line_the_least_that_saves_the_most_heads() {
        let faces = [
            face(0.5, 0.6, 0.54),
            face(0.5, 0.6, 0.55),
            face(0.05, 0.15, 0.1),
        ];
        let height: f64 = 16.0 / 27.0;
        let start = BackdropCrop {
            top: 0.54 - height / 2.0,
            height,
        };
        assert_eq!(cut_heads(&faces, start), 1);
        let moved = backdrop_crop(&faces, 3.0);
        assert_eq!(cut_heads(&faces, moved), 0, "{moved:?}");
        // The high head's hair starts at 0.015, and a tenth of its box may
        // be lost: 0.025 is as low as the crop can start.
        let step = (1.0 - height) / 200.0;
        assert!(moved.top <= 0.025 && moved.top > 0.025 - step, "{moved:?}");
    }

    #[test]
    fn when_no_crop_saves_them_all_it_saves_the_most() {
        let faces = [
            face(0.0, 0.1, 0.04),
            face(0.02, 0.12, 0.06),
            face(0.85, 0.95, 0.9),
        ];
        let crop = backdrop_crop(&faces, 3.0);
        assert_eq!(cut_heads(&faces, crop), 1, "{crop:?}");
        assert_eq!(crop.top, 0.0, "{crop:?}");
    }

    #[test]
    fn hair_above_a_box_counts_and_stops_at_the_image_s_edge() {
        let crop = BackdropCrop {
            top: 0.2,
            height: 0.5,
        };
        // A box 0.1 high: the crop may start 0.01 into its hair and more.
        assert_eq!(cut_heads(&[face(0.25, 0.35, 0.3)], crop), 0);
        assert_eq!(cut_heads(&[face(0.23, 0.33, 0.28)], crop), 0);
        assert_eq!(cut_heads(&[face(0.22, 0.32, 0.27)], crop), 1);
        // At the top of the image there is no hair to lose past the edge.
        let top = BackdropCrop {
            top: 0.0,
            height: 0.5,
        };
        assert_eq!(cut_heads(&[face(0.01, 0.11, 0.05)], top), 0);
    }

    #[test]
    fn a_chin_may_go_a_tenth_of_its_box_past_the_bottom() {
        let crop = BackdropCrop {
            top: 0.0,
            height: 0.5,
        };
        assert_eq!(cut_heads(&[face(0.40, 0.505, 0.45)], crop), 0);
        assert_eq!(cut_heads(&[face(0.40, 0.52, 0.45)], crop), 1);
    }

    #[test]
    fn a_hero_no_wider_than_the_backdrop_keeps_all_of_it() {
        for ratio in [16.0 / 9.0, 1.5, 1.0] {
            let crop = backdrop_crop(&[face(0.1, 0.2, 0.15)], ratio);
            assert_eq!(
                crop,
                BackdropCrop {
                    top: 0.0,
                    height: 1.0
                }
            );
        }
    }

    #[test]
    fn nonsense_cannot_make_a_crop_of_nothing() {
        let whole = BackdropCrop {
            top: 0.0,
            height: 1.0,
        };
        for ratio in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(backdrop_crop(&[], ratio), whole, "{ratio}");
        }
        // A server's NaN is no face.
        assert_eq!(
            backdrop_crop(&[face(f32::NAN, 0.2, 0.15)], 3.0),
            backdrop_crop(&[], 3.0)
        );
    }
}
